use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::{home_dir, Config};
use crate::monitor::now_unix;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    CargoTarget,
    NodeModules,
    NextCache,
    XcodeDerivedData,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::CargoTarget => "Rust build cache",
            ItemKind::NodeModules => "node_modules",
            ItemKind::NextCache => "Next.js cache",
            ItemKind::XcodeDerivedData => "Xcode DerivedData",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub path: PathBuf,
    pub project: PathBuf,
    pub kind: ItemKind,
    pub bytes: u64,
    pub last_used_unix: u64,
    pub in_worktree: bool,
    pub in_use: bool,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ScanReport {
    pub at_unix: u64,
    pub took_ms: u64,
    pub items: Vec<Item>,
    pub total_bytes: u64,
    pub stale_bytes: u64,
}

const MAX_DEPTH: usize = 9;

pub fn scan(cfg: &Config, busy: &[PathBuf]) -> ScanReport {
    let start = std::time::Instant::now();
    let mut found: Vec<(PathBuf, ItemKind)> = Vec::new();
    for root in &cfg.scan_roots {
        walk(root, 0, cfg, &mut found);
    }
    let dd = home_dir().join("Library/Developer/Xcode/DerivedData");
    if cfg.include_xcode && dd.is_dir() {
        found.push((dd, ItemKind::XcodeDerivedData));
    }
    found.sort();
    found.dedup();

    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    let chunk = found.len().div_ceil(threads).max(1);
    let stale_secs = cfg.stale_hours * 3600;
    let wt_stale_secs = cfg.worktree_target_stale_hours * 3600;
    let now = now_unix();

    let mut items: Vec<Item> = std::thread::scope(|s| {
        let handles: Vec<_> = found
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(path, kind)| {
                            let project = path.parent().unwrap_or(path).to_path_buf();
                            let bytes = dir_size(path);
                            let last = last_used(path);
                            let (root, in_worktree) =
                                git_root(&project).unwrap_or((project.clone(), false));
                            let in_use = busy.contains(&root);
                            let limit = if in_worktree {
                                wt_stale_secs
                            } else {
                                stale_secs
                            };
                            let stale = !in_use && now.saturating_sub(last) >= limit;
                            Item {
                                path: path.clone(),
                                project,
                                kind: *kind,
                                bytes,
                                last_used_unix: last,
                                in_worktree,
                                in_use,
                                stale,
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });

    items.retain(|i| i.bytes > 0);
    items.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let total_bytes = items.iter().map(|i| i.bytes).sum();
    let stale_bytes = items.iter().filter(|i| i.stale).map(|i| i.bytes).sum();
    ScanReport {
        at_unix: now,
        took_ms: start.elapsed().as_millis() as u64,
        items,
        total_bytes,
        stale_bytes,
    }
}

fn walk(dir: &Path, depth: usize, cfg: &Config, out: &mut Vec<(PathBuf, ItemKind)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        match name.as_ref() {
            "target" if dir.join("Cargo.toml").is_file() => {
                out.push((path, ItemKind::CargoTarget));
                continue;
            }
            "node_modules" => {
                if cfg.include_node_modules && dir.join("package.json").is_file() {
                    out.push((path, ItemKind::NodeModules));
                }
                continue;
            }
            ".next" if dir.join("package.json").is_file() => {
                out.push((path, ItemKind::NextCache));
                continue;
            }
            ".git" | "Library" | ".Trash" | "Applications" | "Pictures" | "Music" | "Movies" => {
                continue
            }
            n if n.starts_with('.') && n != ".claude" && n != "worktrees" => continue,
            _ => {}
        }
        walk(&path, depth + 1, cfg, out);
    }
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.blocks() * 512;
            }
        }
    }
    total
}

fn last_used(path: &Path) -> u64 {
    let mut newest = mtime(path);
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            newest = newest.max(
                e.metadata()
                    .ok()
                    .map(|m| to_unix(m.modified().ok()))
                    .unwrap_or(0),
            );
            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                for e2 in rd2.flatten().take(256) {
                    newest = newest.max(
                        e2.metadata()
                            .ok()
                            .map(|m| to_unix(m.modified().ok()))
                            .unwrap_or(0),
                    );
                }
            }
        }
    }
    newest
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .ok()
        .map(|m| to_unix(m.modified().ok()))
        .unwrap_or(0)
}

fn to_unix(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn git_root(project: &Path) -> Option<(PathBuf, bool)> {
    let mut cur = Some(project);
    while let Some(d) = cur {
        if let Ok(m) = std::fs::symlink_metadata(d.join(".git")) {
            return Some((d.to_path_buf(), m.is_file()));
        }
        cur = d.parent();
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanResult {
    pub path: PathBuf,
    pub freed: u64,
    pub error: Option<String>,
}

pub fn verify_cleanable(path: &Path, cfg: &Config) -> Result<ItemKind, String> {
    if !path.is_absolute() {
        return Err("path must be absolute".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("not a real directory".into());
    }
    let canon = path.canonicalize().map_err(|e| e.to_string())?;
    if canon != path {
        return Err("path is not canonical".into());
    }
    let home = home_dir();
    if path.components().count() < 4 || path == home {
        return Err("path too shallow".into());
    }
    if path == home.join("Library/Developer/Xcode/DerivedData") {
        return Ok(ItemKind::XcodeDerivedData);
    }
    if !cfg
        .scan_roots
        .iter()
        .any(|r| path.starts_with(r) && path != r)
    {
        return Err("path is outside the configured scan roots".into());
    }
    let parent = path.parent().ok_or("no parent")?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    match name.as_str() {
        "target" if parent.join("Cargo.toml").is_file() => Ok(ItemKind::CargoTarget),
        "node_modules" if parent.join("package.json").is_file() => Ok(ItemKind::NodeModules),
        ".next" if parent.join("package.json").is_file() => Ok(ItemKind::NextCache),
        _ => Err("not a recognised build cache".into()),
    }
}

pub fn clean(path: &Path, cfg: &Config) -> CleanResult {
    if let Err(e) = verify_cleanable(path, cfg) {
        return CleanResult {
            path: path.to_path_buf(),
            freed: 0,
            error: Some(e),
        };
    }
    let before = dir_size(path);
    let res = std::fs::remove_dir_all(path);
    let after = if path.exists() { dir_size(path) } else { 0 };
    CleanResult {
        path: path.to_path_buf(),
        freed: before.saturating_sub(after),
        error: res.err().map(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "mulchie-test-{tag}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn finds_and_cleans_cargo_target_only() {
        let root = tmp("clean");
        let proj = root.join("a/b/proj");
        std::fs::create_dir_all(proj.join("target/debug")).unwrap();
        std::fs::write(proj.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(proj.join("target/debug/blob"), vec![1u8; 100_000]).unwrap();
        let other = root.join("a/b/other/target");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("x"), b"keep").unwrap();

        let cfg = Config {
            scan_roots: vec![root.clone()],
            stale_hours: 0,
            include_xcode: false,
            ..Config::default()
        };
        let report = scan(&cfg, &[]);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, ItemKind::CargoTarget);
        assert!(report.items[0].stale);

        assert!(verify_cleanable(&other, &cfg).is_err());
        assert!(verify_cleanable(&root, &cfg).is_err());
        let r = clean(&proj.join("target"), &cfg);
        assert!(r.error.is_none());
        assert!(r.freed >= 100_000);
        assert!(!proj.join("target").exists());
        assert!(other.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn busy_projects_are_not_stale() {
        let root = tmp("busy");
        let proj = root.join("p");
        std::fs::create_dir_all(proj.join("target")).unwrap();
        std::fs::write(proj.join("Cargo.toml"), "").unwrap();
        std::fs::write(proj.join("target/x"), vec![0u8; 5000]).unwrap();
        let cfg = Config {
            scan_roots: vec![root.clone()],
            stale_hours: 0,
            include_xcode: false,
            ..Config::default()
        };
        let report = scan(&cfg, std::slice::from_ref(&proj));
        assert!(report.items[0].in_use);
        assert!(!report.items[0].stale);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn busy_worktree_does_not_pin_main_repo() {
        let root = tmp("wt");
        let repo = root.join("repo");
        let wt = repo.join(".claude/worktrees/w");
        for d in [&repo, &wt] {
            std::fs::create_dir_all(d.join("target")).unwrap();
            std::fs::write(d.join("Cargo.toml"), "").unwrap();
            std::fs::write(d.join("target/x"), vec![0u8; 5000]).unwrap();
        }
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        let cfg = Config {
            scan_roots: vec![root.clone()],
            stale_hours: 0,
            include_xcode: false,
            ..Config::default()
        };
        let report = scan(&cfg, std::slice::from_ref(&wt));
        let main = report.items.iter().find(|i| i.project == repo).unwrap();
        let tree = report.items.iter().find(|i| i.project == wt).unwrap();
        assert!(!main.in_use && main.stale && !main.in_worktree);
        assert!(tree.in_use && !tree.stale && tree.in_worktree);
        std::fs::remove_dir_all(root).ok();
    }
}
