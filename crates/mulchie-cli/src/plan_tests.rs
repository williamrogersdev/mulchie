use super::*;
use mulchie_core::safety::{SkipReason, CACHEDIR_TAG_SIGNATURE};
use std::path::Path;
use std::time::{Duration, SystemTime};

const OLD: &str = "2020-01-02T03:04:05Z";

/// A temp home with a scan root, a config pointing at it, and helpers.
struct Env {
    _t: tempfile::TempDir,
    paths: Paths,
    root: PathBuf,
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
            worktree_idle_days: 1,
            include_xcode: false,
            ..Config::default()
        };
        cfg.save_to(&paths, 0).unwrap();
        Self { _t: t, paths, root }
    }

    fn engine(&self) -> Engine {
        Engine::new(
            self.paths.clone(),
            crate::commands::safe_test_config(&self.paths),
        )
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .env("HOME", &self.paths.home)
            .env_remove("XDG_CONFIG_HOME")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", OLD)
            .env("GIT_COMMITTER_DATE", OLD)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo whose only commit is years old, with idle agent worktrees.
    fn repo_with_worktrees(&self, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let repo = self.root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        self.git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("lib.rs"), "fn main() {}\n").unwrap();
        self.git(&repo, &["add", "-A"]);
        self.git(&repo, &["commit", "-q", "-m", "init"]);
        let wts: Vec<PathBuf> = names
            .iter()
            .map(|n| {
                let p = repo.join(".claude/worktrees").join(n);
                self.git(
                    &repo,
                    &["worktree", "add", "-q", "-b", n, p.to_str().unwrap()],
                );
                p
            })
            .collect();
        (repo, wts)
    }

    fn cache(&self, rel: &str) -> PathBuf {
        let dir = self.root.join(rel);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
        std::fs::write(dir.join("target/debug/blob"), vec![1u8; 20_000]).unwrap();
        dir.join("target")
    }
}

/// Set every file under `dir` to a month ago.
fn age(dir: &Path) {
    let then = SystemTime::now() - Duration::from_secs(40 * 86_400);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                std::fs::File::options()
                    .write(true)
                    .open(e.path())
                    .unwrap()
                    .set_modified(then)
                    .unwrap();
            }
        }
    }
}

struct Captured {
    code: i32,
    out: String,
    err: String,
    asked: Vec<String>,
}

fn go(
    backend: &Backend,
    job: &Job,
    confirm: Confirm,
    json: bool,
    interactive: bool,
    mut answer: impl FnMut() -> bool,
) -> Captured {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut asked = Vec::new();
    let mut ask = |q: &str| {
        asked.push(q.to_string());
        answer()
    };
    let code = {
        let mut ui = Ui {
            confirm,
            json,
            interactive,
            ask: &mut ask,
            out: &mut out,
            err: &mut err,
        };
        run(backend, job, &mut ui)
    };
    Captured {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        asked,
    }
}

fn yes() -> Confirm {
    Confirm {
        yes: true,
        dry_run: false,
    }
}

fn dry() -> Confirm {
    Confirm {
        yes: false,
        dry_run: true,
    }
}

#[test]
fn sweep_locally_dry_run_then_execute() {
    let env = Env::new();
    let (_repo, wts) = env.repo_with_worktrees(&["agent-a", "agent-b"]);
    std::fs::write(wts[1].join("notes.txt"), "uncommitted\n").unwrap();
    for w in &wts {
        age(w);
    }
    let backend = Backend::Local(env.engine());

    let c = go(
        &backend,
        &Job::Sweep,
        dry(),
        false,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 0, "{}{}", c.out, c.err);
    assert!(c.out.contains("Plan: remove 2 worktree(s)"), "{}", c.out);
    assert!(c.out.contains("repo › agent-b"), "{}", c.out);
    assert!(c.out.contains("saves patch"), "{}", c.out);
    assert!(wts.iter().all(|w| w.exists()));

    // Not a terminal and no --yes: refuse, change nothing.
    let c = go(
        &backend,
        &Job::Sweep,
        Confirm::default(),
        false,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 2);
    assert!(c.err.contains("--yes"), "{}", c.err);
    assert!(wts.iter().all(|w| w.exists()));

    // A terminal, answered no.
    let c = go(
        &backend,
        &Job::Sweep,
        Confirm::default(),
        false,
        true,
        || false,
    );
    assert_eq!(c.code, 0);
    assert_eq!(c.asked.len(), 1);
    assert!(
        c.asked[0].starts_with("Remove these 2 worktree(s)"),
        "{:?}",
        c.asked
    );
    assert!(wts.iter().all(|w| w.exists()));

    // --yes executes.
    let c = go(
        &backend,
        &Job::Sweep,
        yes(),
        false,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 0, "{}{}", c.out, c.err);
    assert!(wts.iter().all(|w| !w.exists()));
    assert!(c.out.contains("2 done, 0 skipped"), "{}", c.out);
    assert!(c.out.contains("patch saved to"), "{}", c.out);
}

#[test]
fn sweep_executes_the_plan_it_showed_through_the_engine() {
    let env = Env::new();
    let (_repo, wts) = env.repo_with_worktrees(&["agent-a", "agent-b"]);
    for w in &wts {
        age(w);
    }
    let engine = env.engine();
    let mut lk = engine.acquire_lock().unwrap();
    let server = mulchie_core::api::serve(engine.clone(), 0).unwrap();
    lk.set_port(Some(server.port())).unwrap();
    let Conn::Engine(client) = conn::connect(&env.paths) else {
        panic!("no engine")
    };
    let backend = Backend::Engine(client);

    // While the user reads the plan, agent-a gets a new commit: its HEAD no
    // longer matches the plan, so it must be skipped, not removed.
    let a = wts[0].clone();
    let c = go(
        &backend,
        &Job::Sweep,
        Confirm::default(),
        true,
        true,
        || {
            std::fs::write(a.join("new.rs"), "x").unwrap();
            env.git(&a, &["add", "-A"]);
            env.git(&a, &["commit", "-q", "-m", "more"]);
            true
        },
    );
    assert_eq!(c.code, 1, "one item was skipped: {}{}", c.out, c.err);
    let v: serde_json::Value = serde_json::from_str(&c.out).unwrap();
    assert_eq!(v["source"], "engine");
    assert_eq!(v["executed"], true);
    let planned: Vec<&str> = v["plan"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["ready"] == true)
        .map(|i| i["target"].as_str().unwrap())
        .collect();
    let results: Vec<ActionResult> = serde_json::from_value(v["results"].clone()).unwrap();
    let executed: Vec<&str> = results.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(planned, executed, "exactly the planned items, in order");
    let ra = results
        .iter()
        .find(|r| Path::new(&r.target) == a.canonicalize().unwrap())
        .unwrap();
    assert_eq!(ra.reason(), Some(SkipReason::HeadMoved));
    assert!(a.exists());
    assert!(!wts[1].exists());
    // The human-readable plan went to stderr, so stdout stayed pure JSON.
    assert!(c.err.contains("Plan: remove 2 worktree(s)"), "{}", c.err);
    server.stop();
}

#[test]
fn clean_paths_through_the_engine_and_locally() {
    let env = Env::new();
    let a = env.cache("a");
    let b = env.cache("b");
    let not_cache = env.root.join("docs");
    std::fs::create_dir_all(&not_cache).unwrap();
    let job = Job::CleanPaths(vec![a.clone(), not_cache.clone()]);

    let engine = env.engine();
    let mut lk = engine.acquire_lock().unwrap();
    let server = mulchie_core::api::serve(engine.clone(), 0).unwrap();
    lk.set_port(Some(server.port())).unwrap();
    let Conn::Engine(client) = conn::connect(&env.paths) else {
        panic!("no engine")
    };
    let remote = Backend::Engine(client);

    let c = go(&remote, &job, dry(), false, false, || unreachable!());
    assert_eq!(c.code, 0);
    assert!(c.out.contains("Plan: delete 1 cache(s)"), "{}", c.out);
    assert!(c.out.contains("Not ready (1)"), "{}", c.out);
    assert!(c.out.contains("[not_recognised_cache]"), "{}", c.out);
    assert!(a.exists());

    let c = go(&remote, &job, yes(), false, false, || unreachable!());
    assert_eq!(c.code, 0, "{}{}", c.out, c.err);
    assert!(!a.exists());
    assert!(b.exists(), "not asked for");
    assert!(not_cache.exists());
    assert!(c.out.contains("1 done, 0 skipped"), "{}", c.out);
    server.stop();
    drop(lk);

    // The same flow with a local engine.
    let local = Backend::Local(env.engine());
    let c = go(
        &local,
        &Job::CleanPaths(vec![b.clone()]),
        yes(),
        true,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 0, "{}{}", c.out, c.err);
    let v: serde_json::Value = serde_json::from_str(&c.out).unwrap();
    assert_eq!(v["source"], "local");
    assert_eq!(v["results"][0]["status"], "done");
    assert!(!b.exists());
}

#[test]
fn empty_plans_do_nothing() {
    let env = Env::new();
    let local = Backend::Local(env.engine());
    let c = go(
        &local,
        &Job::Sweep,
        Confirm::default(),
        false,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 0);
    assert!(c.out.contains("No idle agent worktrees"), "{}", c.out);
    let c = go(
        &local,
        &Job::CleanStale,
        Confirm::default(),
        true,
        false,
        || unreachable!(),
    );
    assert_eq!(c.code, 0);
    let v: serde_json::Value = serde_json::from_str(&c.out).unwrap();
    assert_eq!(v["executed"], false);
}

#[test]
fn local_execute_holds_the_engine_lock() {
    let env = Env::new();
    let a = env.cache("a");
    let job = Job::CleanPaths(vec![a.clone()]);
    let local = Backend::Local(env.engine());
    let plan = local.plan(&job).unwrap();
    // An engine that started after the plan was made holds the lock.
    let other = env.engine();
    let held = other.acquire_lock().unwrap();
    let err = local.execute(&job, &plan.approved()).unwrap_err();
    assert!(err.contains("nothing was changed"), "{err}");
    assert!(a.exists());
    drop(held);
    let r = local.execute(&job, &plan.approved()).unwrap();
    assert!(r[0].is_done(), "{r:?}");
    assert!(!a.exists());
    // Released afterwards.
    drop(other.acquire_lock().unwrap());
}
