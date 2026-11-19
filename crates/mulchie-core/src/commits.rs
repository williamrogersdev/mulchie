//! Commit attribution: which agent session, if any, was working in a
//! workspace when its `HEAD` moved.
//!
//! The engine that holds the lock reads `HEAD` (branch and commit, one
//! `git rev-parse` with a timeout) of each workspace an agent session works
//! in or recently worked in, every `perf.head_check_secs`, and right after an
//! agent's hook reports a shell command that moves `HEAD` (`git commit`,
//! `merge`, `rebase`, `cherry-pick`, `pull` or `am`). Each move is one line
//! in `<config dir>/commits.jsonl` ([`crate::jsonl`]: mode 0600, never through
//! a symbolic link, renamed to `commits.jsonl.1` at 2 MB).
//!
//! An entry is a move of `HEAD`, not a commit: one `git pull` that brings in
//! twenty commits is one entry, and a commit made and amended between two
//! checks is never seen. The session is the one the hook named, else the
//! running (or recently active) session working in that workspace that did
//! something last: a heuristic, not proof of authorship. The first entry for
//! a workspace (Mulchie started watching it) has no `previous_head` and no
//! session.
//!
//! It is a record for people and for reports such as `build_slower`; nothing
//! acts on it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::jsonl::JsonlFile;
use crate::paths::Paths;

/// The file is renamed to `commits.jsonl.1` past this size.
pub const ROTATE_BYTES: u64 = 2 * 1024 * 1024;
/// Entries the engine keeps in memory (the newest).
pub const KEPT: usize = 10_000;
/// `git` subcommands that move `HEAD`.
pub const HEAD_VERBS: &[&str] = &["commit", "merge", "rebase", "cherry-pick", "pull", "am"];

/// One move of a workspace's `HEAD`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitEntry {
    pub at_unix: u64,
    /// The checkout's top directory.
    pub workspace: PathBuf,
    /// `None` when `HEAD` is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit `HEAD` points to now (40 hex digits).
    pub head: String,
    /// Where it pointed at the check before; `None` for the first entry of
    /// a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The session's agent (`claude`, `codex`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// What noticed it: `poll` (the periodic check), `hook` (an agent's
    /// hook reported a git command) or `timing` (a build timing came in).
    #[serde(default)]
    pub via: String,
}

pub fn file(paths: &Paths) -> JsonlFile {
    JsonlFile::at(paths.config_dir.join("commits.jsonl"), ROTATE_BYTES)
}

/// Whether two commit names are the same commit: equal, or one a prefix of
/// the other of at least 7 hex digits.
pub fn same_commit(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a == b || (a.len().min(b.len()) >= 7 && (a.starts_with(&b) || b.starts_with(&a)))
}

/// Whether the shell command `cmd` runs a `git` subcommand that moves
/// `HEAD` ([`HEAD_VERBS`]). Reads text only, so `git -C dir commit` and
/// `make && git pull` match and an alias does not.
pub fn moves_head(cmd: &str) -> bool {
    let words: Vec<&str> = cmd
        .split(|c: char| c.is_whitespace() || ";&|()`".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        let name = w.trim_matches(|c| c == '"' || c == '\'');
        if name.rsplit('/').next() != Some("git") {
            continue;
        }
        let mut j = i + 1;
        while let Some(a) = words.get(j) {
            if matches!(
                *a,
                "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                j += 2;
            } else if a.starts_with('-') {
                j += 1;
            } else {
                break;
            }
        }
        if words.get(j).is_some_and(|v| HEAD_VERBS.contains(v)) {
            return true;
        }
    }
    false
}

/// The newest `HEAD` recorded for each workspace.
pub fn latest_heads(entries: &[CommitEntry]) -> HashMap<PathBuf, String> {
    let mut m = HashMap::new();
    for e in entries {
        m.insert(e.workspace.clone(), e.head.clone());
    }
    m
}

/// The entry that first recorded `HEAD` at `sha` in `workspace`: who was
/// working there when it appeared. `None` when no recorded move ended at it.
pub fn attribute<'a>(
    entries: &'a [CommitEntry],
    workspace: &Path,
    sha: &str,
) -> Option<&'a CommitEntry> {
    entries
        .iter()
        .find(|e| e.workspace == workspace && same_commit(&e.head, sha))
}

/// The recorded moves of `HEAD` in `workspace` that led from `from` to
/// `to`, oldest first: walking back from the last move to `to`, every move
/// up to the one that left `from` (or, when `from` was never recorded, up to
/// the first entry of the workspace, at most [`KEPT`]). Empty when `from`
/// and `to` are the same commit or `to` was never recorded.
pub fn between(
    entries: &[CommitEntry],
    workspace: &Path,
    from: &str,
    to: &str,
) -> Vec<CommitEntry> {
    if same_commit(from, to) {
        return Vec::new();
    }
    let ws: Vec<&CommitEntry> = entries
        .iter()
        .filter(|e| e.workspace == workspace)
        .collect();
    let Some(end) = ws.iter().rposition(|e| same_commit(&e.head, to)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in ws[..=end].iter().rev() {
        if same_commit(&e.head, from) {
            break;
        }
        out.push((*e).clone());
        let left_from = e
            .previous_head
            .as_deref()
            .is_some_and(|p| same_commit(p, from));
        if left_from || e.previous_head.is_none() || out.len() >= KEPT {
            break;
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(at: u64, ws: &str, prev: Option<&str>, head: &str, sid: Option<&str>) -> CommitEntry {
        CommitEntry {
            at_unix: at,
            workspace: PathBuf::from(ws),
            branch: Some("main".into()),
            head: head.into(),
            previous_head: prev.map(String::from),
            session_id: sid.map(String::from),
            agent: sid.map(|_| "claude".into()),
            via: "poll".into(),
        }
    }

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    #[test]
    fn git_commands_that_move_head_are_recognised() {
        for c in [
            "git commit -m x",
            "git -C /r commit --amend",
            "cargo fmt && git commit -am 'x'",
            "/usr/bin/git pull --rebase",
            "git -c user.name=x merge main",
            "git rebase -i HEAD~3",
            "git cherry-pick abc",
            "git am < p.patch",
            "(cd r; git pull)",
        ] {
            assert!(moves_head(c), "{c}");
        }
        for c in [
            "git status",
            "git log --grep commit",
            "gitk commit",
            "git push",
            "",
        ] {
            assert!(!moves_head(c), "{c}");
        }
    }

    #[test]
    fn short_and_long_shas_match() {
        assert!(same_commit(&sha('a'), "aaaaaaa"));
        assert!(!same_commit(&sha('a'), "aaaaaa"));
        assert!(!same_commit(&sha('a'), &sha('b')));
        assert!(same_commit("ABCDEF1", "abcdef1234"));
    }

    #[test]
    fn attribute_finds_the_first_move_to_a_commit() {
        let (a, b, c) = (sha('a'), sha('b'), sha('c'));
        let v = vec![
            e(1, "/r", None, &a, None),
            e(2, "/r", Some(&a), &b, Some("s-1-1")),
            e(3, "/other", None, &b, Some("s-9-9")),
            e(4, "/r", Some(&b), &c, Some("s-2-2")),
        ];
        let got = attribute(&v, Path::new("/r"), &b[..10]).unwrap();
        assert_eq!(got.session_id.as_deref(), Some("s-1-1"));
        assert!(attribute(&v, Path::new("/r"), &sha('d')).is_none());
        assert!(attribute(&v, Path::new("/nope"), &a).is_none());
    }

    #[test]
    fn between_walks_back_to_the_move_that_left_from() {
        let (a, b, c, d) = (sha('a'), sha('b'), sha('c'), sha('d'));
        let v = vec![
            e(1, "/r", None, &a, None),
            e(2, "/r", Some(&a), &b, Some("s-1-1")),
            e(3, "/x", Some(&a), &c, Some("s-5-5")),
            e(4, "/r", Some(&b), &c, Some("s-2-2")),
            e(5, "/r", Some(&c), &d, Some("s-3-3")),
        ];
        let r = between(&v, Path::new("/r"), &a, &c);
        assert_eq!(r.iter().map(|e| e.at_unix).collect::<Vec<_>>(), vec![2, 4]);
        assert!(between(&v, Path::new("/r"), &c, &c).is_empty());
        assert!(between(&v, Path::new("/r"), &a, &sha('e')).is_empty());
        // `from` never recorded: back to the first entry of the workspace.
        let r = between(&v, Path::new("/r"), &sha('f'), &b);
        assert_eq!(r.iter().map(|e| e.at_unix).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn latest_heads_per_workspace() {
        let (a, b) = (sha('a'), sha('b'));
        let v = vec![e(1, "/r", None, &a, None), e(2, "/r", Some(&a), &b, None)];
        assert_eq!(latest_heads(&v)[Path::new("/r")], b);
    }
}
