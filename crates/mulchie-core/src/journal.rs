//! Append-only record of what Mulchie did or refused to do:
//! `<config dir>/actions.jsonl`, one JSON object per line, rotated to
//! `actions.jsonl.1` at 5 MB.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::create_private_dir;
use crate::paths::Paths;
use crate::safety::{Backup, SkipReason};

pub const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// One entry in the activity log and the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Action {
    pub at_unix: u64,
    /// `reclaim`, `worktree`, `stop`, `pause`, `resume`, `throttle`, `alert`.
    pub kind: String,
    pub detail: String,
    pub bytes: u64,
    pub workspace: Option<String>,
    /// Who asked: `optimizer`, `api`, `cli`, `manual`, `mcp`.
    pub source: String,
    /// The path or pid acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `done` or `skipped` for actions that went through the safety gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SkipReason>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<Backup>,
}

impl Action {
    pub fn note(
        at_unix: u64,
        kind: &str,
        detail: String,
        bytes: u64,
        workspace: Option<String>,
        source: &str,
    ) -> Self {
        Self {
            at_unix,
            kind: kind.into(),
            detail,
            bytes,
            workspace,
            source: source.into(),
            target: None,
            status: None,
            reason: None,
            backups: Vec::new(),
        }
    }
}

pub struct Journal {
    path: PathBuf,
    max_bytes: u64,
    lock: Mutex<()>,
    /// The last append failure, cleared by the next successful append.
    last_error: Mutex<Option<String>>,
}

impl Journal {
    pub fn new(paths: &Paths) -> Self {
        Self::at(paths.journal_file(), ROTATE_BYTES)
    }

    pub fn at(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            lock: Mutex::new(()),
            last_error: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotated(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_os_string();
        s.push(".1");
        PathBuf::from(s)
    }

    /// Why the last append failed, if it did (and none has succeeded since).
    /// Failures matter most when the disk is full, exactly when automatic
    /// reclaim runs; callers keep acting and surface this instead.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Append one line, rotating first when the file would pass the limit.
    pub fn append(&self, a: &Action) -> std::io::Result<()> {
        let res = self.append_inner(a);
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = res.as_ref().err().map(|e| {
            format!(
                "could not write the action journal {}: {e}",
                self.path.display()
            )
        });
        res
    }

    fn append_inner(&self, a: &Action) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut line = serde_json::to_vec(a).map_err(std::io::Error::other)?;
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
            .open(&self.path)?;
        f.write_all(&line)
    }

    /// The last `n` entries, oldest first, from the rotated file and the current one.
    pub fn tail(&self, n: usize) -> Vec<Action> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<Action> = Vec::new();
        for p in [self.rotated(), self.path.clone()] {
            let Ok(f) = std::fs::File::open(&p) else {
                continue;
            };
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(a) = serde_json::from_str::<Action>(&line) {
                    out.push(a);
                }
            }
        }
        let skip = out.len().saturating_sub(n);
        out.split_off(skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(i: u64) -> Action {
        Action::note(i, "reclaim", format!("entry {i}"), i, None, "test")
    }

    #[test]
    fn appends_and_reads_tail_across_restart() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        let j = Journal::new(&p);
        for i in 0..5 {
            j.append(&act(i)).unwrap();
        }
        let j2 = Journal::new(&p);
        let tail = j2.tail(3);
        assert_eq!(
            tail.iter().map(|a| a.at_unix).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn rotates_and_keeps_one_old_file() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("cfg/actions.jsonl");
        let line_len = serde_json::to_vec(&act(10)).unwrap().len() as u64 + 1;
        let j = Journal::at(path.clone(), line_len * 3);
        for i in 10..20 {
            j.append(&act(i)).unwrap();
        }
        let cur = std::fs::read_to_string(&path).unwrap();
        let old = std::fs::read_to_string(t.path().join("cfg/actions.jsonl.1")).unwrap();
        assert!(cur.lines().count() <= 3 && old.lines().count() <= 3);
        assert!(!t.path().join("cfg/actions.jsonl.2").exists());
        let tail = j.tail(100);
        assert_eq!(tail.last().unwrap().at_unix, 19);
        assert!(tail.len() >= 4 && tail.len() <= 6);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn bad_lines_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        let j = Journal::new(&p);
        j.append(&act(1)).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(p.journal_file())
            .unwrap();
        f.write_all(b"not json\n").unwrap();
        j.append(&act(2)).unwrap();
        assert_eq!(j.tail(10).len(), 2);
    }
}
