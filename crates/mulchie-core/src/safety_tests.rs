//! One test per safety rule, against real temporary git repositories, a fake
//! process table and `Paths` under a tempdir. Nothing here reads the real home
//! directory or signals a process the test did not spawn.

use super::*;
use crate::process::FakeProcesses;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const DAY: u64 = 86_400;

struct Env {
    _t: tempfile::TempDir,
    paths: Paths,
    root: PathBuf,
    cfg: Config,
    procs: FakeProcesses,
    now: u64,
}

impl Env {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let root = t.path().join("code");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let cfg = Config {
            scan_roots: vec![root.clone()],
            include_xcode: false,
            worktree_idle_days: 1,
            ..Config::default()
        };
        Self {
            _t: t,
            paths,
            root,
            cfg,
            procs: FakeProcesses::empty(),
            now: crate::monitor::now_unix() + 30 * DAY,
        }
    }

    fn gate(&self) -> Gate<'_> {
        Gate::new(&self.cfg, &self.paths, &self.procs, self.now)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        Git::new(&self.paths, dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .text()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn head(&self, dir: &Path) -> String {
        self.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    /// A repo with a committed `.gitignore` covering secrets and caches.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        self.git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join(".gitignore"),
            ".env\n*.sqlite\nnotes/\ntarget/\nnode_modules/\nbig.bin\n",
        )
        .unwrap();
        std::fs::write(repo.join("lib.rs"), "fn main() {}\n").unwrap();
        self.git(&repo, &["add", "-A"]);
        self.git(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    fn worktree(&self, repo: &Path, name: &str, detached: bool) -> PathBuf {
        let p = repo.join(".claude/worktrees").join(name);
        let ps = p.to_str().unwrap().to_string();
        if detached {
            self.git(repo, &["worktree", "add", "-q", "--detach", &ps]);
        } else {
            self.git(repo, &["worktree", "add", "-q", "-b", name, &ps]);
        }
        p
    }

    fn cargo_project(&self, rel: &str) -> PathBuf {
        let dir = self.root.join(rel);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
        std::fs::write(dir.join("target/debug/blob"), vec![1u8; 50_000]).unwrap();
        dir.join("target")
    }

    /// Make a worktree directory disappear (moved elsewhere in the tempdir).
    fn vanish(&self, p: &Path, i: usize) {
        let away = self._t.path().join(format!("gone-{i}"));
        std::fs::rename(p, away).unwrap();
    }

    fn remove_req(&self, path: &Path, head: &str) -> Request {
        Request::RemoveWorktree {
            path: path.to_path_buf(),
            expected_head: head.into(),
            allow_dirty: true,
            require_idle: false,
        }
    }
}

fn reason(r: &ActionResult) -> Option<SkipReason> {
    r.reason()
}

fn done(r: &ActionResult) -> bool {
    assert!(r.is_done() || r.reason().is_some());
    r.is_done()
}

fn backup_of(r: &ActionResult, kind: BackupKind) -> Option<String> {
    r.backups()
        .iter()
        .find(|b| b.kind == kind)
        .map(|b| b.location.clone())
}

fn cache(path: &Path) -> Request {
    Request::DeleteCache {
        path: path.to_path_buf(),
        require_stale: false,
    }
}

// --------------------------------------------------------------------- caches

#[test]
fn cache_is_deleted_when_every_check_passes_and_journaled() {
    let env = Env::new();
    let target = env.cargo_project("a/proj");
    let journal = Journal::new(&env.paths);
    let r = env
        .gate()
        .with_journal(&journal)
        .execute_one(&cache(&target), "test");
    assert!(done(&r), "{r:?}");
    assert!(r.freed_bytes() >= 50_000);
    assert!(!target.exists());
    let parent = target.parent().unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("mulchie-delete"))
        .collect();
    assert!(leftovers.is_empty());
    let j = journal.tail(10);
    assert_eq!(j.len(), 1);
    assert_eq!(j[0].status.as_deref(), Some("done"));
}

#[test]
fn symlinked_cache_path_is_refused() {
    let env = Env::new();
    let target = env.cargo_project("real");
    let link_proj = env.root.join("linked");
    std::fs::create_dir_all(&link_proj).unwrap();
    std::fs::write(link_proj.join("Cargo.toml"), "").unwrap();
    std::os::unix::fs::symlink(&target, link_proj.join("target")).unwrap();
    let r = env
        .gate()
        .execute_one(&cache(&link_proj.join("target")), "t");
    assert_eq!(reason(&r), Some(SkipReason::Symlink));

    // A symlinked ancestor: the path is not canonical.
    std::os::unix::fs::symlink(env.root.join("real"), env.root.join("alias")).unwrap();
    let r = env
        .gate()
        .execute_one(&cache(&env.root.join("alias/target")), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotCanonical));
    assert!(target.exists());
}

#[test]
fn cache_outside_scan_roots_is_refused() {
    let env = Env::new();
    let target = env.cargo_project("p");
    let cfg = Config {
        scan_roots: vec![env.paths.home.clone()],
        ..env.cfg.clone()
    };
    let r = Gate::new(&cfg, &env.paths, &env.procs, env.now).execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::OutsideScanRoots));
    let r = env
        .gate()
        .execute_one(&cache(Path::new("relative/target")), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAbsolute));
    let r = env.gate().execute_one(&cache(&env.root), "t");
    assert_eq!(reason(&r), Some(SkipReason::OutsideScanRoots));
    assert!(target.exists());
}

#[test]
fn target_without_cachedir_tag_is_refused() {
    let env = Env::new();
    let target = env.cargo_project("p");
    std::fs::remove_file(target.join("CACHEDIR.TAG")).unwrap();
    let r = env.gate().execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::MissingCachedirTag));
    std::fs::write(target.join("CACHEDIR.TAG"), "Signature: something else").unwrap();
    let r = env.gate().execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::MissingCachedirTag));
    let other = env.root.join("q/build");
    std::fs::create_dir_all(&other).unwrap();
    let r = env.gate().execute_one(&cache(&other), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotRecognisedCache));
    assert!(target.exists() && other.exists());
}

#[test]
fn node_modules_needs_the_flag() {
    let mut env = Env::new();
    let proj = env.root.join("web");
    std::fs::create_dir_all(proj.join("node_modules/x")).unwrap();
    std::fs::write(proj.join("package.json"), "{}").unwrap();
    std::fs::write(proj.join("node_modules/x/i.js"), "x").unwrap();
    let nm = proj.join("node_modules");
    let r = env.gate().execute_one(&cache(&nm), "t");
    assert_eq!(reason(&r), Some(SkipReason::NodeModulesDisabled));
    assert!(nm.exists());
    env.cfg.include_node_modules = true;
    assert!(done(&env.gate().execute_one(&cache(&nm), "t")));
    assert!(!nm.exists());
}

#[test]
fn busy_process_in_a_subdirectory_blocks_cache_deletion() {
    let mut env = Env::new();
    // A non-git project, outside the (fake) home directory.
    let target = env.cargo_project("plain/proj");
    let sub = target.parent().unwrap().join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    env.procs = FakeProcesses::empty().with(42, "zsh", Some(sub.clone()), 1);
    let r = env.gate().execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy), "{r:?}");
    assert!(target.exists());

    // A sibling whose name only shares a string prefix is not "under" it.
    let sibling = env.root.join("plain/proj2");
    std::fs::create_dir_all(&sibling).unwrap();
    env.procs = FakeProcesses::empty().with(42, "zsh", Some(sibling), 1);
    // Another user's process in the project does not count.
    let mut other = FakeProcesses::empty().with(43, "zsh", Some(sub), 1).procs[0].clone();
    other.uid = Some(env.procs.uid + 1);
    env.procs.procs.push(other);
    assert!(done(&env.gate().execute_one(&cache(&target), "t")));
}

#[test]
fn busy_check_reads_the_process_table_at_execution_time() {
    let mut env = Env::new();
    let target = env.cargo_project("p");
    let plan = env.gate().plan(&[cache(&target)]);
    assert!(plan.items[0].ready);
    env.procs = FakeProcesses::empty().with(7, "cargo", Some(target.parent().unwrap().into()), 1);
    let r = env.gate().execute(&plan.approved(), "t");
    assert_eq!(reason(&r[0]), Some(SkipReason::Busy));
    assert!(target.exists());
}

#[test]
fn stale_flag_from_an_old_scan_is_rechecked() {
    let mut env = Env::new();
    let target = env.cargo_project("p");
    env.now = crate::monitor::now_unix();
    // The scan said stale; the cache was used just now.
    let item = reclaim::Item {
        path: target.clone(),
        project: target.parent().unwrap().into(),
        kind: ItemKind::CargoTarget,
        bytes: 50_000,
        last_used_unix: 0,
        in_worktree: false,
        in_use: false,
        stale: true,
    };
    let plan = env.gate().plan_reclaim(std::slice::from_ref(&item), None);
    assert!(!plan.items[0].ready);
    assert_eq!(plan.items[0].reason, Some(SkipReason::NotStale));
    let r = env.gate().execute_one(
        &Request::DeleteCache {
            path: target.clone(),
            require_stale: true,
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::NotStale));
    assert!(target.exists());
    // A day later it is stale.
    env.now += 2 * DAY;
    let plan = env.gate().plan_reclaim(std::slice::from_ref(&item), None);
    assert!(plan.items[0].ready);
    assert!(done(&env.gate().execute(&plan.approved(), "t")[0]));
}

#[test]
fn reclaim_plan_stops_at_the_wanted_bytes() {
    let env = Env::new();
    let a = env.cargo_project("a");
    std::fs::write(a.join("debug/more"), vec![1u8; 200_000]).unwrap();
    let b = env.cargo_project("b");
    let report = reclaim::scan(&env.cfg, &env.paths, &[], env.now);
    assert_eq!(report.items.len(), 2);
    let plan = env.gate().plan_reclaim(&report.items, Some(1));
    assert_eq!(plan.approved().len(), 1);
    assert_eq!(plan.items[0].target, a.display().to_string());
    assert_eq!(
        env.gate().plan_reclaim(&report.items, Some(0)).items.len(),
        0
    );
    assert!(b.exists());
}

#[test]
fn derived_data_needs_the_flag_and_no_xcode_running() {
    let mut env = Env::new();
    let dd = env.paths.derived_data();
    std::fs::create_dir_all(dd.join("Proj-abc")).unwrap();
    std::fs::write(dd.join("Proj-abc/x"), "x").unwrap();
    let dd = dd.canonicalize().unwrap();
    let r = env.gate().execute_one(&cache(&dd), "t");
    assert_eq!(reason(&r), Some(SkipReason::XcodeDisabled));
    env.cfg.include_xcode = true;
    let mut xb = FakeProcesses::empty().with(9, "xcodebuild", None, 1);
    xb.procs[0].uid = Some(0);
    env.procs = xb;
    let r = env.gate().execute_one(&cache(&dd), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy));
    assert!(dd.exists());
    env.procs = FakeProcesses::empty();
    assert!(done(&env.gate().execute_one(&cache(&dd), "t")));
}

// ------------------------------------------------------------------ worktrees

#[test]
fn ignored_env_file_is_archived_and_restorable_before_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join(".env"), "SECRET=hunter2\n").unwrap();
    std::fs::write(wt.join("local.sqlite"), vec![7u8; 4096]).unwrap();
    std::fs::create_dir_all(wt.join("notes")).unwrap();
    std::fs::write(wt.join("notes/todo.md"), "remember\n").unwrap();
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    std::fs::write(wt.join("target/debug/huge"), vec![0u8; 100_000]).unwrap();
    std::fs::create_dir_all(wt.join("node_modules/x")).unwrap();
    std::fs::write(wt.join("node_modules/x/i.js"), "x").unwrap();

    let head = env.head(&wt);
    let plan = env.gate().plan(&[env.remove_req(&wt, &head)]);
    assert!(plan.items[0].ready, "{:?}", plan.items[0]);
    assert_eq!(plan.items[0].ignored_files, 3, "{:?}", plan.items[0]);

    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    assert!(done(&r), "{r:?}");
    assert!(!wt.exists());
    let archive = PathBuf::from(backup_of(&r, BackupKind::IgnoredArchive).unwrap());
    assert_eq!(std::fs::metadata(&archive).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(archive.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&env.paths.backups_dir).unwrap().mode() & 0o777,
        0o700
    );
    let restore = env.root.join("restore");
    std::fs::create_dir_all(&restore).unwrap();
    let ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&restore)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    assert_eq!(
        std::fs::read_to_string(restore.join(".env")).unwrap(),
        "SECRET=hunter2\n"
    );
    assert_eq!(
        std::fs::read_to_string(restore.join("notes/todo.md")).unwrap(),
        "remember\n"
    );
    assert!(restore.join("local.sqlite").exists());
    assert!(!restore.join("target").exists() && !restore.join("node_modules").exists());
    // The branch is kept.
    env.git(&repo, &["rev-parse", "--verify", "agent-x"]);
}

#[test]
fn ignored_data_over_the_limit_skips_the_worktree() {
    let mut env = Env::new();
    env.cfg.backup_ignored_max_mb = 1;
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("big.bin"), vec![1u8; 2 * 1024 * 1024]).unwrap();
    std::fs::write(wt.join(".env"), "A=1\n").unwrap();
    let head = env.head(&wt);
    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::IgnoredTooLarge));
    assert!(wt.join(".env").exists() && wt.join("big.bin").exists());
    assert!(
        !env.paths.backups_dir.exists(),
        "nothing written for a skip"
    );
}

#[test]
fn head_moved_since_the_plan_is_skipped() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let plan = env.gate().plan(&[env.remove_req(&wt, "")]);
    assert!(plan.items[0].ready);
    assert_eq!(plan.items[0].head.as_deref(), Some(env.head(&wt).as_str()));
    env.git(&wt, &["commit", "-q", "--allow-empty", "-m", "new work"]);
    let r = env.gate().execute(&plan.approved(), "t");
    assert_eq!(reason(&r[0]), Some(SkipReason::HeadMoved));
    assert!(wt.exists());
}

#[test]
fn removal_without_an_expected_head_is_refused() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let r = env.gate().execute_one(&env.remove_req(&wt, ""), "t");
    assert_eq!(reason(&r), Some(SkipReason::MissingExpectedHead));
    assert!(wt.exists());
}

#[test]
fn changes_made_after_the_plan_are_in_the_patch() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let plan = env.gate().plan(&[env.remove_req(&wt, "")]);
    assert!(plan.items[0].ready && plan.items[0].dirty_files == 0);
    std::fs::write(wt.join("lib.rs"), "fn main() { changed() }\n").unwrap();
    std::fs::write(wt.join("added.txt"), "brand new\n").unwrap();
    let r = env.gate().execute(&plan.approved(), "t");
    assert!(done(&r[0]), "{r:?}");
    let patch = PathBuf::from(backup_of(&r[0], BackupKind::Patch).unwrap());
    assert_eq!(std::fs::metadata(&patch).unwrap().mode() & 0o777, 0o600);
    let text = std::fs::read_to_string(&patch).unwrap();
    assert!(text.contains("changed()") && text.contains("brand new"));

    // The patch restores the work on a fresh checkout of the same commit.
    let head = match &plan.items[0].head {
        Some(h) => h.clone(),
        None => unreachable!(),
    };
    let again = env.root.join("restored");
    env.git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            again.to_str().unwrap(),
            &head,
        ],
    );
    env.git(&again, &["apply", "--index", patch.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(again.join("added.txt")).unwrap(),
        "brand new\n"
    );
}

#[test]
fn dirty_worktree_needs_allow_dirty() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("added.txt"), "x").unwrap();
    let r = env.gate().execute_one(
        &Request::RemoveWorktree {
            path: wt.clone(),
            expected_head: env.head(&wt),
            allow_dirty: false,
            require_idle: false,
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::Dirty));
    assert!(wt.join("added.txt").exists());
}

#[test]
fn detached_head_gets_a_unique_branch_that_is_never_moved() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", true);
    env.git(
        &wt,
        &["commit", "-q", "--allow-empty", "-m", "detached work"],
    );
    let first = env.head(&wt);
    let r = env.gate().execute_one(&env.remove_req(&wt, &first), "t");
    assert!(done(&r), "{r:?}");
    let b1 = backup_of(&r, BackupKind::Branch).unwrap();
    assert_eq!(b1, backup_branch_name("agent-x", &first));
    assert_eq!(env.git(&repo, &["rev-parse", &b1]).trim(), first);

    // A second worktree with the same name at another commit.
    let wt2 = env.worktree(&repo, "agent-x", true);
    env.git(&wt2, &["commit", "-q", "--allow-empty", "-m", "other work"]);
    let second = env.head(&wt2);
    let r = env.gate().execute_one(&env.remove_req(&wt2, &second), "t");
    assert!(done(&r), "{r:?}");
    let b2 = backup_of(&r, BackupKind::Branch).unwrap();
    assert_ne!(b1, b2);
    assert_eq!(
        env.git(&repo, &["rev-parse", &b1]).trim(),
        first,
        "first backup kept"
    );
    assert_eq!(env.git(&repo, &["rev-parse", &b2]).trim(), second);

    // Same commit again: the existing branch is reused, not duplicated or moved.
    let wt3 = env.worktree(&repo, "agent-x", true);
    env.git(&wt3, &["checkout", "-q", "--detach", &second]);
    let r = env.gate().execute_one(&env.remove_req(&wt3, &second), "t");
    assert_eq!(backup_of(&r, BackupKind::Branch).unwrap(), b2);
}

#[test]
fn backup_branch_name_taken_by_another_commit_skips() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", true);
    env.git(&wt, &["commit", "-q", "--allow-empty", "-m", "w"]);
    let head = env.head(&wt);
    let name = backup_branch_name("agent-x", &head);
    env.git(&repo, &["branch", &name, "main"]);
    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::BackupFailed));
    assert!(wt.exists());
    assert_eq!(
        env.git(&repo, &["rev-parse", &name]).trim(),
        env.head(&repo),
        "existing branch untouched"
    );
}

#[test]
fn backup_failure_leaves_the_worktree_intact() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    std::fs::create_dir_all(&env.paths.backups_dir).unwrap();
    std::fs::set_permissions(
        &env.paths.backups_dir,
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    std::fs::set_permissions(
        &env.paths.backups_dir,
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(reason(&r), Some(SkipReason::BackupFailed), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        "unsaved"
    );
}

#[test]
fn busy_process_in_a_worktree_subdirectory_blocks_removal() {
    let mut env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::create_dir_all(wt.join("src/a")).unwrap();
    env.procs = FakeProcesses::empty().with(5, "claude", Some(wt.join("src/a")), 1);
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy));
    assert!(wt.exists());
}

#[test]
fn sweeps_take_only_idle_agent_worktrees() {
    let mut env = Env::new();
    let repo = env.repo("repo");
    let human = env.root.join("mine");
    env.git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            human.to_str().unwrap(),
        ],
    );
    let req = |p: &Path, h: String| Request::RemoveWorktree {
        path: p.to_path_buf(),
        expected_head: h,
        allow_dirty: true,
        require_idle: true,
    };
    let r = env.gate().execute_one(&req(&human, env.head(&human)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAgent));
    let wt = env.worktree(&repo, "agent-x", false);
    env.now = crate::monitor::now_unix();
    let r = env.gate().execute_one(&req(&wt, env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotIdle));
    assert!(wt.exists() && human.exists());
}

#[test]
fn locked_and_main_worktrees_are_refused() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    env.git(&repo, &["worktree", "lock", wt.to_str().unwrap()]);
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::Locked));
    let r = env
        .gate()
        .execute_one(&env.remove_req(&repo, &env.head(&repo)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAWorktree));
    assert!(wt.exists() && repo.exists());
}

#[test]
fn a_check_that_cannot_complete_skips() {
    let env = Env::new();
    let plain = env.root.join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    let r = env.gate().execute_one(&env.remove_req(&plain, "abc"), "t");
    assert_eq!(reason(&r), Some(SkipReason::GitFailed));
    assert!(plain.exists());
}

#[test]
fn sweep_plan_from_a_scan_executes_through_the_gate() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join(".env"), "K=V\n").unwrap();
    let map = diskmap::scan(&env.cfg, &env.paths, &[], env.now);
    let plan = env.gate().plan_sweep(&map.worktrees);
    assert_eq!(plan.items.len(), 1);
    let item = &plan.items[0];
    assert!(item.ready, "{item:?}");
    assert!(item.would_backup.iter().any(|b| b.contains("ignored")));
    let journal = Journal::new(&env.paths);
    let r = env
        .gate()
        .with_journal(&journal)
        .execute(&plan.approved(), "optimizer");
    assert!(done(&r[0]), "{r:?}");
    assert!(!wt.exists());
    assert_eq!(journal.tail(5).len(), 1);
}

// ---------------------------------------------------------------------- prune

/// git's expiry for a missing worktree looks at the admin entry's `index` mtime.
fn age_admin_entry(repo: &Path, name: &str) {
    for file in ["index", "gitdir"] {
        let p = repo.join(".git/worktrees").join(name).join(file);
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(40 * DAY))
            .unwrap();
    }
}

#[test]
fn only_prunable_agent_entries_are_pruned_one_at_a_time() {
    let env = Env::new();
    let repo = env.repo("repo");
    let a = env.worktree(&repo, "agent-a", false);
    let b = env.worktree(&repo, "agent-b", false);
    let human = env.root.join("mine");
    env.git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            human.to_str().unwrap(),
        ],
    );

    // Still on disk: not prunable.
    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: a.clone(),
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::NotPrunable));

    for (i, p) in [&a, &b, &human].into_iter().enumerate() {
        env.vanish(p, i);
    }
    for n in ["agent-a", "agent-b", "mine"] {
        age_admin_entry(&repo, n);
    }
    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: human.clone(),
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::NotAgent));

    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: a.clone(),
        },
        "t",
    );
    assert!(done(&r), "{r:?}");
    let list = git::worktree_list(&env.paths, &repo).unwrap();
    assert!(!list.iter().any(|e| e.path == a));
    let eb = list.iter().find(|e| e.path == b).expect("agent-b kept");
    assert!(eb.prunable && !eb.locked, "held entries are unlocked again");
    assert!(list.iter().any(|e| e.path == human));
}

#[test]
fn a_fresh_missing_entry_is_kept_by_the_expiry() {
    let env = Env::new();
    let repo = env.repo("repo");
    let a = env.worktree(&repo, "agent-a", false);
    env.vanish(&a, 0);
    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: a.clone(),
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::PruneDeferred));
    assert!(git::worktree_list(&env.paths, &repo)
        .unwrap()
        .iter()
        .any(|e| e.path == a));
}

// -------------------------------------------------------------------- signals

fn sig(pid: u32, start: u64) -> Request {
    Request::Signal {
        pid,
        signal: Signal::Term,
        expected_start_time: start,
    }
}

#[test]
fn signals_refuse_pid1_foreign_mulchie_reused_and_gone() {
    let mut env = Env::new();
    let mut t = FakeProcesses::empty()
        .with(1, "launchd", None, 1)
        .with(500, "node", None, 100)
        .with(501, "mulchie", None, 100)
        .with(502, "Mulchie", None, 100)
        .with(503, "node", None, 100);
    t.procs[1].uid = Some(t.uid + 1);
    t.procs[4].cmd = vec!["/usr/local/bin/mulchie".into(), "mcp".into()];
    t.procs[4].name = "node".into();
    env.procs = t;
    let g = env.gate();
    assert_eq!(
        reason(&g.execute_one(&sig(1, 1), "t")),
        Some(SkipReason::ProtectedProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(0, 1), "t")),
        Some(SkipReason::ProtectedProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(500, 100), "t")),
        Some(SkipReason::ForeignProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(501, 100), "t")),
        Some(SkipReason::ProtectedProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(502, 100), "t")),
        Some(SkipReason::ProtectedProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(503, 100), "t")),
        Some(SkipReason::ProtectedProcess)
    );
    assert_eq!(
        reason(&g.execute_one(&sig(9999, 1), "t")),
        Some(SkipReason::ProcessGone)
    );
    let me = FakeProcesses {
        pid: 777,
        ..FakeProcesses::empty().with(777, "x", None, 1)
    };
    let g = Gate::new(&env.cfg, &env.paths, &me, env.now);
    assert_eq!(
        reason(&g.execute_one(&sig(777, 1), "t")),
        Some(SkipReason::ProtectedProcess)
    );
}

#[test]
fn signal_checks_start_time_then_reaches_our_own_child() {
    let mut env = Env::new();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    env.procs = FakeProcesses::empty().with(pid, "sleep", None, 1000);
    let r = env.gate().execute_one(&sig(pid, 999), "t");
    assert_eq!(reason(&r), Some(SkipReason::StartTimeMismatch));
    assert!(child.try_wait().unwrap().is_none(), "child untouched");
    let r = env.gate().execute_one(
        &Request::Signal {
            pid,
            signal: Signal::Kill,
            expected_start_time: 1000,
        },
        "t",
    );
    assert!(done(&r), "{r:?}");
    let st = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), Some(libc::SIGKILL));
}

// --------------------------------------------------------------------- misc

#[test]
fn cwd_matching_is_component_wise() {
    assert!(cwd_in(Path::new("/a/b/c"), Path::new("/a/b"), false));
    assert!(cwd_in(Path::new("/a/b"), Path::new("/a/b"), false));
    assert!(!cwd_in(Path::new("/a/bc"), Path::new("/a/b"), false));
    assert!(!cwd_in(Path::new("/a"), Path::new("/a/b"), false));
}

#[test]
fn agent_worktree_rule() {
    assert!(is_agent_worktree(Path::new("/r/.claude/worktrees/x")));
    assert!(is_agent_worktree(Path::new("/anywhere/agent-7")));
    assert!(!is_agent_worktree(Path::new("/r/feature")));
}

#[test]
fn backup_branch_names_are_safe() {
    let n = backup_branch_name("../we ird..name.lock", "0123456789abcdef");
    assert_eq!(n, "backup/we-ird.name-0123456789ab");
}

#[test]
fn skip_reasons_have_stable_codes() {
    assert_eq!(SkipReason::IgnoredTooLarge.code(), "ignored_too_large");
    let r = ActionResult {
        action: ActionKind::RemoveWorktree,
        target: "/x".into(),
        outcome: Outcome::Skipped {
            reason: SkipReason::HeadMoved,
            detail: "d".into(),
        },
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["status"], "skipped");
    assert_eq!(v["reason"], "head_moved");
    assert_eq!(v["action"], "remove_worktree");
}

// ------------------------------------------------------- P2 additions

#[test]
fn changes_during_backup_stop_the_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    let mut g = env.gate();
    let late = wt.join("late.txt");
    let late2 = late.clone();
    g.after_backup = Some(Box::new(move || std::fs::write(&late2, "new").unwrap()));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
    assert!(wt.join("work.txt").exists() && late.exists());

    // A commit made while backing up moves HEAD: also stopped.
    let mut g = env.gate();
    let wt2 = wt.clone();
    let paths = env.paths.clone();
    g.after_backup = Some(Box::new(move || {
        Git::new(&paths, &wt2)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["commit", "-q", "--allow-empty", "-m", "race"])
            .run()
            .unwrap();
    }));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::HeadMoved), "{r:?}");
    assert!(wt.exists());
}

#[test]
fn checked_out_submodules_block_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(
        wt.join(".gitmodules"),
        "[submodule \"lib\"]\n\tpath = lib\n\turl = ../lib\n",
    )
    .unwrap();
    std::fs::create_dir_all(wt.join("lib")).unwrap();
    std::fs::write(wt.join("lib/.git"), "gitdir: ../.git/modules/lib\n").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::HasSubmodules), "{r:?}");
    assert!(wt.exists());
    // Declared but not checked out: no obstacle.
    std::fs::rename(wt.join("lib/.git"), env._t.path().join("gitlink")).unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
}

#[test]
fn same_second_backups_do_not_collide() {
    let env = Env::new();
    // Two repos with the same name, each with a dirty worktree of the same
    // name, removed by one gate (one `now`): two backup sets.
    let r1 = env.repo("one/repo");
    let r2 = env.repo("two/repo");
    let w1 = env.worktree(&r1, "agent-x", false);
    let w2 = env.worktree(&r2, "agent-x", false);
    std::fs::write(w1.join("a.txt"), "first").unwrap();
    std::fs::write(w2.join("a.txt"), "second").unwrap();
    let g = env.gate();
    let res = g.execute(
        &[
            env.remove_req(&w1, &env.head(&w1)),
            env.remove_req(&w2, &env.head(&w2)),
        ],
        "t",
    );
    assert!(res.iter().all(|r| r.is_done()), "{res:?}");
    let patches: Vec<PathBuf> = res
        .iter()
        .flat_map(|r| r.backups().to_vec())
        .filter(|b| b.kind == BackupKind::Patch)
        .map(|b| PathBuf::from(b.location))
        .collect();
    assert_eq!(patches.len(), 2);
    assert_ne!(patches[0].parent(), patches[1].parent());
    let a = std::fs::read_to_string(&patches[0]).unwrap();
    let b = std::fs::read_to_string(&patches[1]).unwrap();
    assert!(a.contains("first") && b.contains("second"));
}

#[test]
fn one_process_table_read_serves_a_whole_plan() {
    let env = Env::new();
    let caches: Vec<Request> = (0..5)
        .map(|i| cache(&env.cargo_project(&format!("p{i}"))))
        .collect();
    let g = env.gate();
    let plan = g.plan(&caches);
    assert!(plan.items.iter().all(|i| i.ready), "{plan:?}");
    assert_eq!(g.process_table_reads(), 1);
}

// ------------------------------------------------- review fixes (C1–C5)

/// C1: `git init vendor` (untracked, not ignored) inside a worktree used to be
/// recorded as a gitlink patch and destroyed by `worktree remove --force`.
#[test]
fn nested_repositories_block_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let vendor = wt.join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    env.git(&vendor, &["init", "-q", "-b", "main"]);
    std::fs::write(vendor.join("a.txt"), "nested work").unwrap();
    env.git(&vendor, &["add", "-A"]);
    env.git(&vendor, &["commit", "-q", "-m", "nested"]);
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(vendor.join(".git").exists() && vendor.join("a.txt").exists());
    assert!(
        !env.paths.backups_dir.exists(),
        "nothing written for a skip"
    );

    // Deeper, inside an ignored (non-cache) directory, and as a `.git` file.
    std::fs::rename(&vendor, env._t.path().join("moved-vendor")).unwrap();
    std::fs::create_dir_all(wt.join("notes/deep/clone")).unwrap();
    std::fs::write(wt.join("notes/deep/clone/.git"), "gitdir: /elsewhere\n").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(format!("{r:?}").contains("notes/deep/clone"), "{r:?}");

    // A repo inside an ignored cache directory is a cache, as before.
    std::fs::remove_dir_all(wt.join("notes")).unwrap();
    std::fs::create_dir_all(wt.join("node_modules/dep/.git")).unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
}

#[test]
fn gitlink_patches_are_refused() {
    let p = b"diff --git a/vendor b/vendor\nnew file mode 160000\nindex 0000000..1234567\n--- /dev/null\n+++ b/vendor\n@@ -0,0 +1 @@\n+Subproject commit 1234567\n";
    assert!(patch_has_gitlink(p));
    let p = b"diff --git a/v b/v\nindex 1111111..2222222 160000\n";
    assert!(patch_has_gitlink(p));
    let p = b"diff --git a/x b/x\nnew file mode 100644\n+mode 160000 text\n";
    assert!(!patch_has_gitlink(p));
}

/// C2: skip-worktree and assume-unchanged files with local edits are invisible
/// to `status` and `add -A`; the gate used to see a clean tree.
#[test]
fn hidden_skip_worktree_and_assume_unchanged_edits_block_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    std::fs::write(repo.join("config.yml"), "a: 1\n").unwrap();
    std::fs::write(repo.join("local.yml"), "b: 1\n").unwrap();
    env.git(&repo, &["add", "-A"]);
    env.git(&repo, &["commit", "-q", "-m", "configs"]);
    let wt = env.worktree(&repo, "agent-x", false);

    // Flagged but unchanged: no obstacle to the plan.
    env.git(&wt, &["update-index", "--skip-worktree", "config.yml"]);
    env.git(&wt, &["update-index", "--assume-unchanged", "local.yml"]);
    let plan = env.gate().plan(&[env.remove_req(&wt, &env.head(&wt))]);
    assert!(plan.items[0].ready, "{:?}", plan.items[0]);

    for (file, edit) in [("config.yml", "a: 2\n"), ("local.yml", "b: 2\n")] {
        std::fs::write(wt.join(file), edit).unwrap();
        assert!(
            env.git(&wt, &["status", "--porcelain"]).trim().is_empty(),
            "git hides it"
        );
        let r = env
            .gate()
            .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
        assert_eq!(reason(&r), Some(SkipReason::HiddenChanges), "{r:?}");
        assert!(format!("{r:?}").contains(file), "{r:?}");
        assert_eq!(std::fs::read_to_string(wt.join(file)).unwrap(), edit);
        let restored = if file == "config.yml" {
            "a: 1\n"
        } else {
            "b: 1\n"
        };
        std::fs::write(wt.join(file), restored).unwrap();
    }

    // A sparse entry that is not on disk is not a change.
    std::fs::remove_file(wt.join("config.yml")).unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
}

/// C3: user diff/colour settings used to leak into `changes.patch` (ANSI codes,
/// missing or odd prefixes), so `git apply` could not restore it.
#[test]
fn user_diff_config_does_not_change_the_patch() {
    let env = Env::new();
    let repo = env.repo("repo");
    for kv in [
        ("color.ui", "always"),
        ("color.diff", "always"),
        ("diff.noprefix", "true"),
        ("diff.mnemonicPrefix", "true"),
        ("diff.srcPrefix", "SRC/"),
        ("diff.dstPrefix", "DST/"),
        ("diff.relative", "true"),
        ("apply.whitespace", "error"),
    ] {
        env.git(&repo, &["config", kv.0, kv.1]);
    }
    let wt = env.worktree(&repo, "agent-x", false);
    let head = env.head(&wt);
    std::fs::write(wt.join("lib.rs"), "fn main() { changed() }   \n").unwrap();
    std::fs::create_dir_all(wt.join("sub")).unwrap();
    std::fs::write(wt.join("sub/new.txt"), "new\n").unwrap();
    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    assert!(done(&r), "{r:?}");
    let patch = PathBuf::from(backup_of(&r, BackupKind::Patch).unwrap());
    let bytes = std::fs::read(&patch).unwrap();
    assert!(!bytes.contains(&0x1b), "no ANSI escapes");
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("diff --git a/lib.rs b/lib.rs"), "{text}");
    assert!(text.contains("+++ b/sub/new.txt"), "{text}");

    let again = env.root.join("restored");
    env.git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            again.to_str().unwrap(),
            &head,
        ],
    );
    env.git(
        &again,
        &[
            "apply",
            "--index",
            "--whitespace=nowarn",
            patch.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(again.join("sub/new.txt")).unwrap(),
        "new\n"
    );
}

/// C3: a patch that does not apply to HEAD is a failed backup.
#[test]
fn a_patch_that_does_not_verify_is_a_failed_backup() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let facts = env.gate().check_worktree(&wt, None, true, false).unwrap();
    let dir = env._t.path().join("verify");
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.patch");
    std::fs::write(
        &bad,
        "diff --git a/lib.rs b/lib.rs\n--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-not what is there\n+x\n",
    )
    .unwrap();
    let e = env.gate().verify_patch(&facts, &dir, &bad).unwrap_err();
    assert!(e.contains("does not apply"), "{e}");
    let good = dir.join("good.patch");
    std::fs::write(
        &good,
        "diff --git a/lib.rs b/lib.rs\n--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-fn main() {}\n+x\n",
    )
    .unwrap();
    env.gate().verify_patch(&facts, &dir, &good).unwrap();
}

/// C4: a second edit to an already-untracked file while the backup runs left
/// `git status` unchanged, so the removal went ahead with a stale patch.
#[test]
fn appending_to_an_untracked_file_during_backup_stops_the_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let work = wt.join("work.txt");
    std::fs::write(&work, "first\n").unwrap();
    let mut g = env.gate();
    let w2 = work.clone();
    g.after_backup = Some(Box::new(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&w2).unwrap();
        f.write_all(b"second\n").unwrap();
    }));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
    assert_eq!(std::fs::read_to_string(&work).unwrap(), "first\nsecond\n");

    // Same size, same second: only the content differs. The tree hash sees it
    // even if the stat fingerprint could not.
    let mut g = env.gate();
    let w3 = work.clone();
    g.after_backup = Some(Box::new(move || {
        let m = std::fs::metadata(&w3).unwrap().modified().unwrap();
        std::fs::write(&w3, "FIRST\nSECOND\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&w3)
            .unwrap()
            .set_modified(m)
            .unwrap();
    }));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
    assert!(wt.exists());
}

/// C4: a change to an ignored `.env` during the backup is not in the archive
/// and invisible to `git status`.
#[test]
fn changing_an_ignored_env_during_backup_stops_the_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let dotenv = wt.join(".env");
    std::fs::write(&dotenv, "A=1\n").unwrap();
    let mut g = env.gate();
    let d2 = dotenv.clone();
    g.after_backup = Some(Box::new(move || std::fs::write(&d2, "A=2\n").unwrap()));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
    assert_eq!(std::fs::read_to_string(&dotenv).unwrap(), "A=2\n");

    // A new ignored file appearing during the backup is not in the archive.
    let mut g = env.gate();
    let n = wt.join("new.sqlite");
    let n2 = n.clone();
    g.after_backup = Some(Box::new(move || std::fs::write(&n2, "db").unwrap()));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
    assert!(n.exists());

    // With nothing touched, it goes ahead.
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
}

/// C5: pruning a missing detached worktree dropped its HEAD and reflog, so
/// commits made there became unreachable.
#[test]
fn pruning_a_detached_entry_keeps_unreachable_commits_on_a_branch() {
    let env = Env::new();
    let repo = env.repo("repo");
    let a = env.worktree(&repo, "agent-a", true);
    env.git(&a, &["commit", "-q", "--allow-empty", "-m", "only here"]);
    let lost = env.head(&a);
    // A second detached entry whose HEAD main already reaches needs no branch.
    let b = env.worktree(&repo, "agent-b", true);
    env.vanish(&a, 0);
    env.vanish(&b, 1);
    age_admin_entry(&repo, "agent-a");
    age_admin_entry(&repo, "agent-b");

    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: a.clone(),
        },
        "t",
    );
    assert!(done(&r), "{r:?}");
    let branch = backup_of(&r, BackupKind::Branch).expect("a backup branch");
    assert_eq!(
        env.git(&repo, &["rev-parse", &format!("refs/heads/{branch}")])
            .trim(),
        lost
    );
    assert!(!git::worktree_list(&env.paths, &repo)
        .unwrap()
        .iter()
        .any(|e| e.path == a));

    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: b.clone(),
        },
        "t",
    );
    assert!(done(&r), "{r:?}");
    assert!(backup_of(&r, BackupKind::Branch).is_none(), "{r:?}");
}

#[test]
fn pruning_skips_when_the_backup_branch_cannot_be_made() {
    let env = Env::new();
    let repo = env.repo("repo");
    let a = env.worktree(&repo, "agent-a", true);
    env.git(&a, &["commit", "-q", "--allow-empty", "-m", "only here"]);
    let lost = env.head(&a);
    // The backup branch name is taken by another commit.
    let main = env.head(&repo);
    env.git(
        &repo,
        &["branch", &backup_branch_name("agent-a", &lost), &main],
    );
    env.vanish(&a, 0);
    age_admin_entry(&repo, "agent-a");
    let r = env.gate().execute_one(
        &Request::PruneWorktree {
            repo: repo.clone(),
            path: a.clone(),
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::BackupFailed), "{r:?}");
    assert!(git::worktree_list(&env.paths, &repo)
        .unwrap()
        .iter()
        .any(|e| e.path == a));
}
