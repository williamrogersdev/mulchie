//! Build and test timings, and the `build_slower` findings they raise.
//!
//! A caller that held a build slot reports, when it releases the slot, how
//! long its command took, its exit code, the command and the directory it ran
//! in (`POST /v1/release`; `mulchie run` does it for every command). The
//! engine that holds the lock adds the workspace, branch and `HEAD` (read
//! through [`crate::commits`]) and the agent session, and appends one line to
//! `<config dir>/timings.jsonl` ([`crate::jsonl`]: mode 0600, never through a
//! symbolic link, renamed to `timings.jsonl.1` at 4 MB). The command is
//! redacted before it is stored.
//!
//! Runs are grouped by workspace and the command as
//! [`crate::issues::normalize_command`] sees it. A group whose latest
//! successful runs (exit code 0) are each much slower than the median of the
//! ones before ([`crate::perf::slower`], thresholds in `perf`) becomes a
//! `build_slower` finding naming the baseline, the slow runs and the moves
//! of `HEAD` between the last normal run and the first slow one, with the
//! sessions that were working then. It only reports: nothing acts on a
//! timing or a finding.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::commits::{self, CommitEntry};
use crate::config::PerfConfig;
use crate::consumers::Severity;
use crate::jsonl::JsonlFile;
use crate::paths::Paths;
use crate::perf::{self, human_ms};
use crate::watch::{ApiRequest, Evidence, Finding, FindingCategory, FindingKind, Suggestion};

/// The file is renamed to `timings.jsonl.1` past this size.
pub const ROTATE_BYTES: u64 = 4 * 1024 * 1024;
/// Timings the engine keeps in memory (the newest).
pub const KEPT: usize = 10_000;
/// A command is stored up to this many characters, after redaction.
pub const MAX_COMMAND_CHARS: usize = 1024;
/// A reported duration longer than this (7 days) is refused.
pub const MAX_DURATION_MS: u64 = 7 * 86_400 * 1000;
/// The longest `cwd` taken.
pub const MAX_CWD_BYTES: usize = 4096;
/// Commits listed in a finding's evidence, at most (the newest).
pub const MAX_EVIDENCE_COMMITS: usize = 20;

/// What a caller reports when it releases its build slot: every field is
/// optional, and nothing is recorded without `duration_ms`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TimingReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

impl TimingReport {
    /// Whether there is anything to record.
    pub fn is_empty(&self) -> bool {
        self.duration_ms.is_none()
    }

    /// Why the report cannot be recorded, if it cannot.
    pub fn check(&self) -> Result<(), String> {
        match self.duration_ms {
            None => return Err("duration_ms is required to record a timing".into()),
            Some(d) if d > MAX_DURATION_MS => {
                return Err(format!("duration_ms must be at most {MAX_DURATION_MS}"))
            }
            Some(_) => {}
        }
        if let Some(c) = &self.cwd {
            if !c.is_absolute() || c.as_os_str().len() > MAX_CWD_BYTES {
                return Err(format!(
                    "cwd must be an absolute path of at most {MAX_CWD_BYTES} bytes"
                ));
            }
        }
        if self.command.as_ref().is_some_and(|c| c.len() > 64 * 1024) {
            return Err("command must be at most 64 KiB".into());
        }
        Ok(())
    }
}

/// One recorded run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Timing {
    pub at_unix: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The command line, redacted and cut to [`MAX_COMMAND_CHARS`].
    #[serde(default)]
    pub command: String,
    /// The command as runs are grouped: [`crate::issues::normalize_command`].
    #[serde(default)]
    pub key: String,
    /// The build slot's label.
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The checkout holding `cwd`, or `cwd` itself outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The workspace's `HEAD` when the run was reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl Timing {
    /// A successful run: exit code 0. A run reported without an exit code
    /// is not counted as either.
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0)
    }
}

pub fn file(paths: &Paths) -> JsonlFile {
    JsonlFile::at(paths.config_dir.join("timings.jsonl"), ROTATE_BYTES)
}

/// A command as stored: redacted, control characters dropped, cut.
pub fn clean_command(cmd: &str) -> String {
    let one: String = cmd
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let red = crate::redact::redact_command_line(one.trim());
    if red.chars().count() > MAX_COMMAND_CHARS {
        let mut s: String = red.chars().take(MAX_COMMAND_CHARS - 1).collect();
        s.push('…');
        s
    } else {
        red
    }
}

/// The evidence of a `build_slower` finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PerfEvidence {
    /// The command as runs are grouped.
    pub command: String,
    /// The median of the runs before the slow ones.
    pub baseline_ms: u64,
    pub baseline_runs: usize,
    /// The slow runs, oldest first.
    pub recent_ms: Vec<u64>,
    /// The median of `recent_ms` over `baseline_ms`.
    pub ratio: f64,
    /// `HEAD` at the last normal run and at the first slow run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_head: Option<String>,
    /// The recorded moves of `HEAD` between them, oldest first, with the
    /// sessions that were working then (at most [`MAX_EVIDENCE_COMMITS`],
    /// the newest).
    #[serde(default)]
    pub commits: Vec<CommitEntry>,
    /// More moves than listed.
    #[serde(default)]
    pub commits_truncated: bool,
}

/// `build_slower:<16 hex digits of the workspace and command>`.
pub fn finding_id(workspace: Option<&Path>, key: &str) -> String {
    let ws = workspace
        .map(|w| w.to_string_lossy().to_string())
        .unwrap_or_default();
    format!(
        "build_slower:{:016x}",
        crate::issues::fnv1a(format!("{ws}\u{1f}{key}").as_bytes())
    )
}

/// `/v1/timings?workspace=…&command=…`.
pub fn timings_route(workspace: Option<&Path>, key: &str) -> String {
    let mut q = Vec::new();
    if let Some(w) = workspace {
        q.push(format!(
            "workspace={}",
            crate::api::query_escape(&w.to_string_lossy())
        ));
    }
    if !key.is_empty() {
        q.push(format!("command={}", crate::api::query_escape(key)));
    }
    if q.is_empty() {
        "/v1/timings".into()
    } else {
        format!("/v1/timings?{}", q.join("&"))
    }
}

/// The `build_slower` findings for `timings` (oldest first), with commit
/// attribution from `commits`.
pub fn findings(
    timings: &[Timing],
    commit_log: &[CommitEntry],
    cfg: &PerfConfig,
    now: u64,
) -> Vec<Finding> {
    let t = cfg.thresholds();
    // Enough history for a full baseline and a long slow streak.
    let keep = (cfg.baseline_runs + cfg.slow_runs) * 4;
    let mut groups: HashMap<(Option<&Path>, &str), Vec<&Timing>> = HashMap::new();
    for r in timings.iter().filter(|r| r.ok() && !r.key.is_empty()) {
        groups
            .entry((r.workspace.as_deref(), r.key.as_str()))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for ((ws, key), runs) in groups {
        let runs = &runs[runs.len().saturating_sub(keep)..];
        let series: Vec<f64> = runs.iter().map(|r| r.duration_ms as f64).collect();
        let Some(s) = perf::slower(&series, &t) else {
            continue;
        };
        let (normal, slow) = (runs[s.last_normal], runs[s.first_slow]);
        let latest = runs[runs.len() - 1];
        let (from_head, to_head) = (normal.head.clone(), slow.head.clone());
        let mut moves = match (ws, &from_head, &to_head) {
            (Some(w), Some(a), Some(b)) => commits::between(commit_log, w, a, b),
            _ => Vec::new(),
        };
        let truncated = moves.len() > MAX_EVIDENCE_COMMITS;
        if truncated {
            moves.drain(..moves.len() - MAX_EVIDENCE_COMMITS);
        }
        let recent_ms: Vec<u64> = s.recent.iter().map(|x| *x as u64).collect();
        let base = s.baseline as u64;
        let now_ms = perf::median(&s.recent).unwrap_or(s.baseline) as u64;
        let place = ws
            .and_then(Path::file_name)
            .map(|n| format!(" in {}", n.to_string_lossy()))
            .unwrap_or_default();
        let title = format!(
            "`{key}`{place} is {:.1}× slower than usual ({} vs {})",
            s.ratio,
            human_ms(now_ms),
            human_ms(base)
        );
        let changed = match (&from_head, &to_head) {
            (Some(a), Some(b)) if commits::same_commit(a, b) => {
                "HEAD did not move between the last normal run and the first slow one, so the cause is outside the code: the build cache, dependencies, the toolchain or a busy machine.".to_string()
            }
            (Some(a), Some(b)) if moves.is_empty() => format!(
                "HEAD moved from {} to {} between the last normal run and the first slow one; Mulchie recorded none of the moves in between.",
                short(a),
                short(b)
            ),
            (Some(a), Some(b)) => {
                let who: Vec<String> = moves
                    .iter()
                    .filter_map(|m| m.agent.clone().or_else(|| m.session_id.clone()))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let by = if who.is_empty() {
                    String::new()
                } else {
                    format!(" while {} worked there", who.join(", "))
                };
                format!(
                    "HEAD moved {} time(s) from {} to {}{by}: see evidence.perf.commits.",
                    moves.len() + usize::from(truncated),
                    short(a),
                    short(b)
                )
            }
            _ => "The workspace's HEAD is not known for those runs.".to_string(),
        };
        let why = format!(
            "The last {} successful runs each took more than {}× the median of the {} before them ({}) and more than {}s longer. {changed} Mulchie only reports this; it changes nothing.",
            recent_ms.len(),
            cfg.slower_ratio,
            s.baseline_runs,
            human_ms(base),
            cfg.min_slowdown_secs
        );
        let severity = if s.ratio >= 2.0 {
            Severity::Medium
        } else {
            Severity::Low
        };
        let id = finding_id(ws, key);
        out.push(Finding {
            id,
            kind: FindingKind::BuildSlower,
            category: FindingCategory::Performance,
            severity,
            title,
            why,
            evidence: Evidence {
                cmd: Some(key.to_string()),
                cwd: latest.cwd.clone(),
                workspace: ws.map(Path::to_path_buf),
                detail: Some(format!(
                    "baseline {} over {} runs; slow runs {}",
                    human_ms(base),
                    s.baseline_runs,
                    recent_ms
                        .iter()
                        .map(|m| human_ms(*m))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                perf: Some(Box::new(PerfEvidence {
                    command: key.to_string(),
                    baseline_ms: base,
                    baseline_runs: s.baseline_runs,
                    recent_ms,
                    ratio: s.ratio,
                    from_head,
                    to_head,
                    commits: moves,
                    commits_truncated: truncated,
                })),
                ..Evidence::default()
            },
            suggested: vec![Suggestion::call(
                "Show this command's timings",
                ApiRequest::get(timings_route(ws, key)),
                true,
            )],
            first_seen_unix: slow.at_unix.min(now),
            last_seen_unix: now,
            dismissed: false,
            dismissed_until: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn short(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}

#[cfg(test)]
#[path = "timings_tests.rs"]
mod tests;
