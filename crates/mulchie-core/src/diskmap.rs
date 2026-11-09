use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::config::{config_dir, home_dir, Config};
use crate::monitor::now_unix;
use crate::reclaim::dir_size;

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

fn categories_for(home: &Path) -> Vec<CatDef> {
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
            config_dir().join("backups"),
            "patches saved before worktree removal",
            None,
        ),
    ]
}

pub fn scan(cfg: &Config, busy: &[PathBuf]) -> DiskMap {
    let start = std::time::Instant::now();
    let home = home_dir();
    let cats: Vec<CatDef> = categories_for(&home)
        .into_iter()
        .filter(|c| c.path.exists())
        .collect();
    let repos = find_repos(cfg);

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
        let wh: Vec<_> = repos
            .iter()
            .map(|r| s.spawn(move || repo_worktrees(r, busy, cfg)))
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
        at_unix: now_unix(),
        took_ms: start.elapsed().as_millis() as u64,
        categories,
        worktrees,
        worktree_bytes,
        sweepable_bytes,
    }
}

fn find_repos(cfg: &Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in &cfg.scan_roots {
        walk_repos(root, 0, &mut out);
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

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn repo_worktrees(repo: &Path, busy: &[PathBuf], cfg: &Config) -> Vec<Worktree> {
    let Some(list) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let main_branch = git(repo, &["symbolic-ref", "--short", "HEAD"]).map(|s| s.trim().to_string());
    let merged: Vec<String> = main_branch
        .as_ref()
        .and_then(|b| {
            git(
                repo,
                &["branch", "--merged", b, "--format=%(refname:short)"],
            )
        })
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();

    let mut entries: Vec<HashMap<&str, String>> = Vec::new();
    let mut cur: HashMap<&str, String> = HashMap::new();
    for line in list.lines() {
        if line.is_empty() {
            if !cur.is_empty() {
                entries.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        let key: &str = match k {
            "worktree" => "worktree",
            "branch" => "branch",
            "HEAD" => "head",
            "detached" => "detached",
            "locked" => "locked",
            "prunable" => "prunable",
            "bare" => "bare",
            _ => continue,
        };
        cur.insert(key, v.to_string());
    }
    if !cur.is_empty() {
        entries.push(cur);
    }

    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let now = now_unix();
    entries
        .into_iter()
        .skip(1)
        .filter(|e| !e.contains_key("bare"))
        .filter_map(|e| {
            let path = PathBuf::from(e.get("worktree")?);
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let branch = e.get("branch").map(|b| b.trim_start_matches("refs/heads/").to_string());
            let detached = e.contains_key("detached");
            let missing = !path.exists() || e.contains_key("prunable");
            let locked = e.contains_key("locked");
            let in_use = busy.contains(&path);
            let agent = name.starts_with("agent-") || path.to_string_lossy().contains("/.claude/worktrees/");
            let bytes = if missing { 0 } else { dir_size(&path) };
            let dirty_files = if missing {
                0
            } else {
                git(&path, &["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0)
            };
            let last_commit_unix = if missing {
                0
            } else {
                git(&path, &["log", "-1", "--format=%ct", "HEAD"]).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
            };
            let last_activity_unix = if missing { 0 } else { last_activity(&path).max(last_commit_unix) };
            let idle_days = now.saturating_sub(last_activity_unix) / 86_400;
            let is_merged = branch.as_ref().map(|b| merged.contains(b)).unwrap_or(false);
            let removable = missing || (!in_use && !locked && dirty_files == 0);
            let sweepable = missing || (agent && !in_use && !locked && idle_days >= cfg.worktree_idle_days);
            let verdict = if missing {
                "directory is gone; prune the stale git entry".to_string()
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
            Some(Worktree {
                name,
                path,
                repo: repo.to_path_buf(),
                repo_name: repo_name.clone(),
                branch,
                detached,
                agent,
                bytes,
                last_activity_unix,
                last_commit_unix,
                idle_days,
                dirty_files,
                merged: is_merged,
                missing,
                locked,
                in_use,
                removable,
                sweepable,
                verdict,
            })
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
pub struct RemoveResult {
    pub path: PathBuf,
    pub ok: bool,
    pub freed: u64,
    pub message: String,
    pub backup: Option<PathBuf>,
}

impl RemoveResult {
    fn fail(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            ok: false,
            freed: 0,
            message: message.into(),
            backup: None,
        }
    }
}

fn save_patch(wt: &Worktree) -> Result<PathBuf, String> {
    let dir = config_dir().join("backups");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{}-{}-{}.patch", wt.repo_name, wt.name, now_unix()));
    let index = std::env::temp_dir().join(format!(
        "mulchie-index-{}-{}",
        std::process::id(),
        now_unix()
    ));
    let real_index =
        git(&wt.path, &["rev-parse", "--git-path", "index"]).map(|s| s.trim().to_string());
    if let Some(ri) = real_index {
        let ri = if Path::new(&ri).is_absolute() {
            PathBuf::from(ri)
        } else {
            wt.path.join(ri)
        };
        std::fs::copy(&ri, &index).ok();
    }
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
    };
    let added = run(&["add", "-A"]).map_err(|e| e.to_string())?;
    if !added.status.success() {
        std::fs::remove_file(&index).ok();
        return Err(format!(
            "could not stage a backup: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        ));
    }
    let diff = run(&["diff", "--cached", "--binary", "HEAD"]).map_err(|e| e.to_string())?;
    std::fs::remove_file(&index).ok();
    if !diff.status.success() {
        return Err("git diff failed while saving a backup".into());
    }
    std::fs::write(&file, &diff.stdout).map_err(|e| e.to_string())?;
    Ok(file)
}

pub fn remove_worktree(wt: &Worktree, backup_dirty: bool) -> RemoveResult {
    if wt.missing {
        let ok = git(&wt.repo, &["worktree", "prune"]).is_some();
        return RemoveResult {
            path: wt.path.clone(),
            ok,
            freed: 0,
            message: if ok {
                "pruned stale entry".into()
            } else {
                "git worktree prune failed".into()
            },
            backup: None,
        };
    }
    if wt.in_use {
        return RemoveResult::fail(&wt.path, "agent or process running here");
    }
    if wt.locked {
        return RemoveResult::fail(&wt.path, "locked by git");
    }
    let mut backup = None;
    if wt.dirty_files > 0 {
        if !backup_dirty {
            return RemoveResult::fail(
                &wt.path,
                format!(
                    "{} uncommitted file(s); pass backup to save a patch and remove",
                    wt.dirty_files
                ),
            );
        }
        match save_patch(wt) {
            Ok(p) => backup = Some(p),
            Err(e) => return RemoveResult::fail(&wt.path, e),
        }
    }
    let mut kept = wt.branch.clone();
    if wt.detached {
        let name = format!("backup/{}", wt.name);
        if git(&wt.path, &["branch", "-f", &name, "HEAD"]).is_none() {
            return RemoveResult::fail(
                &wt.path,
                "could not create a backup branch for the detached HEAD",
            );
        }
        kept = Some(name);
    }
    let mut args = vec!["worktree", "remove"];
    if backup.is_some() {
        args.push("--force");
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(&wt.repo)
        .args(&args)
        .arg(&wt.path)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let mut message = match kept {
                Some(b) => format!("removed; branch {b} kept"),
                None => "removed".into(),
            };
            if let Some(p) = &backup {
                message.push_str(&format!("; patch saved to {}", p.display()));
            }
            RemoveResult {
                path: wt.path.clone(),
                ok: true,
                freed: wt.bytes,
                message,
                backup,
            }
        }
        Ok(o) => RemoveResult {
            backup,
            ..RemoveResult::fail(
                &wt.path,
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            )
        },
        Err(e) => RemoveResult {
            backup,
            ..RemoveResult::fail(&wt.path, e.to_string())
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneResult {
    pub key: String,
    pub ok: bool,
    pub freed: u64,
    pub output: String,
}

fn find_bin(name: &str) -> Option<PathBuf> {
    let home = home_dir();
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

pub fn prune_cache(key: &str) -> PruneResult {
    let home = home_dir();
    let Some(def) = categories_for(&home).into_iter().find(|c| c.key == key) else {
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
    let Some(exe) = find_bin(bin) else {
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

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn dirty_worktree_is_backed_up_then_removed_and_branch_kept() {
        let root =
            std::env::temp_dir().join(format!("mulchie-wt-{}-{}", std::process::id(), now_unix()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt_path = repo.join(".claude/worktrees/agent-x");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                wt_path.to_str().unwrap(),
            ],
        );
        std::fs::write(wt_path.join("new.txt"), "hello").unwrap();

        let cfg = Config {
            scan_roots: vec![root.clone()],
            worktree_idle_days: 0,
            ..Config::default()
        };
        let map = scan(&cfg, &[]);
        let wt = map
            .worktrees
            .iter()
            .find(|w| w.name == "agent-x")
            .unwrap()
            .clone();
        assert!(wt.agent && wt.sweepable && wt.dirty_files == 1 && !wt.removable);

        let refused = remove_worktree(&wt, false);
        assert!(!refused.ok && wt_path.exists());

        let r = remove_worktree(&wt, true);
        assert!(r.ok, "{}", r.message);
        assert!(!wt_path.exists());
        let patch = std::fs::read_to_string(r.backup.as_ref().unwrap()).unwrap();
        assert!(patch.contains("new.txt") && patch.contains("hello"));
        assert!(git(&repo, &["rev-parse", "--verify", "feat"]).is_some());
        std::fs::remove_file(r.backup.unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }
}
