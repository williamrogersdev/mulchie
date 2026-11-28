//! Whether a worktree's branch has landed on the repository's default branch.
//!
//! `git branch --merged` alone misses most merges agents make: repositories
//! that merge through squash merges or a merge queue never make the branch tip
//! an ancestor of the default branch. Three local checks, cheapest first; none
//! touches the network:
//!
//! 1. **ancestor**: the branch tip is an ancestor of the default branch, local
//!    (`main`) or remote-tracking (`origin/main`, from `origin/HEAD`).
//! 2. **upstream_gone**: the branch tracked a remote branch that the last fetch
//!    found deleted (`[gone]`), which hosts do when a pull request merges.
//! 3. **squash**: every file the branch changed since its merge base has the
//!    branch's content on the default branch: its changes landed in another
//!    commit.
//!
//! Each answer is cached by (repository, branch tip, default branch tip), so a
//! rescan or a recheck by the safety gate repeats no work.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::git::git;
use crate::paths::Paths;

/// How a branch was found merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergedBy {
    Ancestor,
    UpstreamGone,
    Squash,
}

impl MergedBy {
    pub fn describe(self) -> &'static str {
        match self {
            MergedBy::Ancestor => "merged",
            MergedBy::UpstreamGone => "its remote branch was deleted (usually after merging)",
            MergedBy::Squash => "its changes are on the default branch (squash merge)",
        }
    }
}

/// A merged worktree skips the idle window only after this long without a
/// file change or commit: a branch just made from the default branch is an
/// ancestor of it too, and the agent that made it may not have started yet.
pub const MERGED_GRACE_SECS: u64 = 6 * 3600;

/// Above this many changed files the squash check gives up (not merged).
const SQUASH_MAX_FILES: usize = 2000;

/// Per-repository facts read once per scan.
pub struct RepoMerge {
    repo: PathBuf,
    /// Default-branch refs to compare against, with their tips.
    bases: Vec<(String, String)>,
    /// Local branches whose tip is an ancestor of a base.
    ancestors: HashSet<String>,
    /// Local branches whose upstream is gone.
    gone: HashSet<String>,
}

type CacheKey = (PathBuf, String, String);
static SQUASH_CACHE: Mutex<Option<HashMap<CacheKey, bool>>> = Mutex::new(None);

impl RepoMerge {
    /// Read the default branch, ancestry and upstream state of `repo`.
    pub fn read(paths: &Paths, repo: &Path) -> Self {
        let mut names: Vec<String> = Vec::new();
        // origin/HEAD names the remote's default branch.
        if let Ok(s) = git(
            paths,
            repo,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        ) {
            let remote = s.trim().to_string();
            if let Some(local) = remote.strip_prefix("origin/") {
                names.push(local.to_string());
            }
            names.push(remote);
        }
        if names.is_empty() {
            for n in ["main", "master"] {
                names.push(n.to_string());
            }
        }
        // The main checkout's branch, as before (often the default branch).
        if let Ok(s) = git(paths, repo, &["symbolic-ref", "--short", "HEAD"]) {
            names.push(s.trim().to_string());
        }
        let mut bases: Vec<(String, String)> = Vec::new();
        for n in names {
            if n.is_empty() || bases.iter().any(|(b, _)| *b == n) {
                continue;
            }
            let full = if n.starts_with("origin/") {
                format!("refs/remotes/{n}")
            } else {
                format!("refs/heads/{n}")
            };
            if let Ok(oid) = git(paths, repo, &["rev-parse", "--verify", "--quiet", &full]) {
                bases.push((full, oid.trim().to_string()));
            }
        }
        let mut ancestors = HashSet::new();
        for (b, _) in &bases {
            if let Ok(s) = git(
                paths,
                repo,
                &["branch", "--merged", b, "--format=%(refname:short)"],
            ) {
                ancestors.extend(s.lines().map(|l| l.trim().to_string()));
            }
        }
        let gone = git(
            paths,
            repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)%00%(upstream:track)",
                "refs/heads",
            ],
        )
        .map(|s| parse_gone(&s))
        .unwrap_or_default();
        RepoMerge {
            repo: repo.to_path_buf(),
            bases,
            ancestors,
            gone,
        }
    }

    /// Whether `branch` (tip `head`) is merged, and how.
    pub fn check(&self, paths: &Paths, branch: &str, head: Option<&str>) -> Option<MergedBy> {
        if self.ancestors.contains(branch) {
            return Some(MergedBy::Ancestor);
        }
        if self.gone.contains(branch) {
            return Some(MergedBy::UpstreamGone);
        }
        let head = head?;
        // The remote-tracking base first: it is the one merges land on.
        let mut bases: Vec<&(String, String)> = self.bases.iter().collect();
        bases.sort_by_key(|(b, _)| !b.starts_with("refs/remotes/"));
        for (_, base_oid) in bases {
            let key = (self.repo.clone(), head.to_string(), base_oid.clone());
            let cached = SQUASH_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(HashMap::new)
                .get(&key)
                .copied();
            let hit = match cached {
                Some(v) => v,
                None => {
                    let v = squash_merged(paths, &self.repo, base_oid, head);
                    let mut c = SQUASH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                    let c = c.get_or_insert_with(HashMap::new);
                    if c.len() > 4096 {
                        c.clear();
                    }
                    c.insert(key, v);
                    v
                }
            };
            if hit {
                return Some(MergedBy::Squash);
            }
        }
        None
    }
}

/// Branches whose upstream is `[gone]`, from
/// `for-each-ref --format=%(refname:short)%00%(upstream:track)`.
fn parse_gone(s: &str) -> HashSet<String> {
    s.lines()
        .filter_map(|l| {
            let (name, track) = l.split_once('\0')?;
            (track.trim() == "[gone]").then(|| name.trim().to_string())
        })
        .collect()
}

/// Every path `head` changed since its merge base with `base` has the same
/// content (or is absent in both) at `base`.
fn squash_merged(paths: &Paths, repo: &Path, base: &str, head: &str) -> bool {
    let Ok(mb) = git(paths, repo, &["merge-base", base, head]) else {
        return false;
    };
    let mb = mb.trim();
    if mb.is_empty() || mb == head {
        return false;
    }
    let Ok(names) = git(
        paths,
        repo,
        &["diff", "--name-only", "-z", "--no-renames", mb, head],
    ) else {
        return false;
    };
    let files: Vec<&str> = names.split('\0').filter(|f| !f.is_empty()).collect();
    if files.is_empty() || files.len() > SQUASH_MAX_FILES {
        return false;
    }
    let tree = |rev: &str| -> Option<HashMap<String, String>> {
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", rev, "--"];
        args.extend(files.iter().copied());
        let out = git(paths, repo, &args).ok()?;
        Some(
            out.split('\0')
                .filter_map(|e| {
                    // "<mode> <type> <oid>\t<path>"
                    let (meta, path) = e.split_once('\t')?;
                    let oid = meta.split(' ').nth(2)?;
                    Some((path.to_string(), oid.to_string()))
                })
                .collect(),
        )
    };
    match (tree(head), tree(base)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(st.status.success(), "git {args:?}: {st:?}");
    }

    fn write(dir: &Path, f: &str, s: &str) {
        std::fs::write(dir.join(f), s).unwrap();
    }

    /// A repository on `main` with one commit, and a paths value under tmp.
    fn repo(tmp: &Path) -> (Paths, PathBuf) {
        let paths = Paths::under(tmp);
        let r = tmp.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        sh(&r, &["init", "-q", "-b", "main"]);
        write(&r, "a.txt", "one\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "init"]);
        (paths, r)
    }

    fn rev(dir: &Path, r: &str) -> String {
        let o = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", r])
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn squash_merge_is_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, r) = repo(tmp.path());
        sh(&r, &["checkout", "-qb", "feature"]);
        write(&r, "b.txt", "new\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "f1"]);
        write(&r, "a.txt", "two\n");
        sh(&r, &["commit", "-qam", "f2"]);
        sh(&r, &["checkout", "-q", "main"]);
        // Unrelated work lands on main first.
        write(&r, "c.txt", "other\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "other"]);
        let head = rev(&r, "feature");
        let before = RepoMerge::read(&paths, &r);
        assert_eq!(before.check(&paths, "feature", Some(&head)), None);
        sh(&r, &["merge", "-q", "--squash", "feature"]);
        sh(&r, &["commit", "-qm", "squashed"]);
        let m = RepoMerge::read(&paths, &r);
        // Not an ancestor, yet merged.
        assert!(!m.ancestors.contains("feature"));
        assert_eq!(
            m.check(&paths, "feature", Some(&head)),
            Some(MergedBy::Squash)
        );
    }

    #[test]
    fn partly_landed_branch_is_not_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, r) = repo(tmp.path());
        sh(&r, &["checkout", "-qb", "feature"]);
        write(&r, "b.txt", "new\n");
        write(&r, "a.txt", "two\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "f"]);
        let head = rev(&r, "feature");
        sh(&r, &["checkout", "-q", "main"]);
        write(&r, "b.txt", "new\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "only b"]);
        let m = RepoMerge::read(&paths, &r);
        assert_eq!(m.check(&paths, "feature", Some(&head)), None);
    }

    #[test]
    fn ancestor_of_remote_default_branch_is_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, r) = repo(tmp.path());
        // The main checkout sits on another branch; only origin/main has the work.
        sh(&r, &["checkout", "-qb", "feature"]);
        write(&r, "b.txt", "new\n");
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "f"]);
        let head = rev(&r, "feature");
        sh(&r, &["update-ref", "refs/remotes/origin/main", &head]);
        sh(
            &r,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        sh(&r, &["checkout", "-qb", "elsewhere", "main"]);
        let m = RepoMerge::read(&paths, &r);
        assert_eq!(
            m.check(&paths, "feature", Some(&head)),
            Some(MergedBy::Ancestor)
        );
    }

    #[test]
    fn gone_upstream_is_parsed() {
        let s = "a\0[gone]\nb\0[ahead 1]\nc\0\n";
        let g = parse_gone(s);
        assert!(g.contains("a"));
        assert_eq!(g.len(), 1);
    }
}
