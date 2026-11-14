//! Feedback from agents (and people) about Mulchie and the machine it
//! watches: a problem hit, advice that was wrong, a finding that was noise,
//! something missing. Each report is one JSON line in
//! `<config dir>/feedback.jsonl`, private to the user, with the state of the
//! machine at that moment where the engine knows it.
//!
//! The file never leaves the machine: Mulchie sends it nowhere. The user
//! reads it with `mulchie feedback list` and decides what to share.
//!
//! The store works without an engine, so a report is kept even when the
//! thing being reported is that the engine is down.

use crate::config::create_private_dir;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The file is renamed to `feedback.jsonl.1` when it would pass this size,
/// so feedback never holds more than twice this much disk.
pub const ROTATE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_SUMMARY_CHARS: usize = 500;
pub const MAX_DETAILS_CHARS: usize = 8000;
const MAX_FIELD_CHARS: usize = 200;

/// What a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Mulchie itself failed: an error, a crash, a tool that did not answer.
    Bug,
    /// Mulchie's advice, diagnosis or recommendation was wrong or useless.
    WrongAdvice,
    /// A finding or a guard rule fired on something ordinary.
    FalsePositive,
    /// Mulchie did not notice or did not act on a real problem.
    Missed,
    /// Mulchie got in the way: it blocked, slowed or stopped wanted work.
    Blocked,
    /// The machine or a tool was slow, whatever the cause.
    Slow,
    /// A problem in the environment that is not Mulchie's doing.
    Environment,
    /// Something Mulchie should do and does not.
    Idea,
    #[default]
    Other,
}

impl Category {
    pub const ALL: &'static [&'static str] = &[
        "bug",
        "wrong_advice",
        "false_positive",
        "missed",
        "blocked",
        "slow",
        "environment",
        "idea",
        "other",
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    #[default]
    Medium,
    High,
}

impl Severity {
    pub const ALL: &'static [&'static str] = &["low", "medium", "high"];
}

/// What the reporter sends.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// One or two sentences: what happened.
    pub summary: String,
    #[serde(default)]
    pub category: Category,
    #[serde(default)]
    pub severity: Severity,
    /// What was tried, what was expected, the error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// The Mulchie tool, command or route the report is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Who reports: the agent's name (`claude-code`, `codex`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The directory the reporter works in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How the report reached the engine when a Mulchie front end passes it
    /// on: `mcp` or `cli`. Anything else, or nothing, is stored as `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The machine when the report was taken, from the engine's last sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub health: String,
    pub cpu_pct: f32,
    pub load1: f64,
    pub cores: usize,
    pub mem_free_pct: f32,
    pub disk_free_gb: f64,
    pub sessions: usize,
}

/// One stored report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// `fb-<unix seconds>-<random>`.
    pub id: String,
    pub at_unix: u64,
    /// How it arrived: `mcp`, `cli` or `api`.
    pub source: String,
    pub category: Category,
    pub severity: Severity,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub mulchie_version: String,
    pub os: String,
    /// `None` when no engine was running to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Machine>,
}

/// Why a report was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Free text as stored: secrets hidden word by word, control characters
/// dropped, cut to `max` characters. Line breaks are kept.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        out.push_str(&crate::redact::redact_command_line(&line));
    }
    let out = out.trim();
    if out.chars().count() > max {
        let mut cut: String = out.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        out.to_string()
    }
}

fn clean_opt(v: Option<String>, max: usize) -> Option<String> {
    v.map(|s| clean(&s, max)).filter(|s| !s.is_empty())
}

impl Report {
    /// Turn a report into the entry to store. `source` is how it arrived.
    pub fn into_entry(
        self,
        source: &str,
        machine: Option<Machine>,
        at_unix: u64,
    ) -> Result<Entry, Invalid> {
        let summary = clean(&self.summary, MAX_SUMMARY_CHARS);
        if summary.is_empty() {
            return Err(Invalid(
                "summary is required: one or two sentences saying what happened".into(),
            ));
        }
        let rand = crate::paths::random_hex(4).unwrap_or_else(|_| "0000".into());
        Ok(Entry {
            id: format!("fb-{at_unix}-{rand}"),
            at_unix,
            source: source.to_string(),
            category: self.category,
            severity: self.severity,
            summary,
            details: clean_opt(self.details, MAX_DETAILS_CHARS),
            tool: clean_opt(self.tool, MAX_FIELD_CHARS),
            client: clean_opt(self.client, MAX_FIELD_CHARS),
            cwd: self
                .cwd
                .map(|c| c.chars().filter(|c| !c.is_control()).take(1024).collect())
                .filter(|c: &String| !c.is_empty()),
            mulchie_version: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            machine,
        })
    }
}

/// The feedback file. Appends are single `write` calls on a file opened for
/// appending, so several processes (the engine, a CLI, an MCP server with no
/// engine) can add to it at once.
pub struct Store {
    path: PathBuf,
    max_bytes: u64,
}

impl Store {
    pub fn new(paths: &Paths) -> Self {
        Self::at(paths.config_dir.join("feedback.jsonl"), ROTATE_BYTES)
    }

    pub fn at(path: PathBuf, max_bytes: u64) -> Self {
        Self { path, max_bytes }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotated(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_os_string();
        s.push(".1");
        PathBuf::from(s)
    }

    pub fn append(&self, e: &Entry) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
        line.push(b'\n');
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 > self.max_bytes {
            std::fs::rename(&self.path, self.rotated())?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)?;
        f.write_all(&line)
    }

    /// Every stored entry, oldest first. Lines that do not parse are skipped.
    pub fn all(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for p in [self.rotated(), self.path.clone()] {
            let Ok(f) = std::fs::File::open(&p) else {
                continue;
            };
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(e) = serde_json::from_str::<Entry>(&line) {
                    out.push(e);
                }
            }
        }
        out
    }

    /// The newest `limit` entries, newest first, and how many there are in all.
    pub fn recent(&self, limit: usize) -> (Vec<Entry>, usize) {
        let mut all = self.all();
        let total = all.len();
        all.reverse();
        all.truncate(limit);
        (all, total)
    }
}

/// Store a report taken without an engine (the CLI or an MCP server when
/// none runs): no machine context.
pub fn record_local(paths: &Paths, report: Report, source: &str) -> Result<Entry, String> {
    let e = report
        .into_entry(source, None, crate::monitor::now_unix())
        .map_err(|e| e.0)?;
    Store::new(paths)
        .append(&e)
        .map_err(|err| format!("could not save the feedback: {err}"))?;
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(summary: &str) -> Report {
        Report {
            summary: summary.into(),
            ..Report::default()
        }
    }

    #[test]
    fn a_report_is_stored_and_read_back_newest_first() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let a = record_local(&paths, report("diagnose said good at load 50"), "cli").unwrap();
        let b = record_local(
            &paths,
            Report {
                category: Category::FalsePositive,
                severity: Severity::High,
                details: Some("line one\nline two".into()),
                tool: Some("list_findings".into()),
                client: Some("claude-code".into()),
                cwd: Some("/work/repo".into()),
                ..report("simulator daemons flagged")
            },
            "mcp",
        )
        .unwrap();
        assert!(a.id.starts_with("fb-") && a.id != b.id);
        let (got, total) = Store::new(&paths).recent(10);
        assert_eq!(total, 2);
        assert_eq!(got[0].id, b.id);
        assert_eq!(got[0].category, Category::FalsePositive);
        assert_eq!(got[0].details.as_deref(), Some("line one\nline two"));
        assert_eq!(got[0].source, "mcp");
        assert_eq!(got[1].summary, "diagnose said good at load 50");
        assert!(got[1].machine.is_none());
        let (one, total) = Store::new(&paths).recent(1);
        assert_eq!((one.len(), total), (1, 2));
    }

    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        record_local(&paths, report("x"), "cli").unwrap();
        let mode = std::fs::metadata(Store::new(&paths).path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn an_empty_summary_is_refused_and_long_text_is_cut() {
        let e = report("  \n ").into_entry("cli", None, 1).unwrap_err();
        assert!(e.0.contains("summary"));
        let long = "word ".repeat(5000);
        let e = Report {
            details: Some(long.clone()),
            ..report(&long)
        }
        .into_entry("cli", None, 1)
        .unwrap();
        assert_eq!(e.summary.chars().count(), MAX_SUMMARY_CHARS);
        assert_eq!(e.details.unwrap().chars().count(), MAX_DETAILS_CHARS);
    }

    #[test]
    fn secrets_in_the_text_are_hidden() {
        let e = Report {
            details: Some(
                "ran curl -H 'Authorization: Bearer abcdef0123456789abcdef0123456789' x".into(),
            ),
            ..report("failed with --token ghp_0123456789abcdefghijABCDEFGHIJ012345")
        }
        .into_entry("cli", None, 1)
        .unwrap();
        assert!(!e.summary.contains("ghp_0123456789"), "{}", e.summary);
        assert!(
            !e.details
                .as_deref()
                .unwrap()
                .contains("abcdef0123456789abcdef"),
            "{:?}",
            e.details
        );
    }

    #[test]
    fn the_file_rotates_at_its_limit_and_both_halves_are_read() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::at(t.path().join("feedback.jsonl"), 600);
        for i in 0..6 {
            let e = report(&format!("report number {i}"))
                .into_entry("cli", None, i)
                .unwrap();
            s.append(&e).unwrap();
        }
        assert!(s.rotated().exists());
        let all = s.all();
        assert!(all.len() >= 2 && all.len() <= 6);
        assert_eq!(all.last().unwrap().summary, "report number 5");
        assert!(std::fs::metadata(s.path()).unwrap().len() <= 600);
    }

    #[test]
    fn an_unknown_field_or_category_is_refused() {
        assert!(serde_json::from_str::<Report>(r#"{"summary":"x","nope":1}"#).is_err());
        assert!(serde_json::from_str::<Report>(r#"{"summary":"x","category":"rant"}"#).is_err());
        let r: Report = serde_json::from_str(r#"{"summary":"x","category":"slow"}"#).unwrap();
        assert_eq!(r.category, Category::Slow);
        assert_eq!(r.severity, Severity::Medium);
    }
}
