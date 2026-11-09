//! Build-cache scanner. Read-only: it produces candidates, and the safety gate
//! (`safety.rs`) decides and deletes.

use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::paths::Paths;
use crate::safety::{any_cwd_in, project_root, recognise_cache};

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

/// Find build caches under the scan roots. `busy_cwds` are the working
/// directories of the user's processes (see `process::own_cwds`); a cache is in use
/// when one of them is at or under its project.
pub fn scan(cfg: &Config, paths: &Paths, busy_cwds: &[PathBuf], now: u64) -> ScanReport {
    let start = std::time::Instant::now();
    let mut found: Vec<(PathBuf, ItemKind, PathBuf)> = Vec::new();
    for root in cfg.roots(paths) {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        walk(&root, &root, 0, cfg, &mut found);
    }
    let dd = paths.derived_data();
    if cfg.include_xcode && dd.is_dir() {
        if let Ok(dd) = dd.canonicalize() {
            found.push((dd.clone(), ItemKind::XcodeDerivedData, dd));
        }
    }
    found.sort();
    found.dedup();

    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    let chunk = found.len().div_ceil(threads).max(1);
    let stale_secs = cfg.stale_hours * 3600;
    let wt_stale_secs = cfg.worktree_target_stale_hours * 3600;

    let mut items: Vec<Item> = std::thread::scope(|s| {
        let handles: Vec<_> = found
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(path, kind, root)| {
                            let project = path.parent().unwrap_or(path).to_path_buf();
                            let bytes = dir_size(path);
                            let last = last_used(path);
                            let (proj_root, in_worktree) = if *kind == ItemKind::XcodeDerivedData {
                                (path.clone(), false)
                            } else {
                                project_root(&project, root)
                            };
                            let in_use = any_cwd_in(busy_cwds, &proj_root, true);
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

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    cfg: &Config,
    out: &mut Vec<(PathBuf, ItemKind, PathBuf)>,
) {
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
            "target" | "node_modules" | ".next" => {
                if let Ok(kind) = recognise_cache(&path, cfg) {
                    out.push((path, kind, root.to_path_buf()));
                    continue;
                }
                if name != "target" || dir.join("Cargo.toml").is_file() {
                    continue;
                }
            }
            ".git" | "Library" | ".Trash" | "Applications" | "Pictures" | "Music" | "Movies" => {
                continue
            }
            n if n.starts_with('.') && n != ".claude" && n != "worktrees" => continue,
            _ => {}
        }
        walk(root, &path, depth + 1, cfg, out);
    }
}

/// Disk usage of everything under `path`, without following symbolic links.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.blocks() * 512;
            }
        }
    }
    total
}

/// Newest modification time of `path` and two levels below it.
pub fn last_used(path: &Path) -> u64 {
    let mut newest = mtime(path);
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            newest = newest.max(mtime(&e.path()));
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                for e2 in rd2.flatten().take(256) {
                    newest = newest.max(mtime(&e2.path()));
                }
            }
        }
    }
    newest
}

fn mtime(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .ok()
        .map(|m| to_unix(m.modified().ok()))
        .unwrap_or(0)
}

fn to_unix(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::CACHEDIR_TAG_SIGNATURE;

    const NOW: u64 = 4_000_000_000;

    fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        let root = t.path().join("code");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        (t, p, root)
    }

    fn cargo_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
        std::fs::write(dir.join("target/debug/blob"), vec![1u8; 20_000]).unwrap();
    }

    fn cfg(root: &Path) -> Config {
        Config {
            scan_roots: vec![root.to_path_buf()],
            include_xcode: false,
            ..Config::default()
        }
    }

    #[test]
    fn finds_tagged_cargo_target_only() {
        let (_t, p, root) = setup();
        cargo_project(&root.join("a/b/proj"));
        let untagged = root.join("a/b/other");
        std::fs::create_dir_all(untagged.join("target")).unwrap();
        std::fs::write(untagged.join("Cargo.toml"), "").unwrap();
        std::fs::write(untagged.join("target/x"), b"keep").unwrap();
        let report = scan(&cfg(&root), &p, &[], NOW);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, ItemKind::CargoTarget);
        assert!(report.items[0].stale);
    }

    #[test]
    fn busy_subdirectory_marks_project_in_use() {
        let (_t, p, root) = setup();
        let proj = root.join("p");
        cargo_project(&proj);
        std::fs::create_dir_all(proj.join("src/deep")).unwrap();
        let report = scan(&cfg(&root), &p, &[proj.join("src/deep")], NOW);
        assert!(report.items[0].in_use && !report.items[0].stale);
        let sibling = root.join("p2");
        std::fs::create_dir_all(&sibling).unwrap();
        let report = scan(&cfg(&root), &p, &[sibling], NOW);
        assert!(!report.items[0].in_use, "string prefix p2 must not match p");
    }

    #[test]
    fn busy_worktree_does_not_pin_main_repo() {
        let (_t, p, root) = setup();
        let repo = root.join("repo");
        let wt = repo.join(".claude/worktrees/w");
        cargo_project(&repo);
        cargo_project(&wt);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        let report = scan(&cfg(&root), &p, std::slice::from_ref(&wt), NOW);
        let main = report.items.iter().find(|i| i.project == repo).unwrap();
        let tree = report.items.iter().find(|i| i.project == wt).unwrap();
        assert!(!main.in_use && main.stale && !main.in_worktree);
        assert!(tree.in_use && !tree.stale && tree.in_worktree);
    }

    #[test]
    fn node_modules_only_when_enabled() {
        let (_t, p, root) = setup();
        let proj = root.join("web");
        std::fs::create_dir_all(proj.join("node_modules/x")).unwrap();
        std::fs::write(proj.join("package.json"), "{}").unwrap();
        std::fs::write(proj.join("node_modules/x/i.js"), vec![0u8; 5000]).unwrap();
        assert!(scan(&cfg(&root), &p, &[], NOW).items.is_empty());
        let on = Config {
            include_node_modules: true,
            ..cfg(&root)
        };
        assert_eq!(scan(&on, &p, &[], NOW).items[0].kind, ItemKind::NodeModules);
    }

    #[test]
    fn dir_size_does_not_follow_symlinks() {
        let (t, _p, root) = setup();
        let big = t.path().join("big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("f"), vec![0u8; 200_000]).unwrap();
        let d = root.join("d");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&big, d.join("link")).unwrap();
        assert!(dir_size(&d) < 100_000);
    }
}
