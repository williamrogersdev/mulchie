//! Disk map: package and toolchain caches, and every git worktree under the scan
//! roots. Read-only; removal goes through the safety gate.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::config::Config;
use crate::git::{self, git};
use crate::paths::Paths;
use crate::reclaim::dir_size;
use crate::safety::{any_cwd_in, is_agent_worktree};

/// Repositories scanned at once.
const REPO_WORKERS: usize = 8;

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
    prune: Option<(&'static str, &'static [&'static str])>,
}

fn categories_for(paths: &Paths) -> Vec<CatDef> {
    let home = &paths.home;
    let lib = home.join("Library");
    let c = |key, name, path, hint, prune| CatDef {
        key,
        name,
        path,
        hint,
        prune,
    };
    vec![
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
            Some(("npm", &["cache", "clean", "--force"])),
        ),
        c(
            "bun",
            "bun install cache",
            home.join(".bun/install/cache"),
            "bun pm cache rm",
            Some(("bun", &["pm", "cache", "rm"])),
        ),
        c(
            "pnpm",
            "pnpm store",
            lib.join("pnpm/store"),
            "pnpm store prune",
            Some(("pnpm", &["store", "prune"])),
        ),
        c(
            "yarn",
            "Yarn cache",
            lib.join("Caches/Yarn"),
            "yarn cache clean",
            Some(("yarn", &["cache", "clean"])),
        ),
        c(
            "uv",
            "uv cache",
            home.join(".cache/uv"),
            "uv cache prune",
            Some(("uv", &["cache", "prune"])),
        ),
        c(
            "pip",
            "pip cache",
            lib.join("Caches/pip"),
            "pip cache purge",
            Some(("pip3", &["cache", "purge"])),
        ),
        c(
            "homebrew",
            "Homebrew downloads",
            lib.join("Caches/Homebrew"),
            "brew cleanup --prune=7",
            Some(("brew", &["cleanup", "--prune=7"])),
        ),
        c(
            "go",
            "Go module cache",
            home.join("go/pkg/mod"),
            "go clean -modcache",
            Some(("go", &["clean", "-modcache"])),
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
            Some(("xcrun", &["simctl", "delete", "unavailable"])),
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
        c(
            "hf",
            "Hugging Face models",
            home.join(".cache/huggingface"),
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
    ]
}

/// Scan package caches and every git worktree under the scan roots. `busy_cwds`
/// are the working directories of the user's processes, read now.
pub fn scan(cfg: &Config, paths: &Paths, busy_cwds: &[PathBuf], now: u64) -> DiskMap {
    let start = std::time::Instant::now();
    let cats: Vec<CatDef> = categories_for(paths)
        .into_iter()
        .filter(|c| c.path.exists())
        .collect();
    let repos = find_repos(cfg, paths);
    // A bounded pool: at most REPO_WORKERS repositories at once.
    let queue = Mutex::new(repos.iter());

    let (mut categories, mut worktrees) = std::thread::scope(|s| {
        let ch: Vec<_> = cats
            .iter()
            .map(|c| {
                s.spawn(move || Category {
                    key: c.key.to_string(),
                    name: c.name.to_string(),
                    path: c.path.clone(),
                    bytes: dir_size(&c.path),
                    hint: c.hint.to_string(),
                    prunable: c.prune.is_some(),
                })
            })
            .collect();
        let wh: Vec<_> = (0..REPO_WORKERS.min(repos.len()))
            .map(|_| {
                let queue = &queue;
                s.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                        let Some(r) = next else { break };
                        out.extend(repo_worktrees(r, busy_cwds, cfg, paths, now));
                    }
                    out
                })
            })
            .collect();
        let cats: Vec<Category> = ch.into_iter().filter_map(|h| h.join().ok()).collect();
        let wts: Vec<Worktree> = wh
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect();
        (cats, wts)
    });

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
) -> Vec<Worktree> {
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
            let bytes = if exists { dir_size(&path) } else { 0 };
            let dirty_files = if exists {
                git(
                    paths,
                    &path,
                    &[
                        "status",
                        "--porcelain=v1",
                        "--untracked-files=all",
                        "--ignore-submodules=all",
                    ],
                )
                .map(|s| s.lines().filter(|l| !l.is_empty()).count())
                .unwrap_or(0)
            } else {
                0
            };
            let last_commit_unix = if exists {
                git(paths, &path, &["log", "-1", "--format=%ct", "HEAD"])
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0)
            } else {
                0
            };
            let last_activity_unix = if exists {
                last_activity(&path).max(last_commit_unix)
            } else {
                0
            };
            let idle_days = now.saturating_sub(last_activity_unix) / 86_400;
            let is_merged = e.branch.as_ref().map(|b| merged.contains(b)).unwrap_or(false);
            let locked = e.locked;
            let removable = (missing && agent) || (exists && !in_use && !locked && dirty_files == 0);
            let sweepable = agent
                && !locked
                && (missing || (exists && !in_use && idle_days >= cfg.worktree_idle_days));
            let verdict = if missing && !agent {
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
            } else {
                format!("clean; active {idle_days}d ago")
            };
            Worktree {
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
            }
        })
        .collect()
}

fn mtime(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Newest file modification under `root`, skipping caches and `.git`, within a
/// budget of 20,000 entries.
pub fn last_activity(root: &Path) -> u64 {
    let mut newest = 0u64;
    let mut budget = 20_000usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if budget == 0 {
                return newest;
            }
            budget -= 1;
            let n = e.file_name();
            if n == "target" || n == "node_modules" || n == ".git" || n == ".next" || n == "dist" {
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
    newest
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneResult {
    pub key: String,
    pub ok: bool,
    pub freed: u64,
    pub output: String,
}

fn find_bin(paths: &Paths, name: &str) -> Option<PathBuf> {
    let home = &paths.home;
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        dirs.push(PathBuf::from(d));
    }
    for d in [".bun/bin", ".cargo/bin", ".local/bin", "go/bin"] {
        dirs.push(home.join(d));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Run a package manager's own prune command for one cache category.
pub fn prune_cache(paths: &Paths, key: &str) -> PruneResult {
    let Some(def) = categories_for(paths).into_iter().find(|c| c.key == key) else {
        return PruneResult {
            key: key.into(),
            ok: false,
            freed: 0,
            output: "unknown cache".into(),
        };
    };
    let Some((bin, args)) = def.prune else {
        return PruneResult {
            key: key.into(),
            ok: false,
            freed: 0,
            output: format!("no safe prune command; {}", def.hint),
        };
    };
    let Some(exe) = find_bin(paths, bin) else {
        return PruneResult {
            key: key.into(),
            ok: false,
            freed: 0,
            output: format!("{bin} not found"),
        };
    };
    let before = dir_size(&def.path);
    let mut cmd = Command::new(exe);
    cmd.args(args);
    if let Some(path) = std::env::var_os("PATH") {
        let mut parts: Vec<PathBuf> = std::env::split_paths(&path).collect();
        parts.push("/opt/homebrew/bin".into());
        parts.push("/usr/local/bin".into());
        if let Ok(joined) = std::env::join_paths(parts) {
            cmd.env("PATH", joined);
        }
    }
    let out = cmd.output();
    let after = dir_size(&def.path);
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let text: String = text
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            PruneResult {
                key: key.into(),
                ok: o.status.success(),
                freed: before.saturating_sub(after),
                output: text,
            }
        }
        Err(e) => PruneResult {
            key: key.into(),
            ok: false,
            freed: 0,
            output: e.to_string(),
        },
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
    }
}
