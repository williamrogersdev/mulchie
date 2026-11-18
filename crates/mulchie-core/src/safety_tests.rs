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
    assert_eq!(SkipReason::BackingOff.code(), "backing_off");
    let r = ActionResult {
        action: ActionKind::RemoveWorktree,
        target: "/x".into(),
        outcome: Outcome::Skipped {
            reason: SkipReason::HeadMoved,
            detail: "d".into(),
        },
        journal_error: None,
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

    // R2: location never exempts a nested repository. An ignored `build/`
    // holding a repo with an unpushed commit and an untracked file blocks.
    std::fs::remove_dir_all(wt.join("notes")).unwrap();
    std::fs::write(wt.join(".gitignore"), "build/\nnode_modules/\n").unwrap();
    let nested = wt.join("build/gen");
    std::fs::create_dir_all(&nested).unwrap();
    env.git(&nested, &["init", "-q", "-b", "main"]);
    std::fs::write(nested.join("work.txt"), "unpushed").unwrap();
    env.git(&nested, &["add", "-A"]);
    env.git(&nested, &["commit", "-q", "-m", "local only"]);
    std::fs::write(nested.join("scratch.txt"), "untracked").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(nested.join("work.txt").exists());
    std::fs::remove_dir_all(wt.join("build")).unwrap();

    // A dependency cloned from a remote with nothing local is disposable...
    let upstream = env._t.path().join("upstream-dep");
    std::fs::create_dir_all(&upstream).unwrap();
    env.git(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("index.js"), "x").unwrap();
    env.git(&upstream, &["add", "-A"]);
    env.git(&upstream, &["commit", "-q", "-m", "dep"]);
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    env.git(
        &wt.join("node_modules"),
        &["clone", "-q", upstream.to_str().unwrap(), "dep"],
    );
    let dep = wt.join("node_modules/dep");
    // ...but not with a local commit that is on no remote.
    std::fs::write(dep.join("patch.js"), "local fix").unwrap();
    env.git(&dep, &["add", "-A"]);
    env.git(&dep, &["commit", "-q", "-m", "local fix"]);
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(format!("{r:?}").contains("on none of its remotes"), "{r:?}");
    env.git(&dep, &["reset", "-q", "--hard", "origin/main"]);
    // N2: the reset commit lives on in the reflog only: still blocks.
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(format!("{r:?}").contains("reflog"), "{r:?}");
    env.git(&dep, &["reflog", "expire", "--expire=now", "--all"]);
    // N2: an ignored file that is not a build cache blocks...
    std::fs::write(dep.join(".gitignore"), ".env\ndist/\n").unwrap();
    env.git(&dep, &["add", ".gitignore"]);
    env.git(&dep, &["commit", "-q", "-m", "ignore"]);
    env.git(&dep, &["push", "-q", "origin", "HEAD:refs/heads/ignore"]);
    env.git(&dep, &["fetch", "-q"]);
    std::fs::write(dep.join(".env"), "SECRET=local").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(format!("{r:?}").contains(".env"), "{r:?}");
    assert!(dep.join(".env").exists());
    // ...its own build cache does not.
    std::fs::remove_file(dep.join(".env")).unwrap();
    std::fs::create_dir_all(dep.join("dist")).unwrap();
    std::fs::write(dep.join("dist/out.js"), "built").unwrap();
    // N2: a skip-worktree edit blocks.
    env.git(&dep, &["update-index", "--skip-worktree", "index.js"]);
    std::fs::write(dep.join("index.js"), "hidden local edit").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::NestedRepo), "{r:?}");
    assert!(format!("{r:?}").contains("skip-worktree"), "{r:?}");
    env.git(&dep, &["update-index", "--no-skip-worktree", "index.js"]);
    env.git(&dep, &["checkout", "-q", "--", "index.js"]);
    // Clean, no stash, everything on the remote: removable.
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

/// C6: a journal that cannot be written no longer fails silently.
#[test]
fn journal_failures_are_reported_on_the_result() {
    let env = Env::new();
    let blocker = env._t.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let j = Journal::at(blocker.join("actions.jsonl"), 1 << 20);
    let g = env.gate().with_journal(&j);
    let r = g.execute_one(&sig(1, 0), "t");
    assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess));
    let e = r
        .journal_error
        .clone()
        .expect("journal error on the result");
    assert!(e.contains("not journaled"), "{e}");
    assert!(j.last_error().is_some());
    let v = serde_json::to_value(&r).unwrap();
    assert!(v["journal_error"].is_string());

    // A working journal: no field at all.
    let ok = Journal::at(env._t.path().join("j/actions.jsonl"), 1 << 20);
    let r = env.gate().with_journal(&ok).execute_one(&sig(1, 0), "t");
    assert!(r.journal_error.is_none());
    assert!(serde_json::to_value(&r)
        .unwrap()
        .get("journal_error")
        .is_none());
    assert!(ok.last_error().is_none());
}

/// A process table that changes after its first read.
struct SecondReadBusy {
    reads: std::sync::atomic::AtomicUsize,
    later: FakeProcesses,
}

impl ProcessSource for SecondReadBusy {
    fn list(&self) -> Vec<ProcFacts> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            Vec::new()
        } else {
            self.later.procs.clone()
        }
    }
    fn current_uid(&self) -> u32 {
        self.later.uid
    }
    fn self_pid(&self) -> u32 {
        u32::MAX
    }
}

/// C11: the cache busy check reads the process table again just before the
/// rename, not only the view used by the checks (up to 2 s old).
#[test]
fn cache_deletion_rereads_processes_right_before_acting() {
    let env = Env::new();
    let target = env.cargo_project("p");
    let procs = SecondReadBusy {
        reads: Default::default(),
        later: FakeProcesses::empty().with(7, "cargo", Some(target.parent().unwrap().into()), 1),
    };
    let g = Gate::new(&env.cfg, &env.paths, &procs, env.now);
    let r = g.execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy), "{r:?}");
    assert!(target.exists());
    assert!(procs.reads.load(Ordering::SeqCst) >= 2);
}

/// An own process whose cwd cannot be read but whose arguments name the
/// project counts as busy.
#[test]
fn unreadable_cwd_with_the_project_on_its_command_line_is_busy() {
    let mut env = Env::new();
    let target = env.cargo_project("p");
    let project = target.parent().unwrap().to_path_buf();
    let mut p = FakeProcesses::empty().with(9, "cargo", None, 1);
    p.procs[0].cmd = vec![
        "cargo".into(),
        format!("--manifest-path={}/Cargo.toml", project.display()),
    ];
    env.procs = p;
    let r = env.gate().execute_one(&cache(&target), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy), "{r:?}");
    // Unrelated arguments: not busy.
    env.procs.procs[0].cmd = vec!["cargo".into(), "/elsewhere/Cargo.toml".into()];
    assert!(done(&env.gate().execute_one(&cache(&target), "t")));
}

/// Ignored names that begin with `-` are archived as names, not tar options.
#[test]
fn ignored_names_starting_with_a_dash_are_archived() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("-rf.sqlite"), "data").unwrap();
    std::fs::write(wt.join("--checkpoint=1.sqlite"), "more").unwrap();
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
    let archive = backup_of(&r, BackupKind::IgnoredArchive).unwrap();
    let restore = env.root.join("restore");
    std::fs::create_dir_all(&restore).unwrap();
    assert!(std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&restore)
        .status()
        .unwrap()
        .success());
    assert_eq!(
        std::fs::read_to_string(restore.join("-rf.sqlite")).unwrap(),
        "data"
    );
    assert_eq!(
        std::fs::read_to_string(restore.join("--checkpoint=1.sqlite")).unwrap(),
        "more"
    );
}

/// Renice through the gate: our own child only, lowering only, start time
/// checked; protected names are refused even with the right start time.
#[test]
fn renice_requests_lower_only_and_protected_processes_are_refused() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let before = crate::engine::nice_of(pid).unwrap();
    let mut env = Env::new();
    env.procs = FakeProcesses::empty().with(pid, "sleep", None, 55);
    let req = |nice, st| Request::Renice {
        pid,
        nice,
        expected_start_time: st,
    };
    if before < 18 {
        // A CI runner can start its jobs at a negative nice value; the gate
        // only takes 0..=20, so aim at 0 or above.
        let target = (before + 2).max(0);
        let r = env.gate().execute_one(&req(target, 54), "t");
        assert_eq!(reason(&r), Some(SkipReason::StartTimeMismatch), "{r:?}");
        let r = env.gate().execute_one(&req(target, 55), "t");
        assert!(done(&r), "{r:?}");
        assert_eq!(crate::engine::nice_of(pid), Some(target));
        let r = env.gate().execute_one(&req(before, 55), "t");
        assert_eq!(
            reason(&r),
            Some(SkipReason::CommandFailed),
            "never raises priority"
        );
    }
    let r = env.gate().execute_one(&req(25, 55), "t");
    assert!(!done(&r));

    // The same pid presented as Finder: refused for signals and renice.
    env.procs.procs[0].name = "Finder".into();
    env.procs.procs[0].exe =
        Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder".into());
    let r = env.gate().execute_one(&sig(pid, 55), "t");
    assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess), "{r:?}");
    let r = env.gate().execute_one(&req(19, 55), "t");
    assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess));
    let _ = child.kill();
    let _ = child.wait();

    // Reopen: a bad bundle id never reaches `open`.
    let r = env.gate().execute_one(
        &Request::ReopenApp {
            bundle_id: "-a Calculator".into(),
            after_pid: pid,
            after_start_time: 55,
        },
        "t",
    );
    assert!(!done(&r));
}

/// R4: a reopen is carried out only after a quit of a process of that app in
/// the same batch, and it waits for the app to exit without holding the
/// execution lock.
#[cfg(target_os = "macos")]
#[test]
fn reopen_needs_a_quit_in_the_same_batch_and_waits_without_the_lock() {
    let mut env = Env::new();
    let contents = env.root.join("Foo.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        "<plist><dict><key>CFBundleIdentifier</key><string>com.example.foo</string></dict></plist>",
    )
    .unwrap();
    let exe = contents.join("MacOS/foo");
    let spawn = || {
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap()
    };
    let (mut a, mut b) = (spawn(), spawn());
    env.procs = FakeProcesses::empty()
        .with(a.id(), "foo", None, 55)
        .with(b.id(), "foo", None, 56);
    for p in &mut env.procs.procs {
        p.exe = Some(exe.clone());
    }
    let reopen = |pid: u32, st: u64, id: &str| Request::ReopenApp {
        bundle_id: id.into(),
        after_pid: pid,
        after_start_time: st,
    };

    // On its own, or with no quit before it: refused.
    let r = env
        .gate()
        .execute_one(&reopen(a.id(), 55, "com.example.foo"), "t");
    assert_eq!(reason(&r), Some(SkipReason::ReopenNotQuit), "{r:?}");
    let rs = env
        .gate()
        .execute(&[reopen(a.id(), 55, "com.example.foo")], "t");
    assert_eq!(reason(&rs[0]), Some(SkipReason::ReopenNotQuit));

    // A quit of Foo followed by a reopen of another app: refused.
    let rs = env.gate().execute(
        &[sig(a.id(), 55), reopen(a.id(), 55, "com.example.other")],
        "t",
    );
    assert!(done(&rs[0]), "{:?}", rs[0]);
    assert_eq!(
        reason(&rs[1]),
        Some(SkipReason::ReopenNotQuit),
        "{:?}",
        rs[1]
    );

    // A quit of Foo, then its reopen: the fake still lists the process, so
    // the reopen waits and times out, and the lock is free meanwhile.
    // Other tests may hold the lock for seconds; the old code held it for
    // the whole 12 s wait.
    std::thread::scope(|s| {
        let start = Instant::now();
        let h = s.spawn(|| {
            let mut g = Gate::new(&env.cfg, &env.paths, &env.procs, env.now);
            g.reopen_wait = Duration::from_secs(12);
            g.execute(
                &[sig(b.id(), 56), reopen(b.id(), 56, "com.example.foo")],
                "t",
            )
        });
        std::thread::sleep(Duration::from_millis(500));
        drop(EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
        let got = start.elapsed();
        assert!(
            got < Duration::from_secs(9),
            "the lock was held while waiting for the app to exit ({got:?})"
        );
        let rs = h.join().unwrap();
        assert!(done(&rs[0]), "{:?}", rs[0]);
        assert_eq!(reason(&rs[1]), Some(SkipReason::TimedOut), "{:?}", rs[1]);
    });
    let _ = a.wait();
    let _ = b.wait();
}

/// R6: the gate refuses to signal a direct child of `systemd --user` whose
/// working directory is in no scan root, and allows one in a workspace.
#[test]
fn user_services_of_systemd_user_are_not_signalled() {
    let mut env = Env::new();
    let ws = env.root.join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    env.procs = FakeProcesses::empty()
        .with(700, "systemd", None, 10)
        .with(701, "tracker-miner-f", Some(env.paths.home.clone()), 11)
        .with(702, "node", Some(ws), 12)
        .with(703, "node", Some(env.paths.home.clone()), 13);
    env.procs.procs[0].cmd = vec!["/usr/lib/systemd/systemd".into(), "--user".into()];
    env.procs.procs[1].ppid = 700;
    env.procs.procs[2].ppid = 700;
    let plan = env.gate().plan(&[sig(701, 11), sig(702, 12), sig(703, 13)]);
    let i = &plan.items;
    assert!(!i[0].ready, "{:?}", i[0]);
    assert_eq!(i[0].reason, Some(SkipReason::ProtectedProcess));
    assert!(i[0].verdict.contains("systemd --user"), "{}", i[0].verdict);
    assert!(i[1].ready, "in a workspace: {:?}", i[1]);
    assert!(i[2].ready, "another parent: {:?}", i[2]);
}

/// R13: a patch that does not apply to HEAD is a failed backup, through the
/// whole removal: nothing is removed and the changes stay in place.
#[test]
fn a_patch_that_does_not_verify_fails_the_removal() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    let head = env.head(&wt);
    let mut g = env.gate();
    g.before_verify = Some(Box::new(|p: &Path| {
        // A hunk against a file HEAD does not have.
        std::fs::write(
            p,
            "diff --git a/missing.txt b/missing.txt\n--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
    }));
    let r = g.execute_one(&env.remove_req(&wt, &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::BackupFailed), "{r:?}");
    let Outcome::Skipped { detail, .. } = &r.outcome else {
        unreachable!()
    };
    assert!(detail.contains("does not apply"), "{detail}");
    assert!(wt.exists());
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        "unsaved"
    );
    // Without the corruption the same removal goes through.
    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    assert!(done(&r), "{r:?}");
    assert!(!wt.exists());
}

// ------------------------------------------------- audit additions (gate)

fn detail(r: &ActionResult) -> String {
    match &r.outcome {
        Outcome::Skipped { detail, .. } => detail.clone(),
        _ => String::new(),
    }
}

fn backup_sets(env: &Env) -> usize {
    std::fs::read_dir(&env.paths.backups_dir)
        .map(|d| d.count())
        .unwrap_or(0)
}

/// A removal that is skipped after its backup was written did not happen, so
/// the backup set is discarded: a sweep that keeps hitting the same skip must
/// not add a patch and an archive on every scan.
#[test]
fn a_skip_after_the_backup_discards_the_unused_backup_set() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    std::fs::write(wt.join(".env"), "SECRET=1").unwrap();
    for i in 0..3 {
        let mut g = env.gate();
        let late = wt.join(format!("late-{i}.txt"));
        g.after_backup = Some(Box::new(move || std::fs::write(&late, "new").unwrap()));
        let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
        assert_eq!(reason(&r), Some(SkipReason::ChangedDuringBackup), "{r:?}");
        assert!(detail(&r).contains("discarded"), "{}", detail(&r));
        assert_eq!(backup_sets(&env), 0, "try {i} left a backup set behind");
    }
    assert_eq!(
        std::fs::read_to_string(wt.join("work.txt")).unwrap(),
        "unsaved"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(".env")).unwrap(),
        "SECRET=1"
    );
    // The same worktree, left alone, is then removed with its backups kept.
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert!(done(&r), "{r:?}");
    assert_eq!(backup_sets(&env), 1);
    assert!(Path::new(&backup_of(&r, BackupKind::Patch).unwrap()).exists());
}

/// A backup set that could not be completed is not left half written.
#[test]
fn a_half_written_backup_set_is_discarded() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    let mut g = env.gate();
    g.before_verify = Some(Box::new(|p: &Path| {
        std::fs::write(
            p,
            "diff --git a/missing.txt b/missing.txt\n--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
    }));
    let r = g.execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::BackupFailed), "{r:?}");
    assert_eq!(backup_sets(&env), 0);
    assert!(wt.join("work.txt").exists());
}

/// Once `git worktree remove` has started, files may be gone even when it
/// fails: the backups stay and the result says where they are.
#[test]
fn a_removal_that_fails_partway_keeps_and_names_its_backups() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
    let head = env.head(&wt);
    // The directory holding the worktree cannot be written: git cannot
    // remove the worktree's own directory.
    let parent = wt.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let r = env.gate().execute_one(&env.remove_req(&wt, &head), "t");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(reason(&r), Some(SkipReason::RemoveFailed), "{r:?}");
    let d = detail(&r);
    assert!(d.contains("its backups are kept in"), "{d}");
    assert_eq!(backup_sets(&env), 1);
    let set = std::fs::read_dir(&env.paths.backups_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(d.contains(set.to_str().unwrap()), "{d}");
    let patch = std::fs::read_to_string(set.join("changes.patch")).unwrap();
    assert!(
        patch.contains("unsaved"),
        "the uncommitted file is in the patch"
    );
}

/// The path rules of a worktree removal, each refused before git is asked
/// to change anything.
#[test]
fn worktree_paths_that_are_relative_missing_links_or_files_are_refused() {
    let env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let head = env.head(&wt);
    let g = env.gate();
    let r = g.execute_one(&env.remove_req(Path::new("repo/agent-x"), &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAbsolute));
    let r = g.execute_one(&env.remove_req(&env.root.join("nowhere"), &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotFound));
    // A symlink to a real, removable agent worktree is not followed.
    let link = env.root.join("agent-link");
    std::os::unix::fs::symlink(&wt, &link).unwrap();
    let r = g.execute_one(&env.remove_req(&link, &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::Symlink));
    let file = env.root.join("agent-file");
    std::fs::write(&file, "x").unwrap();
    let r = g.execute_one(&env.remove_req(&file, &head), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAWorktree));
    assert!(wt.join("lib.rs").exists() && link.exists() && file.exists());
}

#[test]
fn a_worktree_outside_every_scan_root_is_refused() {
    let mut env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let elsewhere = env._t.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    env.cfg.scan_roots = vec![elsewhere.canonicalize().unwrap()];
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::OutsideScanRoots));
    assert!(wt.exists());
}

/// An agent that asks for its own worktree to be removed is refused while
/// anything of it (its MCP server included) still works there.
#[test]
fn the_callers_own_mulchie_process_in_a_worktree_keeps_it() {
    let mut env = Env::new();
    let repo = env.repo("repo");
    let wt = env.worktree(&repo, "agent-x", false);
    let mut t = FakeProcesses::empty().with(40, "mulchie", Some(wt.clone()), 1);
    t.procs[0].cmd = vec!["/opt/homebrew/bin/mulchie".into(), "mcp".into()];
    env.procs = t;
    let r = env
        .gate()
        .execute_one(&env.remove_req(&wt, &env.head(&wt)), "t");
    assert_eq!(reason(&r), Some(SkipReason::Busy), "{r:?}");
    assert!(wt.exists());
}

#[test]
fn prune_refuses_relative_missing_outside_unlisted_and_locked_entries() {
    let mut env = Env::new();
    let repo = env.repo("repo");
    let a = env.worktree(&repo, "agent-a", false);
    let prune = |repo: &Path, path: &Path| Request::PruneWorktree {
        repo: repo.to_path_buf(),
        path: path.to_path_buf(),
    };
    let g = env.gate();
    let r = g.execute_one(&prune(Path::new("repo"), &a), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAbsolute));
    let r = g.execute_one(&prune(&repo, Path::new("agent-a")), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAbsolute));
    let r = g.execute_one(&prune(&env.root.join("no-repo"), &a), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotFound));
    let r = g.execute_one(&prune(&repo, &env.root.join("agent-unlisted")), "t");
    assert_eq!(reason(&r), Some(SkipReason::NotAWorktree));

    // A locked entry whose directory is gone stays: the lock is the user's.
    // (git does not report a locked entry as prunable, so either check may
    // be the one that answers.)
    env.git(&repo, &["worktree", "lock", a.to_str().unwrap()]);
    env.vanish(&a, 0);
    age_admin_entry(&repo, "agent-a");
    let r = env.gate().execute_one(&prune(&repo, &a), "t");
    assert!(
        matches!(
            reason(&r),
            Some(SkipReason::Locked | SkipReason::NotPrunable)
        ),
        "{r:?}"
    );
    assert!(git::worktree_list(&env.paths, &repo)
        .unwrap()
        .iter()
        .any(|e| e.path == a && e.locked));
    env.git(&repo, &["worktree", "unlock", a.to_str().unwrap()]);

    let elsewhere = env._t.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    env.cfg.scan_roots = vec![elsewhere.canonicalize().unwrap()];
    let r = env.gate().execute_one(&prune(&repo, &a), "t");
    assert_eq!(reason(&r), Some(SkipReason::OutsideScanRoots));
    assert!(git::worktree_list(&env.paths, &repo)
        .unwrap()
        .iter()
        .any(|e| e.path == a));
}

/// Whatever started the engine (its shell, terminal or service manager) is
/// never signalled, and neither is a pid that does not fit a positive i32
/// (`kill` would read it as a process group or as "everything").
#[test]
fn signals_refuse_the_engines_ancestors_and_out_of_range_pids() {
    let mut env = Env::new();
    let mut t = FakeProcesses::empty()
        .with(900, "runner", None, 10)
        .with(800, "wrapper", None, 10)
        .with(700, "starter", None, 10)
        .with(600, "bystander", None, 10);
    t.pid = 900;
    t.procs[0].ppid = 800;
    t.procs[1].ppid = 700;
    env.procs = t;
    let g = env.gate();
    for pid in [800, 700] {
        let r = g.execute_one(&sig(pid, 10), "t");
        assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess), "{r:?}");
        assert!(detail(&r).contains("started Mulchie"), "{}", detail(&r));
    }
    for pid in [i32::MAX as u32 + 1, u32::MAX] {
        let r = g.execute_one(&sig(pid, 10), "t");
        assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess), "{r:?}");
    }
    // Renice goes through the same checks.
    let r = g.execute_one(
        &Request::Renice {
            pid: 800,
            nice: 10,
            expected_start_time: 10,
        },
        "t",
    );
    assert_eq!(reason(&r), Some(SkipReason::ProtectedProcess), "{r:?}");
}

/// A plan over many requests is checked by several threads: the items come
/// back in the order asked, each with the verdict a single check gives, and
/// the process table is still read once.
#[test]
fn a_large_plan_is_checked_in_parallel_with_the_same_verdicts_in_order() {
    let env = Env::new();
    let repo = env.repo("repo");
    let mut reqs = Vec::new();
    for i in 0..10 {
        let wt = env.worktree(&repo, &format!("agent-{i}"), i % 4 == 3);
        match i % 5 {
            1 => std::fs::write(wt.join("work.txt"), "unsaved").unwrap(),
            2 => {
                env.git(&repo, &["worktree", "lock", wt.to_str().unwrap()]);
            }
            3 => std::fs::write(wt.join(".env"), "SECRET=1").unwrap(),
            _ => {}
        }
        reqs.push(Request::RemoveWorktree {
            path: wt,
            expected_head: String::new(),
            // Odd ones refuse uncommitted changes.
            allow_dirty: i % 2 == 0,
            require_idle: false,
        });
    }
    reqs.push(cache(&env.cargo_project("p")));
    reqs.push(env.remove_req(&env.root.join("nowhere"), "abc"));
    assert!(reqs.len() > PLAN_WORKERS);

    let g = env.gate();
    let plan = g.plan(&reqs);
    assert_eq!(g.process_table_reads(), 1);
    assert_eq!(plan.items.len(), reqs.len());
    let one_by_one = env.gate();
    for (item, req) in plan.items.iter().zip(&reqs) {
        let alone = one_by_one.plan_item(req);
        assert_eq!(item.target, req.target());
        assert_eq!(
            serde_json::to_value(item).unwrap(),
            serde_json::to_value(&alone).unwrap(),
            "{}",
            item.target
        );
    }
    let ready = plan.items.iter().filter(|i| i.ready).count();
    assert!(ready >= 4 && ready < reqs.len(), "{plan:?}");
    assert_eq!(
        plan.ready_bytes,
        plan.items
            .iter()
            .filter(|i| i.ready)
            .map(|i| i.bytes)
            .sum::<u64>()
    );
    // Planning changed nothing.
    assert_eq!(backup_sets(&env), 0);
    assert!(reqs
        .iter()
        .take(10)
        .all(|r| Path::new(&r.target()).exists()));
}

/// Not a pass or fail check: prints how long a plan over many worktrees
/// takes, one at a time and as `Gate::plan` does it.
/// `cargo test -p mulchie-core plan_time -- --ignored --nocapture`
#[test]
#[ignore = "timing only"]
fn plan_time_over_many_worktrees() {
    let env = Env::new();
    let repo = env.repo("repo");
    for i in 0..200 {
        std::fs::write(repo.join(format!("f{i}.rs")), "fn f() {}\n").unwrap();
    }
    env.git(&repo, &["add", "-A"]);
    env.git(&repo, &["commit", "-q", "-m", "files"]);
    let reqs: Vec<Request> = (0..32)
        .map(|i| {
            let wt = env.worktree(&repo, &format!("agent-{i}"), false);
            if i % 2 == 0 {
                std::fs::write(wt.join("work.txt"), "unsaved").unwrap();
            }
            Request::RemoveWorktree {
                path: wt,
                expected_head: String::new(),
                allow_dirty: true,
                require_idle: false,
            }
        })
        .collect();
    let g = env.gate();
    let t = Instant::now();
    let serial: Vec<PlanItem> = reqs.iter().map(|r| g.plan_item(r)).collect();
    let serial_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let plan = g.plan(&reqs);
    let parallel_ms = t.elapsed().as_millis();
    assert!(serial.iter().all(|i| i.ready) && plan.items.iter().all(|i| i.ready));
    println!(
        "plan over {} worktrees: {serial_ms} ms one at a time, {parallel_ms} ms with {PLAN_WORKERS} workers",
        reqs.len()
    );
}
