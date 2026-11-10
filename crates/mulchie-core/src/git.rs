//! The one way Mulchie runs `git`.
//!
//! Scanning a repository must never run code from that repository. Repo-local
//! config can execute commands from these keys, verified against git 2.56:
//!
//! | key | runs on |
//! | --- | --- |
//! | `core.fsmonitor` | `status`, `ls-files`, `add`, `diff` |
//! | `filter.<name>.clean` / `.smudge` / `.process` (via attributes) | `add`, `diff --cached`, `status` (stat-dirty files) |
//! | `diff.<driver>.command`, `diff.<driver>.textconv`, `diff.external` | `diff` |
//! | hooks (`core.hooksPath`, `.git/hooks`) | `branch` (reference-transaction), `worktree add` |
//! | `core.pager` | any command on a terminal |
//! | `log.showSignature` + `gpg.program` | `log` |
//!
//! Every call gets `-c core.fsmonitor=false -c core.hooksPath=/dev/null
//! -c log.showSignature=false -c core.pager=cat`, an empty `clean`, `smudge` and
//! `process` for every filter driver configured for that directory, and
//! `--no-ext-diff --no-textconv` on `diff`. `core.sshCommand`, `credential.helper`
//! and `core.askPass` only run on network or credential operations, which Mulchie
//! never performs; `GIT_TERMINAL_PROMPT=0` covers prompts.
//!
//! `-c` settings reach child git processes (for example the `status` that
//! `worktree remove` runs) through `GIT_CONFIG_PARAMETERS`.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::paths::Paths;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// At most this many git processes run at once in this process.
pub const MAX_CONCURRENT: usize = 8;

const FIXED_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "log.showSignature=false",
    "core.pager=cat",
    // Output we parse or store (patches) must not depend on user taste.
    "color.ui=false",
    "color.diff=false",
    "color.status=false",
    "diff.noprefix=false",
    "diff.mnemonicPrefix=false",
    "diff.srcPrefix=a/",
    "diff.dstPrefix=b/",
    "diff.relative=false",
    "diff.suppressBlankEmpty=false",
];

/// Inherited variables that would redirect git away from the directory we name.
const CLEARED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_EXTERNAL_DIFF",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "GIT_EDITOR",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// git could not be started.
    Spawn(String),
    /// git ran past its deadline and was killed.
    Timeout(Duration),
    /// git exited unsuccessfully.
    Failed { code: Option<i32>, stderr: String },
    /// The command wrote more to stdout than [`MAX_STDOUT`]; nothing is
    /// returned rather than a truncated result (patches come from stdout).
    OutputTooLarge(usize),
    /// The command exited but its output stayed open (a background process
    /// outside its process group kept it), so the output may be incomplete.
    OutputOpen,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "could not run git: {e}"),
            GitError::Timeout(d) => write!(f, "git timed out after {}s", d.as_secs()),
            GitError::Failed { code, stderr } => match code {
                Some(c) => write!(f, "git exited with {c}: {stderr}"),
                None => write!(f, "git was killed: {stderr}"),
            },
            GitError::OutputTooLarge(n) => {
                write!(f, "the command wrote more than {} MiB of output", n >> 20)
            }
            GitError::OutputOpen => write!(
                f,
                "the command exited but a background process kept its output open"
            ),
        }
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }
}

/// A git invocation in `dir`. Build with [`Git::new`], run with [`Git::run`].
pub struct Git<'a> {
    paths: &'a Paths,
    dir: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl<'a> Git<'a> {
    pub fn new(paths: &'a Paths, dir: &Path) -> Self {
        Self {
            paths,
            dir: dir.to_path_buf(),
            args: Vec::new(),
            env: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    pub fn env(mut self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> Self {
        self.env
            .push((k.as_ref().to_os_string(), v.as_ref().to_os_string()));
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Run and require exit status 0.
    pub fn run(self) -> Result<GitOutput, GitError> {
        let _permit = Permit::acquire();
        let filters = filter_drivers(self.paths, &self.dir, self.timeout);
        let mut cmd = base_command(self.paths, &self.dir);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for f in &filters {
            for key in ["clean", "smudge", "process"] {
                cmd.arg("-c").arg(format!("filter.{f}.{key}="));
            }
            cmd.arg("-c").arg(format!("filter.{f}.required=false"));
        }
        let is_diff = self.args.first().map(|a| a == "diff").unwrap_or(false);
        let mut args = self.args.into_iter();
        if is_diff {
            cmd.arg(args.next().unwrap_or_default());
            cmd.args(["--no-ext-diff", "--no-textconv"]);
        }
        cmd.args(args);
        let out = run_with_timeout(cmd, self.timeout)?;
        if out.success {
            Ok(GitOutput {
                stdout: out.stdout,
                stderr: out.stderr,
            })
        } else {
            Err(GitError::Failed {
                code: out.code,
                stderr: out.stderr.trim().to_string(),
            })
        }
    }

    /// Run and return stdout as text.
    pub fn text(self) -> Result<String, GitError> {
        self.run().map(|o| o.text())
    }
}

/// `git -C dir args...`, stdout as text.
pub fn git(paths: &Paths, dir: &Path, args: &[&str]) -> Result<String, GitError> {
    Git::new(paths, dir).args(args).text()
}

fn base_command(paths: &Paths, dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    for k in CLEARED_ENV {
        cmd.env_remove(k);
    }
    cmd.env("HOME", &paths.home)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("LC_ALL", "C");
    match &paths.xdg_config_home {
        Some(x) => cmd.env("XDG_CONFIG_HOME", x),
        None => cmd.env_remove("XDG_CONFIG_HOME"),
    };
    cmd.arg("-C").arg(dir);
    for c in FIXED_CONFIG {
        cmd.arg("-c").arg(c);
    }
    cmd
}

/// Names of filter drivers configured for `dir` (any scope). Reading config runs
/// no repository code.
fn filter_drivers(paths: &Paths, dir: &Path, timeout: Duration) -> Vec<String> {
    let mut cmd = base_command(paths, dir);
    cmd.args([
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        r"^filter\.",
    ]);
    let Ok(out) = run_with_timeout(cmd, timeout) else {
        return Vec::new();
    };
    let mut names: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|k| {
            let k = std::str::from_utf8(k).ok()?.trim();
            let rest = k.strip_prefix("filter.")?;
            let (name, _key) = rest.rsplit_once('.')?;
            (!name.is_empty() && !name.contains(['\n', '='])).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

pub struct ProcOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Captured stdout above this is an error ([`GitError::OutputTooLarge`]).
pub const MAX_STDOUT: usize = 512 << 20;
/// Captured stderr keeps its first this many bytes.
pub const MAX_STDERR: usize = 1 << 20;
/// After the command exits, how long its output may stay open before the
/// process group is killed.
const READER_GRACE: Duration = Duration::from_secs(2);

/// Read `src` to its end on a thread, keeping at most `cap` bytes; the
/// buffer and whether more was written arrive on the channel.
fn spawn_reader<R: Read + Send + 'static>(
    src: Option<R>,
    cap: usize,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let mut over = false;
        if let Some(mut s) = src {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) if b.len() + n <= cap => b.extend_from_slice(&buf[..n]),
                    Ok(n) => {
                        let room = cap - b.len();
                        b.extend_from_slice(&buf[..room.min(n)]);
                        over = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        let _ = tx.send((b, over));
    });
    rx
}

/// Run `cmd` in its own process group with stdout and stderr captured. Past
/// `timeout` the whole group is killed and [`GitError::Timeout`] returned.
/// Reading the output never waits without bound: once the command exits, a
/// group member still holding it open is killed after [`READER_GRACE`].
pub fn run_with_timeout(cmd: Command, timeout: Duration) -> Result<ProcOutput, GitError> {
    run_capped(cmd, timeout, MAX_STDOUT)
}

fn run_capped(
    mut cmd: Command,
    timeout: Duration,
    max_stdout: usize,
) -> Result<ProcOutput, GitError> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| GitError::Spawn(e.to_string()))?;
    let out_rx = spawn_reader(child.stdout.take(), max_stdout);
    let err_rx = spawn_reader(child.stderr.take(), MAX_STDERR);
    let deadline = Instant::now() + timeout;
    let mut nap = Duration::from_millis(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => {
                std::thread::sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(50));
            }
            Err(e) => {
                kill_group(child.id());
                let _ = child.wait();
                return Err(GitError::Spawn(e.to_string()));
            }
        }
    };
    let Some(status) = status else {
        kill_group(child.id());
        let _ = child.wait();
        // The readers end when the group is gone; one held open by a
        // process outside the group is left behind, not waited for.
        return Err(GitError::Timeout(timeout));
    };
    // The leader has exited; a group member may still hold the pipes.
    let pid = child.id();
    let grace = Instant::now() + READER_GRACE;
    let mut after_kill: Option<Instant> = None;
    let mut collect = |rx: &std::sync::mpsc::Receiver<(Vec<u8>, bool)>| {
        let left = grace.saturating_duration_since(Instant::now());
        if let Ok(v) = rx.recv_timeout(left) {
            return Some(v);
        }
        let end = *after_kill.get_or_insert_with(|| {
            kill_group(pid);
            Instant::now() + Duration::from_millis(500)
        });
        rx.recv_timeout(end.saturating_duration_since(Instant::now()))
            .ok()
    };
    let out = collect(&out_rx);
    let err = collect(&err_rx);
    let (Some((stdout, out_over)), Some((err_bytes, _))) = (out, err) else {
        return Err(GitError::OutputOpen);
    };
    if out_over {
        return Err(GitError::OutputTooLarge(max_stdout));
    }
    let stderr = String::from_utf8_lossy(&err_bytes).to_string();
    Ok(ProcOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

/// SIGKILL the process group led by our own child `pid`.
fn kill_group(pid: u32) {
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

struct Pool {
    used: Mutex<usize>,
    cv: Condvar,
}

static POOL: Pool = Pool {
    used: Mutex::new(0),
    cv: Condvar::new(),
};

struct Permit;

impl Permit {
    fn acquire() -> Self {
        let mut used = POOL.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used >= MAX_CONCURRENT {
            used = POOL.cv.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = POOL.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(1);
        POOL.cv.notify_one();
    }
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short branch name (`refs/heads/` stripped).
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// Parse `git worktree list --porcelain`. The first entry is the main worktree.
pub fn parse_worktree_list(s: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for line in s.lines() {
        if line.is_empty() {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        }
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        if k == "worktree" {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            cur = Some(WorktreeEntry {
                path: PathBuf::from(v),
                ..Default::default()
            });
            continue;
        }
        let Some(e) = cur.as_mut() else { continue };
        match k {
            "HEAD" => e.head = Some(v.to_string()),
            "branch" => e.branch = Some(v.trim_start_matches("refs/heads/").to_string()),
            "detached" => e.detached = true,
            "bare" => e.bare = true,
            "locked" => e.locked = true,
            "prunable" => e.prunable = true,
            _ => {}
        }
    }
    if let Some(e) = cur {
        out.push(e);
    }
    out
}

/// The worktree list for the repository that contains `dir`.
pub fn worktree_list(paths: &Paths, dir: &Path) -> Result<Vec<WorktreeEntry>, GitError> {
    git(paths, dir, &["worktree", "list", "--porcelain"]).map(|s| parse_worktree_list(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_worktree_list() {
        let s = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /r/w\nHEAD bbb\ndetached\nlocked reason\n\nworktree /gone\nHEAD ccc\nbranch refs/heads/x\nprunable gitdir file points to non-existent location\n";
        let v = parse_worktree_list(s);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert!(v[1].detached && v[1].locked && !v[1].prunable);
        assert!(v[2].prunable && v[2].head.as_deref() == Some("ccc"));
    }

    #[test]
    fn timeout_kills_the_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let t = Instant::now();
        let r = run_with_timeout(cmd, Duration::from_millis(150));
        assert!(matches!(r, Err(GitError::Timeout(_))));
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    /// R12: a background process holding the output open does not block
    /// the caller after the command exits; output is capped.
    #[test]
    fn output_reading_is_bounded() {
        // Same process group: killed after the grace period, output kept.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo hi"]);
        let t = Instant::now();
        let r = run_with_timeout(cmd, Duration::from_secs(60)).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout), "hi\n");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

        // A new session holds stdout: not waited for, reported as open.
        let mut cmd = Command::new("/usr/bin/perl");
        cmd.args([
            "-MPOSIX",
            "-e",
            "if (fork) { print qq(hi\n); exit 0 } POSIX::setsid(); sleep 8;",
        ]);
        let t = Instant::now();
        let r = run_with_timeout(cmd, Duration::from_secs(60));
        assert!(
            matches!(r, Err(GitError::OutputOpen)),
            "{:?}",
            r.map(|o| o.stdout)
        );
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

        // Too much stdout is an error, never a truncated success.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 3000000 /dev/zero"]);
        let r = run_capped(cmd, Duration::from_secs(30), 1 << 20);
        assert!(matches!(r, Err(GitError::OutputTooLarge(_))));
        // Stderr keeps its first MAX_STDERR bytes.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 3000000 /dev/zero >&2; echo ok"]);
        let r = run_with_timeout(cmd, Duration::from_secs(30)).unwrap();
        assert!(r.stderr.len() <= MAX_STDERR);
        assert_eq!(String::from_utf8_lossy(&r.stdout), "ok\n");
    }

    #[test]
    fn errors_are_values() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let r = git(&paths, tmp.path(), &["rev-parse", "HEAD"]);
        assert!(matches!(r, Err(GitError::Failed { .. })));
    }

    #[test]
    fn repo_config_cannot_run_code() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        let canary = tmp.path().join("ran");
        let script = tmp.path().join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho x >> '{}'\ncat\n", canary.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = tmp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for h in ["reference-transaction", "post-checkout"] {
            std::fs::copy(&script, hooks.join(h)).unwrap();
        }
        git(&paths, &repo, &["init", "-q", "-b", "main"]).unwrap();
        Git::new(&paths, &repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["commit", "-q", "--allow-empty", "-m", "i"])
            .run()
            .unwrap();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git(&paths, &repo, &["add", "a.txt"]).unwrap();
        Git::new(&paths, &repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["commit", "-q", "-m", "a"])
            .run()
            .unwrap();
        let s = script.display();
        let cfg = format!(
            "[filter \"x\"]\n\tclean = {s}\n\tsmudge = {s}\n[diff \"y\"]\n\ttextconv = {s}\n\tcommand = {s}\n[diff]\n\texternal = {s}\n[core]\n\tfsmonitor = {s}\n\thooksPath = {}\n\tpager = {s}\n",
            hooks.display()
        );
        let mut old = std::fs::read_to_string(repo.join(".git/config")).unwrap();
        old.push_str(&cfg);
        std::fs::write(repo.join(".git/config"), old).unwrap();
        std::fs::write(repo.join(".git/info/attributes"), "* filter=x diff=y\n").unwrap();
        std::fs::write(repo.join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();

        git(&paths, &repo, &["status", "--porcelain"]).unwrap();
        git(
            &paths,
            &repo,
            &["ls-files", "-o", "-i", "--exclude-standard"],
        )
        .unwrap();
        let idx = tmp.path().join("idx");
        std::fs::copy(repo.join(".git/index"), &idx).unwrap();
        Git::new(&paths, &repo)
            .env("GIT_INDEX_FILE", &idx)
            .args(["add", "-A"])
            .run()
            .unwrap();
        let diff = Git::new(&paths, &repo)
            .env("GIT_INDEX_FILE", &idx)
            .args(["diff", "--cached", "--binary", "HEAD"])
            .text()
            .unwrap();
        assert!(diff.contains("+world"));
        git(&paths, &repo, &["log", "-1", "--format=%ct", "HEAD"]).unwrap();
        git(&paths, &repo, &["branch", "b1", "HEAD"]).unwrap();
        let wt = tmp.path().join("w");
        git(
            &paths,
            &repo,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        )
        .unwrap();
        git(
            &paths,
            &repo,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        )
        .unwrap();
        assert!(
            !canary.exists(),
            "repository config ran code: {:?}",
            std::fs::read_to_string(&canary)
        );
    }
}
