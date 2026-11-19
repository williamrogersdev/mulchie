//! Issues: findings about the same problem, grouped across processes and
//! engine restarts.
//!
//! A finding names one cause while it lasts: a process finding's id holds the
//! pid and start time, so the same runaway `npm run dev` started again
//! tomorrow is a new finding. An issue groups those findings by a
//! fingerprint that leaves the pid out (the kind, the workspace and the
//! command with paths, numbers and ids stripped), and remembers how often
//! the problem appeared, when it was first and last seen, and what the user
//! decided about it:
//!
//! - `open`: seen, nothing decided.
//! - `resolved`: the user says it is fixed. When a finding for it appears
//!   again, the issue becomes `regressed` and the engine says so.
//! - `regressed`: it came back after being resolved.
//! - `ignored`: still counted, never reported again.
//!
//! Only performance and hygiene findings become issues (the `hygiene`,
//! `limit` and `performance` categories, without the unreadable paused
//! list). Security, posture and update findings keep their own lifecycle.
//!
//! The book is saved in `<config dir>/issues.json` (mode 0600; it names
//! workspaces and commands, already redacted by the findings) by the engine
//! that holds the lock. It is replaced whole with
//! [`crate::config::write_atomic`]; old issues are dropped from it, no file is
//! deleted. It is a record for people, never a reason to act: nothing
//! signals or deletes because of an issue.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::consumers::Severity;
use crate::paths::Paths;
use crate::watch::{Finding, FindingCategory, FindingKind};

/// At most this many issues are kept; the ones seen longest ago go first.
pub const MAX_ISSUES: usize = 500;
/// An issue not seen for this long is dropped.
pub const RETAIN_SECS: u64 = 90 * 86_400;
/// A saved book larger than this is ignored rather than read.
const MAX_BYTES: u64 = 8 << 20;
/// Bumped whenever the saved book's meaning changes; another version is
/// ignored and the book starts empty.
const FORMAT: u32 = 1;
/// While an issue is active, `last_seen_unix` is saved at least this often.
const SAVE_SEEN_SECS: u64 = 300;
/// Command words that make up a fingerprint, at most.
const CMD_WORDS: usize = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    #[default]
    Open,
    Resolved,
    Regressed,
    Ignored,
}

impl IssueStatus {
    /// Open or regressed: something nobody has decided about yet.
    pub fn unresolved(self) -> bool {
        matches!(self, Self::Open | Self::Regressed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    /// 16 hex digits of the fingerprint; stable across restarts and versions.
    pub id: String,
    pub kind: FindingKind,
    pub category: FindingCategory,
    /// The highest severity any of its findings had.
    pub severity: Severity,
    /// The title of the latest finding.
    pub title: String,
    /// What the problem is about, without pids: the normalized command, the
    /// agent and cap, or the memory consumer.
    pub culprit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub first_seen_unix: u64,
    pub last_seen_unix: u64,
    /// How many separate findings it was: a new process, a new breach, a
    /// new low-memory spell. One that lasts a day counts once.
    pub occurrences: u64,
    pub status: IssueStatus,
    /// When `status` last changed, by the user or by a regression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_unix: Option<u64>,
    /// How many times it came back after being resolved.
    #[serde(default)]
    pub regressions: u32,
    /// A finding for it is current now.
    pub active: bool,
    /// The latest finding's id (see `GET /v1/findings`).
    pub finding_id: String,
}

/// Unresolved issues, counted.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueCounts {
    pub open: usize,
    pub regressed: usize,
    /// Unresolved and current now.
    pub active: usize,
}

/// What one [`IssueBook::observe_changes`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    /// Issues seen for the first time.
    pub new: Vec<Issue>,
    /// Resolved issues with a new finding.
    pub regressed: Vec<Issue>,
}

/// Whether findings of `kind` become issues.
pub fn tracked(kind: FindingKind) -> bool {
    matches!(
        kind.category(),
        FindingCategory::Hygiene | FindingCategory::Limit | FindingCategory::Performance
    ) && kind != FindingKind::PausedListUnreadable
}

/// The issue id, culprit and workspace of finding `f`, or `None` when its
/// kind is not tracked.
pub fn fingerprint(f: &Finding) -> Option<(String, String, Option<PathBuf>)> {
    if !tracked(f.kind) {
        return None;
    }
    let code = f.kind.code();
    // The part of the finding id after `<kind>:`.
    let rest =
        f.id.strip_prefix(&code)
            .map(|r| r.trim_start_matches(':'))
            .unwrap_or(&f.id);
    let culprit = match f.kind {
        // `limit_exceeded:<session>:<cap>`: the agent and the cap, not the
        // session, which is new every run.
        FindingKind::LimitExceeded | FindingKind::SessionHog => {
            let what = rest.rsplit(':').next().unwrap_or_default();
            let agent = f.evidence.name.as_deref().unwrap_or("agent");
            format!("{agent} {what}")
        }
        // Already stable: `memory_low` or `memory_low:<consumer>`, and
        // `orphan_swarm:<program>`.
        FindingKind::MemoryLow => {
            if rest.is_empty() || rest == f.id {
                "machine".to_string()
            } else {
                rest.to_string()
            }
        }
        FindingKind::OrphanSwarm => rest.to_string(),
        // The service and the operation, as they are: a route's path is the
        // identity, and `normalize_command` would cut it to its last part.
        FindingKind::EndpointSlower | FindingKind::EndpointErrors => {
            match f.evidence.endpoint.as_deref() {
                Some(e) => format!("{} {}", e.service, e.operation),
                None => rest.to_string(),
            }
        }
        _ => {
            let cmd = f
                .evidence
                .cmd
                .as_deref()
                .map(normalize_command)
                .filter(|c| !c.is_empty());
            cmd.or_else(|| f.evidence.name.clone())
                .or_else(|| {
                    f.evidence
                        .exe
                        .as_deref()
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "unknown".to_string())
        }
    };
    let ws = f.evidence.workspace.clone();
    let key = format!(
        "{code}\u{1f}{}\u{1f}{culprit}",
        ws.as_deref()
            .map(|w| w.to_string_lossy())
            .unwrap_or_default()
    );
    Some((format!("{:016x}", fnv1a(key.as_bytes())), culprit, ws))
}

/// A command line as a fingerprint sees it: the first [`CMD_WORDS`] words,
/// paths cut to their last part, and numbers, versions, ports and long hex
/// ids replaced by `#`. `/usr/bin/node /repo/node_modules/.bin/next dev
/// --port=3001` becomes `node next dev --port=#`.
pub fn normalize_command(cmd: &str) -> String {
    cmd.split_whitespace()
        .take(CMD_WORDS)
        .map(|w| match w.split_once('=') {
            Some((k, v)) if k.starts_with('-') => format!("{k}={}", word(v)),
            _ => word(w),
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn word(w: &str) -> String {
    let w = if w.contains('/') {
        w.trim_end_matches('/').rsplit('/').next().unwrap_or(w)
    } else {
        w
    };
    let numeric = w.chars().any(|c| c.is_ascii_digit())
        && w.chars().all(|c| c.is_ascii_digit() || ".:-_".contains(c));
    let hex_id = w.len() >= 12
        && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && w.chars().any(|c| c.is_ascii_digit());
    if numeric || hex_id {
        "#".to_string()
    } else {
        w.to_string()
    }
}

/// 64-bit FNV-1a: small, and the same in every build (std's hasher is not).
pub(crate) fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h ^= u64::from(*x);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[derive(Serialize, Deserialize)]
struct Saved {
    format: u32,
    issues: Vec<Issue>,
    /// Finding id → issue id for findings current when it was saved, so a
    /// restart does not count a finding that is still there a second time.
    #[serde(default)]
    live: Vec<(String, String)>,
}

/// Every issue, and which current finding belongs to which.
#[derive(Debug, Default)]
pub struct IssueBook {
    issues: HashMap<String, Issue>,
    /// Finding id → issue id, for the findings current at the last observe.
    live: HashMap<String, String>,
    /// Something changed that is not saved yet.
    dirty: bool,
    saved_unix: u64,
}

impl IssueBook {
    pub fn file(paths: &Paths) -> PathBuf {
        paths.config_dir.join("issues.json")
    }

    /// The book the last engine saved, or an empty one when there is none,
    /// it is not a plain file, is too large or cannot be read. Issues last
    /// seen after `now` (the clock moved back) keep their times.
    pub fn load(paths: &Paths) -> Self {
        let path = Self::file(paths);
        let plain = std::fs::symlink_metadata(&path)
            .is_ok_and(|m| m.file_type().is_file() && m.len() <= MAX_BYTES);
        let saved: Option<Saved> = plain
            .then(|| std::fs::read(&path).ok())
            .flatten()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|s: &Saved| s.format == FORMAT);
        let Some(s) = saved else {
            return Self::default();
        };
        let issues: HashMap<String, Issue> =
            s.issues.into_iter().map(|i| (i.id.clone(), i)).collect();
        let live = s
            .live
            .into_iter()
            .filter(|(_, iid)| issues.contains_key(iid))
            .collect();
        Self {
            issues,
            live,
            dirty: false,
            saved_unix: 0,
        }
    }

    /// Write the book when something changed (or, while an issue is active,
    /// every few minutes so `last_seen_unix` survives a crash).
    pub fn save_if_needed(&mut self, paths: &Paths, now: u64) -> std::io::Result<bool> {
        let seen_due = now.saturating_sub(self.saved_unix) >= SAVE_SEEN_SECS
            && self.issues.values().any(|i| i.active);
        if !self.dirty && !seen_due {
            return Ok(false);
        }
        let mut issues: Vec<Issue> = self.issues.values().cloned().collect();
        issues.sort_by(|a, b| a.id.cmp(&b.id));
        let mut live: Vec<(String, String)> = self
            .live
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        live.sort();
        let data = serde_json::to_vec(&Saved {
            format: FORMAT,
            issues,
            live,
        })
        .map_err(std::io::Error::other)?;
        crate::config::create_private_dir(&paths.config_dir)?;
        crate::config::write_atomic(&Self::file(paths), &data, 0o600)?;
        self.dirty = false;
        self.saved_unix = now;
        Ok(true)
    }

    /// Fold the current findings in. A finding not current at the last call
    /// is a new occurrence of its issue (or a new issue). Returns the issues
    /// that regressed: resolved before, with a new finding now.
    pub fn observe(&mut self, findings: &[Finding], now: u64) -> Vec<Issue> {
        self.observe_changes(findings, now).regressed
    }

    /// [`IssueBook::observe`], also returning the issues seen for the first
    /// time (a fingerprint the book did not have), as they are after this
    /// call.
    pub fn observe_changes(&mut self, findings: &[Finding], now: u64) -> Observed {
        let mut regressed = Vec::new();
        let mut created: Vec<String> = Vec::new();
        let mut current: HashMap<String, String> = HashMap::new();
        for f in findings {
            let Some((id, culprit, ws)) = fingerprint(f) else {
                continue;
            };
            let new = self.live.get(&f.id) != Some(&id);
            if !self.issues.contains_key(&id) {
                created.push(id.clone());
            }
            let issue = self.issues.entry(id.clone()).or_insert_with(|| Issue {
                id: id.clone(),
                kind: f.kind,
                category: f.kind.category(),
                severity: f.severity,
                title: f.title.clone(),
                culprit,
                workspace: ws,
                first_seen_unix: f.first_seen_unix.min(now),
                last_seen_unix: now,
                occurrences: 0,
                status: IssueStatus::Open,
                status_unix: None,
                regressions: 0,
                active: true,
                finding_id: f.id.clone(),
            });
            if new {
                issue.occurrences += 1;
                if issue.status == IssueStatus::Resolved {
                    issue.status = IssueStatus::Regressed;
                    issue.status_unix = Some(now);
                    issue.regressions += 1;
                    regressed.push(issue.clone());
                }
                self.dirty = true;
            }
            if issue.title != f.title || issue.finding_id != f.id {
                issue.title = f.title.clone();
                issue.finding_id = f.id.clone();
                self.dirty = true;
            }
            issue.severity = issue.severity.max(f.severity);
            issue.last_seen_unix = now;
            current.insert(f.id.clone(), id);
        }
        let active: HashSet<&String> = current.values().collect();
        for i in self.issues.values_mut() {
            let a = active.contains(&i.id);
            if i.active != a {
                i.active = a;
                self.dirty = true;
            }
        }
        self.live = current;
        self.prune(now);
        let new = created
            .iter()
            .filter_map(|id| self.issues.get(id).cloned())
            .collect();
        Observed { new, regressed }
    }

    /// Drop issues not seen for [`RETAIN_SECS`], then the ones seen longest
    /// ago beyond [`MAX_ISSUES`]. Active issues are kept.
    fn prune(&mut self, now: u64) {
        let before = self.issues.len();
        self.issues
            .retain(|_, i| i.active || now.saturating_sub(i.last_seen_unix) < RETAIN_SECS);
        if self.issues.len() > MAX_ISSUES {
            let mut old: Vec<(u64, String)> = self
                .issues
                .values()
                .filter(|i| !i.active)
                .map(|i| (i.last_seen_unix, i.id.clone()))
                .collect();
            old.sort();
            let extra = self.issues.len() - MAX_ISSUES;
            for (_, id) in old.into_iter().take(extra) {
                self.issues.remove(&id);
            }
        }
        if self.issues.len() != before {
            self.dirty = true;
        }
    }

    /// Set issue `id` to `status` (resolved, ignored, or open again).
    /// `None` when there is no such issue. Resolving an active issue counts
    /// only findings that appear after now as a regression.
    pub fn set_status(&mut self, id: &str, status: IssueStatus, now: u64) -> Option<Issue> {
        let i = self.issues.get_mut(id)?;
        if i.status != status {
            i.status = status;
            i.status_unix = Some(now);
            self.dirty = true;
        }
        Some(i.clone())
    }

    pub fn get(&self, id: &str) -> Option<&Issue> {
        self.issues.get(id)
    }

    /// Issues with `status` (unresolved ones when `None`; every one with
    /// `all`), of `kind`: active first, then the most recently seen.
    pub fn list(
        &self,
        status: Option<IssueStatus>,
        all: bool,
        kind: Option<FindingKind>,
    ) -> Vec<Issue> {
        let mut v: Vec<Issue> = self
            .issues
            .values()
            .filter(|i| match status {
                Some(s) => i.status == s,
                None => all || i.status.unresolved(),
            })
            .filter(|i| kind.is_none_or(|k| i.kind == k))
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then(b.last_seen_unix.cmp(&a.last_seen_unix))
                .then(a.id.cmp(&b.id))
        });
        v
    }

    pub fn counts(&self) -> IssueCounts {
        let mut c = IssueCounts::default();
        for i in self.issues.values() {
            match i.status {
                IssueStatus::Open => c.open += 1,
                IssueStatus::Regressed => c.regressed += 1,
                _ => continue,
            }
            if i.active {
                c.active += 1;
            }
        }
        c
    }
}

#[cfg(test)]
#[path = "issues_tests.rs"]
mod tests;
