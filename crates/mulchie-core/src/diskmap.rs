//! Disk map: package and toolchain caches, and every git worktree under the scan
//! roots. Read-only; removal goes through the safety gate.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::git::{self, git, run_with_timeout, GitError};
use crate::measure::{measure_tree, Background};
use crate::paths::Paths;
use crate::process::{is_tool, ProcessSource};
use crate::reclaim::{dir_size, SizeMemo, REMEASURE_SECS};
use crate::safety::{any_cwd_in, is_agent_worktree, SkipReason, KNOWN_CACHE_DIRS};

/// Repositories scanned at once: by a requested scan, and by the periodic
/// one, which runs in the background and must not be felt.
const REPO_WORKERS: usize = 8;
const QUIET_WORKERS: usize = 2;

/// A worktree changed (a file or a commit) within this long is measured on
/// every scan; an older one only when something about it moved.
pub const ACTIVE_SECS: u64 = 86_400;

/// What a scan measured in one worktree: the expensive part (a walk of
/// every file and a `git status`), kept so the next periodic scan can skip
/// it for a worktree that shows no sign of change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeMemo {
    pub at_unix: u64,
    pub head: Option<String>,
    /// Modification time of the worktree's folder and of its git index.
    pub mark: (u64, u64),
    pub in_use: bool,
    pub bytes: u64,
    pub dirty_files: usize,
    pub status_error: Option<String>,
    pub last_commit_unix: u64,
    /// Newest file change the walk saw.
    pub newest: u64,
    pub activity_unknown: bool,
}

impl WorktreeMemo {
    /// Whether the periodic scan may reuse this: measured within
    /// [`REMEASURE_SECS`], read without error, nothing running in it then or
    /// now, the same commit, folder and index untouched, and idle for
    /// [`ACTIVE_SECS`] when measured. The safety gate reads a worktree again
    /// itself before removing it, so a reused figure is never acted on.
    fn fresh(&self, now: u64, head: &Option<String>, mark: (u64, u64), in_use: bool) -> bool {
        !in_use
            && !self.in_use
            && self.head == *head
            && self.mark == mark
            && self.status_error.is_none()
            && !self.activity_unknown
            && now.saturating_sub(self.at_unix) < REMEASURE_SECS
            && self
                .at_unix
                .saturating_sub(self.newest.max(self.last_commit_unix))
                >= ACTIVE_SECS
    }
}

/// What the last scans measured. The engine keeps one between periodic
/// scans; a scan someone asked for starts from an empty one.
#[derive(Debug, Clone, Default)]
pub struct ScanMemo {
    pub worktrees: HashMap<PathBuf, WorktreeMemo>,
    pub caches: HashMap<PathBuf, SizeMemo>,
    pub stores: HashMap<PathBuf, SizeMemo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Category {
    pub key: String,
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub hint: String,
    pub prunable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub repo: PathBuf,
    pub repo_name: String,
    pub name: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub agent: bool,
    pub bytes: u64,
    pub last_activity_unix: u64,
    pub last_commit_unix: u64,
    pub idle_days: u64,
    pub dirty_files: usize,
    pub merged: bool,
    /// HEAD commit at scan time. Removal requests carry it as the expected HEAD.
    pub head: Option<String>,
    /// git marks the entry prunable (its directory is gone).
    pub missing: bool,
    pub locked: bool,
    pub in_use: bool,
    pub removable: bool,
    pub sweepable: bool,
    pub verdict: String,
    /// Why `git status` could not be read; while set, `dirty_files` is unknown
    /// (reported as 0) and the worktree is never `removable` or `sweepable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_error: Option<String>,
    /// The walk for the newest file ran out of budget: when it was last used
    /// is unknown, so it is not `sweepable`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub activity_unknown: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiskMap {
    pub at_unix: u64,
    pub took_ms: u64,
    pub categories: Vec<Category>,
    pub worktrees: Vec<Worktree>,
    pub worktree_bytes: u64,
    pub sweepable_bytes: u64,
}

struct CatDef {
    key: &'static str,
    name: &'static str,
    path: PathBuf,
    hint: &'static str,
    prune: Option<Prune>,
}

/// A package manager's own prune command, and the processes that must not be
/// running while it runs.
#[derive(Clone, Copy)]
struct Prune {
    bin: &'static str,
    args: &'static [&'static str],
    busy: &'static [&'static str],
}

const fn prune(
    bin: &'static str,
    args: &'static [&'static str],
    busy: &'static [&'static str],
) -> Option<Prune> {
    Some(Prune { bin, args, busy })
}

/// Longest a package-manager prune may run before its process group is killed.
pub const PRUNE_TIMEOUT: Duration = Duration::from_secs(600);

/// The cache categories for this platform.
fn categories_for(paths: &Paths) -> Vec<CatDef> {
    categories_for_os(paths, cfg!(target_os = "macos"))
}

/// The cache categories for macOS (`macos`) or Linux and other Unixes.
fn categories_for_os(paths: &Paths, macos: bool) -> Vec<CatDef> {
    let home = &paths.home;
    let lib = home.join("Library");
    let cache = home.join(".cache");
    let share = home.join(".local/share");
    let c = |key, name, path, hint, prune| CatDef {
        key,
        name,
        path,
        hint,
        prune,
    };
    // Shared by both platforms.
    let mut v = vec![
        c(
            "cargo_registry",
            "Cargo registry",
            home.join(".cargo/registry"),
            "downloaded crate sources; refetched on demand",
            None,
        ),
        c(
            "cargo_git",
            "Cargo git checkouts",
            home.join(".cargo/git"),
            "safe to delete; refetched on next build",
            None,
        ),
        c(
            "rustup",
            "Rust toolchains",
            home.join(".rustup/toolchains"),
            "rustup toolchain uninstall <old>",
            None,
        ),
        c(
            "npm",
            "npm cache",
            home.join(".npm"),
            "npm cache clean --force",
            prune(
                "npm",
                &["cache", "clean", "--force"],
                &["npm", "npx", "npm-cli"],
            ),
        ),
        c(
            "bun",
            "bun install cache",
            home.join(".bun/install/cache"),
            "bun pm cache rm",
            prune("bun", &["pm", "cache", "rm"], &["bun", "bunx"]),
        ),
        c(
            "uv",
            "uv cache",
            cache.join("uv"),
            "uv cache prune",
            prune("uv", &["cache", "prune"], &["uv", "uvx"]),
        ),
        c(
            "go",
            "Go module cache",
            home.join("go/pkg/mod"),
            "go clean -modcache",
            // `go build` runs compile, link and asm from the toolchain.
            prune(
                "go",
                &["clean", "-modcache"],
                &["go", "compile", "link", "asm", "cgo"],
            ),
        ),
        c(
            "hf",
            "Hugging Face models",
            cache.join("huggingface"),
            "large model weights",
            None,
        ),
        c(
            "ollama",
            "Ollama models",
            home.join(".ollama/models"),
            "ollama rm <model>",
            None,
        ),
        c(
            "claude_backups",
            "Worktree backups",
            home.join(".claude/worktree-backups"),
            "patches saved before worktree removal",
            None,
        ),
        c(
            "mulchie_backups",
            "Mulchie backups",
            paths.backups_dir.clone(),
            "patches and ignored-file archives saved before worktree removal",
            None,
        ),
    ];
    let pnpm = prune("pnpm", &["store", "prune"], &["pnpm", "pnpx"]);
    let yarn = prune("yarn", &["cache", "clean"], &["yarn"]);
    let pip = prune("pip3", &["cache", "purge"], &["pip", "pip3"]);
    let brew = prune("brew", &["cleanup", "--prune=7"], &["brew"]);
    if macos {
        v.extend([
            c(
                "pnpm",
                "pnpm store",
                lib.join("pnpm/store"),
                "pnpm store prune",
                pnpm,
            ),
            c(
                "yarn",
                "Yarn cache",
                lib.join("Caches/Yarn"),
                "yarn cache clean",
                yarn,
            ),
            c(
                "pip",
                "pip cache",
                lib.join("Caches/pip"),
                "pip cache purge",
                pip,
            ),
            c(
                "homebrew",
                "Homebrew downloads",
                lib.join("Caches/Homebrew"),
                "brew cleanup --prune=7",
                brew,
            ),
            c(
                "xcode_dd",
                "Xcode DerivedData",
                lib.join("Developer/Xcode/DerivedData"),
                "reclaimable as a build cache",
                None,
            ),
            c(
                "simulators",
                "iOS Simulators",
                lib.join("Developer/CoreSimulator"),
                "xcrun simctl delete unavailable",
                prune(
                    "xcrun",
                    &["simctl", "delete", "unavailable"],
                    &["simulator", "simctl", "launchd_sim"],
                ),
            ),
            c(
                "docker",
                "Docker disk image",
                lib.join("Containers/com.docker.docker"),
                "docker system prune",
                None,
            ),
            c(
                "lib_caches",
                "~/Library/Caches",
                lib.join("Caches"),
                "app caches; most apps rebuild them",
                None,
            ),
            c(
                "logs",
                "~/Library/Logs",
                lib.join("Logs"),
                "old logs are safe to delete",
                None,
            ),
            c(
                "trash",
                "Trash",
                home.join(".Trash"),
                "empty the Trash",
                None,
            ),
        ]);
    } else {
        v.extend([
            c(
                "pnpm",
                "pnpm store",
                share.join("pnpm/store"),
                "pnpm store prune",
                pnpm,
            ),
            c(
                "yarn",
                "Yarn cache",
                cache.join("yarn"),
                "yarn cache clean",
                yarn,
            ),
            c(
                "pip",
                "pip cache",
                cache.join("pip"),
                "pip cache purge",
                pip,
            ),
            c(
                "homebrew",
                "Homebrew downloads",
                cache.join("Homebrew"),
                "brew cleanup --prune=7",
                brew,
            ),
            c(
                "xdg_cache",
                "~/.cache",
                cache.clone(),
                "app caches; most apps rebuild them",
                None,
            ),
            c(
                "trash",
                "Trash",
                share.join("Trash"),
                "empty the Trash",
                None,
            ),
        ]);
    }
    v
}

/// Scan package caches and every git worktree under the scan roots. `busy_cwds`
/// are the working directories of the user's processes, read now.
pub fn scan(cfg: &Config, paths: &Paths, busy_cwds: &[PathBuf], now: u64) -> DiskMap {
    let mut memo = ScanMemo::default();
    scan_with(cfg, paths, busy_cwds, now, &mut memo, false)
}

/// [`scan`], reusing from `memo` what shows no sign of change and leaving
/// in it what this scan measured (`memo.caches` is the cache scan's and is
/// not touched). `quiet` is the periodic scan: fewer threads, at background
/// priority.
pub fn scan_with(
    cfg: &Config,
    paths: &Paths,
    busy_cwds: &[PathBuf],
    now: u64,
    memo: &mut ScanMemo,
    quiet: bool,
) -> DiskMap {
    let prev: &ScanMemo = memo;
    let start = std::time::Instant::now();
    let cats: Vec<CatDef> = categories_for(paths)
        .into_iter()
        .filter(|c| c.path.exists())
        .collect();
    let repos = find_repos(cfg, paths);
    // A bounded pool: at most REPO_WORKERS repositories at once.
    let queue = Mutex::new(repos.iter());

    let cat_queue = Mutex::new(cats.iter());
    let workers = if quiet { QUIET_WORKERS } else { REPO_WORKERS };
    let (cat_out, wt_out) = std::thread::scope(|s| {
        // A requested scan measures every store at once; the periodic one,
        // one at a time.
        let ch: Vec<_> = (0..if quiet { 1 } else { cats.len() })
            .map(|_| {
                let cat_queue = &cat_queue;
                s.spawn(move || {
                    let _bg = quiet.then(Background::enter);
                    let mut out = Vec::new();
                    loop {
                        let next = cat_queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                        let Some(c) = next else { break };
                        let mark = mtime(&c.path);
                        let known = prev.stores.get(&c.path).filter(|m| {
                            m.mark == mark && now.saturating_sub(m.at_unix) < REMEASURE_SECS
                        });
                        let size = match known {
                            Some(m) => m.clone(),
                            None => SizeMemo {
                                at_unix: now,
                                bytes: dir_size(&c.path),
                                mark,
                                in_use: false,
                            },
                        };
                        let cat = Category {
                            key: c.key.to_string(),
                            name: c.name.to_string(),
                            path: c.path.clone(),
                            bytes: size.bytes,
                            hint: c.hint.to_string(),
                            prunable: c.prune.is_some(),
                        };
                        out.push((cat, size));
                    }
                    out
                })
            })
            .collect();
        let wh: Vec<_> = (0..workers.min(repos.len()))
            .map(|_| {
                let queue = &queue;
                s.spawn(move || {
                    let _bg = quiet.then(Background::enter);
                    let mut out = Vec::new();
                    loop {
                        let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                        let Some(r) = next else { break };
                        out.extend(repo_worktrees(r, busy_cwds, cfg, paths, now, prev));
                    }
                    out
                })
            })
            .collect();
        let cats: Vec<(Category, SizeMemo)> = ch
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect();
        let wts: Vec<(Worktree, Option<WorktreeMemo>)> = wh
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect();
        (cats, wts)
    });
    memo.stores = cat_out
        .iter()
        .map(|(c, m)| (c.path.clone(), m.clone()))
        .collect();
    memo.worktrees = wt_out
        .iter()
        .filter_map(|(w, m)| Some((w.path.clone(), m.clone()?)))
        .collect();
    let mut categories: Vec<Category> = cat_out.into_iter().map(|(c, _)| c).collect();
    let mut worktrees: Vec<Worktree> = wt_out.into_iter().map(|(w, _)| w).collect();

    categories.retain(|c| c.bytes > 0);
    categories.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    worktrees.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let worktree_bytes = worktrees.iter().map(|w| w.bytes).sum();
    let sweepable_bytes = worktrees
        .iter()
        .filter(|w| w.sweepable)
        .map(|w| w.bytes)
        .sum();
    DiskMap {
        at_unix: now,
        took_ms: start.elapsed().as_millis() as u64,
        categories,
        worktrees,
        worktree_bytes,
        sweepable_bytes,
    }
}

fn find_repos(cfg: &Config, paths: &Paths) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in cfg.roots(paths) {
        if let Ok(root) = root.canonicalize() {
            walk_repos(&root, 0, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk_repos(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    if dir.join(".git").is_dir() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "node_modules" | "target" | "Library" | "dist" | "build"
            )
        {
            continue;
        }
        walk_repos(&e.path(), depth + 1, out);
    }
}

fn repo_worktrees(
    repo: &Path,
    busy_cwds: &[PathBuf],
    cfg: &Config,
    paths: &Paths,
    now: u64,
    prev: &ScanMemo,
) -> Vec<(Worktree, Option<WorktreeMemo>)> {
    let Ok(entries) = git::worktree_list(paths, repo) else {
        return Vec::new();
    };
    let main_branch = git(paths, repo, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());
    let merged: Vec<String> = main_branch
        .as_ref()
        .and_then(|b| {
            git(
                paths,
                repo,
                &["branch", "--merged", b, "--format=%(refname:short)"],
            )
            .ok()
        })
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();

    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    entries
        .into_iter()
        .skip(1)
        .filter(|e| !e.bare)
        .map(|e| {
            let path = e.path.clone();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let missing = e.prunable;
            let exists = !missing && path.is_dir();
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            let in_use = exists && any_cwd_in(busy_cwds, &canon, false);
            let agent = is_agent_worktree(&path);
            let measured = exists.then(|| {
                let mark = (mtime(&path), mtime(&git_index(repo, &path)));
                if let Some(m) = prev
                    .worktrees
                    .get(&path)
                    .filter(|m| m.fresh(now, &e.head, mark, in_use))
                {
                    return m.clone();
                }
                // One walk gives the size and the newest change.
                let walk = measure_tree(&path, ACTIVITY_BUDGET);
                let (dirty_files, status_error) = match git(
                    paths,
                    &path,
                    &[
                        "status",
                        "--porcelain=v1",
                        "--untracked-files=all",
                        "--ignore-submodules=all",
                    ],
                ) {
                    Ok(s) => (s.lines().filter(|l| !l.is_empty()).count(), None),
                    Err(e) => (0, Some(e.to_string())),
                };
                let last_commit_unix = git(paths, &path, &["log", "-1", "--format=%ct", "HEAD"])
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                WorktreeMemo {
                    at_unix: now,
                    head: e.head.clone(),
                    // Read again: `git status` may refresh the index.
                    mark: (mtime(&path), mtime(&git_index(repo, &path))),
                    in_use,
                    bytes: walk.bytes,
                    dirty_files,
                    status_error,
                    last_commit_unix,
                    newest: walk.newest,
                    activity_unknown: !walk.complete,
                }
            });
            let m = measured.as_ref();
            let bytes = m.map_or(0, |m| m.bytes);
            let dirty_files = m.map_or(0, |m| m.dirty_files);
            let status_error = m.and_then(|m| m.status_error.clone());
            let last_commit_unix = m.map_or(0, |m| m.last_commit_unix);
            let activity_unknown = m.is_some_and(|m| m.activity_unknown);
            let last_activity_unix = m.map_or(0, |m| m.newest.max(m.last_commit_unix));
            let idle_days = now.saturating_sub(last_activity_unix) / 86_400;
            let is_merged = e.branch.as_ref().map(|b| merged.contains(b)).unwrap_or(false);
            let locked = e.locked;
            let known = status_error.is_none();
            let removable = (missing && agent)
                || (exists && known && !in_use && !locked && dirty_files == 0);
            let sweepable = agent
                && !locked
                && (missing
                    || (exists
                        && known
                        && !activity_unknown
                        && !in_use
                        && idle_days >= cfg.worktree_idle_days));
            let verdict = if let (true, Some(err)) = (exists, status_error.as_deref()) {
                format!("could not read git status ({err}); left alone")
            } else if missing && !agent {
                "directory is gone; not an agent worktree, so it is left for you".to_string()
            } else if missing {
                "directory is gone; git marks the entry prunable".to_string()
            } else if !exists {
                "directory is missing but git does not mark it prunable".to_string()
            } else if in_use {
                "agent or process running here".to_string()
            } else if locked {
                "locked by git".to_string()
            } else if sweepable && dirty_files > 0 {
                format!("idle {idle_days}d with {dirty_files} uncommitted file(s); removal saves a patch first")
            } else if dirty_files > 0 {
                format!("{dirty_files} uncommitted file(s); review before removing")
            } else if is_merged {
                "clean and merged; safe to remove".to_string()
            } else if sweepable {
                format!("idle {idle_days}d; branch is kept on removal")
            } else if activity_unknown && agent {
                "too many files to tell when it was last used; not swept".to_string()
            } else {
                format!("clean; active {idle_days}d ago")
            };
            let wt = Worktree {
                name,
                path,
                repo: repo.to_path_buf(),
                repo_name: repo_name.clone(),
                branch: e.branch.clone(),
                detached: e.detached,
                agent,
                bytes,
                last_activity_unix,
                last_commit_unix,
                idle_days,
                dirty_files,
                merged: is_merged,
                head: e.head.clone(),
                missing,
                locked,
                in_use,
                removable,
                sweepable,
                verdict,
                status_error,
                activity_unknown,
            };
            (wt, measured)
        })
        .collect()
}

/// The index file of the worktree at `path` of `repo`: it changes when
/// something is staged, committed or checked out there.
fn git_index(repo: &Path, path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default();
    // A linked worktree's `.git` is a file naming its directory under the
    // repository's `.git/worktrees`; that directory is named after the
    // worktree's folder unless two share a name.
    let linked = repo.join(".git").join("worktrees").join(name);
    if linked.is_dir() {
        linked.join("index")
    } else {
        path.join(".git").join("index")
    }
}

fn mtime(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Entries [`last_activity_checked`] looks at before giving up.
pub const ACTIVITY_BUDGET: usize = 100_000;

/// Newest file modification under `root`, skipping [`KNOWN_CACHE_DIRS`] and
/// `.git`. Past [`ACTIVITY_BUDGET`] entries it returns what it found so far;
/// use [`last_activity_checked`] where "unknown" must not read as "idle".
pub fn last_activity(root: &Path) -> u64 {
    walk_newest(root, ACTIVITY_BUDGET).0
}

/// Newest file modification under `root`, or `None` when the walk ran out of
/// budget before finishing (the answer would be a guess).
pub fn last_activity_checked(root: &Path) -> Option<u64> {
    last_activity_within(root, ACTIVITY_BUDGET)
}

pub(crate) fn last_activity_within(root: &Path, budget: usize) -> Option<u64> {
    match walk_newest(root, budget) {
        (n, true) => Some(n),
        (_, false) => None,
    }
}

/// (newest mtime, finished within budget)
fn walk_newest(root: &Path, mut budget: usize) -> (u64, bool) {
    let mut newest = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if budget == 0 {
                return (newest, false);
            }
            budget -= 1;
            let n = e.file_name();
            if n == ".git" || KNOWN_CACHE_DIRS.iter().any(|k| n == *k) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else {
                newest = newest.max(mtime(&e.path()));
            }
        }
    }
    (newest, true)
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneResult {
    pub key: String,
    pub ok: bool,
    pub freed: u64,
    pub output: String,
    /// Why the prune did not run or did not succeed.
    pub reason: Option<SkipReason>,
}

impl PruneResult {
    fn refused(key: &str, reason: SkipReason, output: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ok: false,
            freed: 0,
            output: output.into(),
            reason: Some(reason),
        }
    }
}

/// Directories searched for tools after `$PATH`: the platform's package-manager
/// prefixes, then per-user tool directories.
pub fn tool_dirs(paths: &Paths) -> Vec<PathBuf> {
    let home = &paths.home;
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".into());
    } else {
        dirs.push("/home/linuxbrew/.linuxbrew/bin".into());
        dirs.push(home.join(".linuxbrew/bin"));
        dirs.push("/snap/bin".into());
    }
    dirs.push("/usr/local/bin".into());
    dirs.push("/usr/bin".into());
    dirs.push("/bin".into());
    for d in [
        ".bun/bin",
        ".cargo/bin",
        ".local/bin",
        "go/bin",
        ".volta/bin",
    ] {
        dirs.push(home.join(d));
    }
    dirs
}

/// An executable named `name` on `$PATH` or in [`tool_dirs`].
pub fn find_bin(paths: &Paths, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(tool_dirs(paths));
    dirs.into_iter()
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Run a package manager's own prune command for one cache category. Refused
/// while the tool itself (or, for Go, a build it drives) is running; killed
/// with its whole process group after [`PRUNE_TIMEOUT`].
pub fn prune_cache(paths: &Paths, procs: &dyn ProcessSource, key: &str) -> PruneResult {
    prune_cache_with(paths, procs, key, PRUNE_TIMEOUT)
}

fn prune_cache_with(
    paths: &Paths,
    procs: &dyn ProcessSource,
    key: &str,
    timeout: Duration,
) -> PruneResult {
    let Some(def) = categories_for(paths).into_iter().find(|c| c.key == key) else {
        return PruneResult::refused(key, SkipReason::UnknownTarget, "unknown cache");
    };
    let Some(pr) = def.prune else {
        return PruneResult::refused(
            key,
            SkipReason::NoPruneCommand,
            format!("no safe prune command; {}", def.hint),
        );
    };
    let Some(exe) = find_bin(paths, pr.bin) else {
        return PruneResult::refused(
            key,
            SkipReason::ToolNotFound,
            format!("{} not found", pr.bin),
        );
    };
    let uid = procs.current_uid();
    let me = procs.self_pid();
    if let Some(p) = procs
        .list()
        .into_iter()
        .find(|p| p.uid == Some(uid) && p.pid != me && is_tool(p, pr.busy))
    {
        return PruneResult::refused(
            key,
            SkipReason::Busy,
            format!(
                "{} (pid {}) is running; try again when it finishes",
                p.name, p.pid
            ),
        );
    }
    let before = dir_size(&def.path);
    let mut cmd = Command::new(exe);
    cmd.args(pr.args);
    let mut parts: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    parts.extend(tool_dirs(paths));
    if let Ok(joined) = std::env::join_paths(parts) {
        cmd.env("PATH", joined);
    }
    cmd.env("HOME", &paths.home);
    let out = run_with_timeout(cmd, timeout);
    let after = dir_size(&def.path);
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&o.stderr);
            let lines: Vec<&str> = text.lines().collect();
            let text = lines[lines.len().saturating_sub(6)..].join("\n");
            PruneResult {
                key: key.into(),
                ok: o.success,
                freed: before.saturating_sub(after),
                output: text,
                reason: (!o.success).then_some(SkipReason::CommandFailed),
            }
        }
        Err(GitError::Timeout(_)) => PruneResult {
            key: key.into(),
            ok: false,
            freed: before.saturating_sub(after),
            output: format!("{} ran past {}s and was stopped", pr.bin, timeout.as_secs()),
            reason: Some(SkipReason::TimedOut),
        },
        Err(e) => PruneResult::refused(key, SkipReason::CommandFailed, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Git;

    const DAY: u64 = 86_400;

    fn commit(paths: &Paths, dir: &Path, msg: &str) {
        Git::new(paths, dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["commit", "-q", "--allow-empty", "-m", msg])
            .run()
            .unwrap();
    }

    #[test]
    fn scans_worktrees_without_touching_home() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::create_dir_all(t.path().join("code/repo")).unwrap();
        let root = t.path().join("code").canonicalize().unwrap();
        let repo = root.join("repo");
        git(&paths, &repo, &["init", "-q", "-b", "main"]).unwrap();
        commit(&paths, &repo, "init");
        let agent = repo.join(".claude/worktrees/agent-x");
        let human = root.join("human-wt");
        let gone = repo.join(".claude/worktrees/agent-gone");
        for (p, b) in [(&agent, "feat"), (&human, "mine"), (&gone, "g")] {
            git(
                &paths,
                &repo,
                &["worktree", "add", "-q", "-b", b, p.to_str().unwrap()],
            )
            .unwrap();
        }
        std::fs::write(agent.join("new.txt"), "hello").unwrap();
        std::fs::rename(&gone, t.path().join("moved-away")).unwrap();

        let cfg = Config {
            scan_roots: vec![root.clone()],
            worktree_idle_days: 1,
            ..Config::default()
        };
        let later = crate::monitor::now_unix() + 30 * DAY;
        let map = scan(&cfg, &paths, &[], later);
        assert!(map.categories.is_empty(), "home is empty");
        let find = |n: &str| map.worktrees.iter().find(|w| w.name == n).unwrap().clone();
        let a = find("agent-x");
        assert!(a.agent && a.sweepable && a.dirty_files == 1 && !a.removable);
        assert!(a.head.as_deref().map(|h| h.len() == 40).unwrap_or(false));
        let h = find("human-wt");
        assert!(!h.agent && !h.sweepable && h.removable);
        let g = find("agent-gone");
        assert!(g.missing && g.sweepable);

        let busy = scan(&cfg, &paths, std::slice::from_ref(&agent), later);
        let a = busy.worktrees.iter().find(|w| w.name == "agent-x").unwrap();
        assert!(a.in_use && !a.sweepable);

        // C13: a git status that fails is not "clean".
        std::fs::write(repo.join(".git/worktrees/human-wt/index"), b"garbage").unwrap();
        std::fs::write(repo.join(".git/worktrees/agent-x/index"), b"garbage").unwrap();
        let map = scan(&cfg, &paths, &[], later);
        let find = |n: &str| map.worktrees.iter().find(|w| w.name == n).unwrap().clone();
        let h = find("human-wt");
        assert!(h.status_error.is_some(), "{h:?}");
        assert!(!h.removable && !h.sweepable, "{h:?}");
        assert!(
            h.verdict.starts_with("could not read git status"),
            "{}",
            h.verdict
        );
        let a = find("agent-x");
        assert!(a.status_error.is_some() && !a.sweepable && !a.removable);
        let v = serde_json::to_value(&h).unwrap();
        assert!(v["status_error"].is_string());
        assert!(v.get("activity_unknown").is_none());
    }

    /// `MULCHIE_BENCH_ROOT=<folder of repos> cargo test -p mulchie-core --release
    /// --lib diskmap::tests::bench_periodic_scan -- --ignored --nocapture`:
    /// the CPU a first periodic scan costs, then the next one. Read-only.
    #[test]
    #[ignore]
    fn bench_periodic_scan() {
        let Some(root) = std::env::var_os("MULCHIE_BENCH_ROOT").map(PathBuf::from) else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let cfg = Config {
            scan_roots: vec![root.canonicalize().unwrap()],
            ..Config::default()
        };
        let cpu = || {
            let mut r: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
            let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            s(r.ru_utime) + s(r.ru_stime)
        };
        let mut memo = ScanMemo::default();
        for round in 0..3 {
            let (c, at) = (cpu(), std::time::Instant::now());
            let m = scan_with(
                &cfg,
                &paths,
                &[],
                crate::monitor::now_unix(),
                &mut memo,
                true,
            );
            println!(
                "scan {round}: {:.2}s cpu, {:.2?} wall, {} worktrees, {} remembered",
                cpu() - c,
                at.elapsed(),
                m.worktrees.len(),
                memo.worktrees.len()
            );
        }
    }

    /// The periodic scan reuses a worktree's figures only while nothing
    /// about it moved; anything that may have changed is measured again.
    #[test]
    fn a_memo_is_reused_only_for_a_worktree_that_shows_no_change() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::create_dir_all(t.path().join("code/repo")).unwrap();
        let root = t.path().join("code").canonicalize().unwrap();
        let repo = root.join("repo");
        git(&paths, &repo, &["init", "-q", "-b", "main"]).unwrap();
        commit(&paths, &repo, "init");
        let wt = repo.join(".claude/worktrees/agent-x");
        git(
            &paths,
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        )
        .unwrap();
        std::fs::create_dir(wt.join("sub")).unwrap();
        std::fs::write(wt.join("sub/a.txt"), "small").unwrap();
        let cfg = Config {
            scan_roots: vec![root.clone()],
            worktree_idle_days: 1,
            ..Config::default()
        };
        let grow = |n: usize| std::fs::write(wt.join("sub/a.txt"), vec![b'x'; n]).unwrap();
        let bytes = |m: &DiskMap| {
            m.worktrees
                .iter()
                .find(|w| w.name == "agent-x")
                .unwrap()
                .bytes
        };
        let now = crate::monitor::now_unix();
        let later = now + 30 * DAY;

        // Changed within a day of the scan: measured every time.
        let mut memo = ScanMemo::default();
        let first = scan_with(&cfg, &paths, &[], now, &mut memo, true);
        grow(200_000);
        let second = scan_with(&cfg, &paths, &[], now + 60, &mut memo, true);
        assert!(bytes(&second) > bytes(&first) + 150_000);

        // Idle for a month: a change deep inside moves no mark, so the
        // figures are reused, and they keep the time they were measured.
        let mut memo = ScanMemo::default();
        let base = scan_with(&cfg, &paths, &[], later, &mut memo, true);
        assert_eq!(memo.worktrees[&wt].at_unix, later);
        grow(600_000);
        let reused = scan_with(&cfg, &paths, &[], later + 60, &mut memo, true);
        assert_eq!(bytes(&reused), bytes(&base));
        assert_eq!(memo.worktrees[&wt].at_unix, later);
        let w = reused
            .worktrees
            .iter()
            .find(|w| w.name == "agent-x")
            .unwrap();
        assert!(w.sweepable && w.dirty_files == 1 && w.idle_days >= 29);
        // A scan someone asked for measures it.
        assert!(bytes(&scan(&cfg, &paths, &[], later + 60)) > bytes(&base) + 300_000);

        // Something running in it: measured, and not reused next time either.
        let kept = memo.clone();
        let busy = scan_with(
            &cfg,
            &paths,
            std::slice::from_ref(&wt),
            later + 60,
            &mut memo,
            true,
        );
        assert!(bytes(&busy) > bytes(&base) + 300_000);
        assert!(memo.worktrees[&wt].in_use);
        grow(900_000);
        let after = scan_with(&cfg, &paths, &[], later + 120, &mut memo, true);
        assert!(bytes(&after) > bytes(&busy) + 200_000);

        // Too old.
        let mut memo = kept.clone();
        let old = scan_with(&cfg, &paths, &[], later + REMEASURE_SECS, &mut memo, true);
        assert!(bytes(&old) > bytes(&base) + 300_000);

        // A new commit, or a file added at the top: measured.
        let mut memo = kept.clone();
        commit(&paths, &wt, "more");
        let moved = scan_with(&cfg, &paths, &[], later + 60, &mut memo, true);
        assert!(bytes(&moved) > bytes(&base) + 300_000);
        let mut memo = ScanMemo::default();
        let base = scan_with(&cfg, &paths, &[], later, &mut memo, true);
        grow(1_500_000);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(wt.join("top.txt"), "new").unwrap();
        let top = scan_with(&cfg, &paths, &[], later + 60, &mut memo, true);
        assert!(bytes(&top) > bytes(&base) + 300_000);
    }

    /// C12: a walk that runs out of budget is unknown, not "idle"; caches are
    /// not walked.
    #[test]
    fn activity_walk_reports_unknown_past_its_budget() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        for i in 0..10 {
            std::fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        assert!(last_activity_within(d, 5).is_none());
        assert!(last_activity_within(d, 100).is_some());
        // Thousands of entries inside a cache directory cost nothing.
        let nm = d.join("node_modules/pkg");
        std::fs::create_dir_all(&nm).unwrap();
        for i in 0..50 {
            std::fs::write(nm.join(format!("m{i}")), "x").unwrap();
        }
        assert!(last_activity_within(d, 20).is_some());
    }
}
