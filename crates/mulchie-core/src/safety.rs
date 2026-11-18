//! The safety gate. Every cache deletion, worktree removal and signal goes through
//! here; nothing else in Mulchie deletes directories, removes worktrees or sends
//! signals.
//!
//! Scanners produce candidates. The gate re-reads every fact a decision depends
//! on at the moment it acts, never from a cached scan, and answers each request
//! with [`Outcome::Done`] or [`Outcome::Skipped`]. When a check cannot be
//! completed (git times out, a backup step fails, a path cannot be
//! canonicalized), the answer is "skipped", never "proceed".

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{create_private_dir, Config};
use crate::diskmap::{self, last_activity_checked};
use crate::git::{self, Git, WorktreeEntry};
use crate::journal::{Action, Journal};
use crate::paths::{random_hex, Paths};
use crate::process::{ProcFacts, ProcessSource};
use crate::reclaim::{self, dir_size, ItemKind};

/// Ignored directories that are regenerable caches. They are not archived before
/// a worktree is removed. Any path with one of these as a component is left out.
pub const KNOWN_CACHE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    ".angular",
    "dist",
    "build",
    ".build",
    ".gradle",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "DerivedData",
];

/// The first line Cargo (and other tools) write to `CACHEDIR.TAG`.
pub const CACHEDIR_TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Process names never signalled, compared case-insensitively against the
/// process name, its executable's file name and `argv[0]`'s file name.
const PROTECTED_PREFIX: &str = "mulchie";

/// git steps that write backups may take longer than a status read.
const BACKUP_TIMEOUT: Duration = Duration::from_secs(120);

/// Destructive actions run one at a time per process, so two callers (the API and
/// an automatic sweep) never back up and remove the same worktree at once.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

/// How long one gate reuses a read of the process table. A plan or execute call
/// over many items reads the table once per window instead of once per item;
/// the recheck just before a worktree is removed always reads it fresh.
pub const PROC_VIEW_MAX_AGE: Duration = Duration::from_secs(2);

/// Every process other than Mulchie itself; own processes carry their
/// canonical working directory (others `None`, so they never make a path busy).
type ProcView = Arc<Vec<(ProcFacts, Option<PathBuf>)>>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NotAbsolute,
    NotFound,
    Symlink,
    NotCanonical,
    OutsideScanRoots,
    NotRecognisedCache,
    MissingCachedirTag,
    NodeModulesDisabled,
    XcodeDisabled,
    Busy,
    NotStale,
    NotAWorktree,
    MissingExpectedHead,
    HeadMoved,
    Locked,
    NotAgent,
    NotIdle,
    Dirty,
    HasSubmodules,
    IgnoredTooLarge,
    BackupFailed,
    ChangedDuringBackup,
    NotPrunable,
    PruneDeferred,
    GitFailed,
    RemoveFailed,
    ProtectedProcess,
    ForeignProcess,
    ProcessGone,
    StartTimeMismatch,
    SignalFailed,
    UnknownTarget,
    /// A package-manager prune: the category has no safe prune command.
    NoPruneCommand,
    /// A package-manager prune: the tool is not installed.
    ToolNotFound,
    /// A command ran past its deadline and was killed.
    TimedOut,
    /// A command ran and failed.
    CommandFailed,
    /// A worktree contains another git repository (an untracked clone, a
    /// `git init`, a gitlink) whose history and files a patch cannot hold.
    NestedRepo,
    /// A worktree has changes git hides from `status` (skip-worktree or
    /// assume-unchanged files that differ from the index).
    HiddenChanges,
    /// A reopen that does not follow a quit, in the same batch, of a process
    /// belonging to that app.
    ReopenNotQuit,
    /// An automatic pass left the target alone because it was skipped
    /// several times in a row; see [`crate::failures`]. Never the answer to
    /// an explicit request, and never journaled.
    BackingOff,
}

impl SkipReason {
    /// The machine-readable code, as serialized.
    pub fn code(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default()
    }
}

/// Why a request was not carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub reason: SkipReason,
    pub detail: String,
}

fn skip<T>(reason: SkipReason, detail: impl Into<String>) -> Result<T, Skip> {
    Err(Skip {
        reason,
        detail: detail.into(),
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    /// Uncommitted changes, including untracked files, as a binary git patch.
    Patch,
    /// Gitignored files that are not known caches, as a `tar.gz`.
    IgnoredArchive,
    /// A branch pointing at a detached HEAD.
    Branch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Backup {
    pub kind: BackupKind,
    /// A file path, or a branch name for [`BackupKind::Branch`].
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Done {
        freed_bytes: u64,
        backups: Vec<Backup>,
    },
    Skipped {
        reason: SkipReason,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    DeleteCache,
    RemoveWorktree,
    PruneWorktree,
    Signal,
    Renice,
    ReopenApp,
}

/// The outcome of one request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionResult {
    pub action: ActionKind,
    /// The path, or `pid <n>` for a signal.
    pub target: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Set when the action ran (or was skipped) but could not be written to the
    /// action journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_error: Option<String>,
}

impl ActionResult {
    pub fn is_done(&self) -> bool {
        matches!(self.outcome, Outcome::Done { .. })
    }

    pub fn freed_bytes(&self) -> u64 {
        match &self.outcome {
            Outcome::Done { freed_bytes, .. } => *freed_bytes,
            _ => 0,
        }
    }

    pub fn backups(&self) -> &[Backup] {
        match &self.outcome {
            Outcome::Done { backups, .. } => backups,
            _ => &[],
        }
    }

    pub fn reason(&self) -> Option<SkipReason> {
        match &self.outcome {
            Outcome::Skipped { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// One line for people.
    pub fn summary(&self) -> String {
        match &self.outcome {
            Outcome::Done { backups, .. } => {
                let verb = match self.action {
                    ActionKind::DeleteCache => "Cleared",
                    ActionKind::RemoveWorktree => "Removed worktree",
                    ActionKind::PruneWorktree => "Pruned stale worktree entry",
                    ActionKind::Signal => "Signalled",
                    ActionKind::Renice => "Lowered the priority of",
                    ActionKind::ReopenApp => "Reopened",
                };
                let mut s = format!("{verb} {}", self.target);
                for b in backups {
                    let what = match b.kind {
                        BackupKind::Patch => "patch",
                        BackupKind::IgnoredArchive => "ignored files",
                        BackupKind::Branch => "branch",
                    };
                    s.push_str(&format!("; {what} saved to {}", b.location));
                }
                s
            }
            Outcome::Skipped { detail, .. } => format!("Skipped {}: {detail}", self.target),
        }
    }

    /// The journal entry for this result.
    pub fn to_action(&self, at_unix: u64, source: &str, signal: Option<Signal>) -> Action {
        let kind = match (self.action, signal) {
            (ActionKind::DeleteCache, _) => "reclaim",
            (ActionKind::RemoveWorktree | ActionKind::PruneWorktree, _) => "worktree",
            (ActionKind::Signal, Some(Signal::Stop)) => "pause",
            (ActionKind::Signal, Some(Signal::Cont)) => "resume",
            (ActionKind::Signal, _) => "stop",
            (ActionKind::Renice, _) => "throttle",
            (ActionKind::ReopenApp, _) => "restart",
        };
        let (status, reason) = match &self.outcome {
            Outcome::Done { .. } => ("done", None),
            Outcome::Skipped { reason, .. } => ("skipped", Some(*reason)),
        };
        Action {
            at_unix,
            kind: kind.into(),
            detail: self.summary(),
            bytes: self.freed_bytes(),
            workspace: None,
            source: source.into(),
            target: Some(self.target.clone()),
            status: Some(status.into()),
            reason,
            backups: self.backups().to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Term,
    Kill,
    Stop,
    Cont,
}

impl Signal {
    pub fn number(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Kill => "SIGKILL",
            Signal::Stop => "SIGSTOP",
            Signal::Cont => "SIGCONT",
        }
    }
}

/// One thing a caller wants done. Plans contain these; callers send back the ones
/// the user approved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    /// Delete a recognised build cache. `require_stale` rechecks staleness now.
    DeleteCache {
        path: PathBuf,
        #[serde(default)]
        require_stale: bool,
    },
    /// Remove a git worktree whose HEAD is still `expected_head`.
    RemoveWorktree {
        path: PathBuf,
        expected_head: String,
        /// Remove even with uncommitted changes (a patch is saved first).
        #[serde(default)]
        allow_dirty: bool,
        /// Only an idle agent worktree (what sweeps ask for).
        #[serde(default)]
        require_idle: bool,
    },
    /// Prune the admin entry of an agent worktree whose directory is gone.
    PruneWorktree { repo: PathBuf, path: PathBuf },
    /// Signal a process that still has the start time the caller saw.
    Signal {
        pid: u32,
        signal: Signal,
        expected_start_time: u64,
    },
    /// Lower the priority of a process (raise its nice value to `nice`,
    /// 0–20; never lowers a nice value) that still has the start time the
    /// caller saw.
    Renice {
        pid: u32,
        nice: i32,
        expected_start_time: u64,
    },
    /// Reopen an app (`open -b <bundle_id>`, macOS) once the process
    /// `after_pid` (its main process, started at `after_start_time`) has
    /// exited; waits up to [`REOPEN_WAIT`] for that. The second half of a
    /// group restart.
    ReopenApp {
        bundle_id: String,
        after_pid: u32,
        after_start_time: u64,
    },
}

/// Tests: a hook given the patch file before it is verified.
#[cfg(test)]
pub(crate) type PatchHook = Box<dyn Fn(&Path) + Send + Sync>;

/// How long [`Request::ReopenApp`] waits for the app's main process to exit.
pub const REOPEN_WAIT: Duration = Duration::from_secs(20);

/// A process a batch quit, for the [`Request::ReopenApp`] that may follow.
struct QuitApp {
    pid: u32,
    start_time: u64,
    bundle_id: Option<String>,
}

impl Request {
    pub fn action(&self) -> ActionKind {
        match self {
            Request::DeleteCache { .. } => ActionKind::DeleteCache,
            Request::RemoveWorktree { .. } => ActionKind::RemoveWorktree,
            Request::PruneWorktree { .. } => ActionKind::PruneWorktree,
            Request::Signal { .. } => ActionKind::Signal,
            Request::Renice { .. } => ActionKind::Renice,
            Request::ReopenApp { .. } => ActionKind::ReopenApp,
        }
    }

    /// The journal kind of a request whose target is a stable name (a path):
    /// `reclaim` or `worktree`. `None` for process requests, whose pid names
    /// a different process each time.
    pub fn failure_kind(&self) -> Option<&'static str> {
        match self {
            Request::DeleteCache { .. } => Some("reclaim"),
            Request::RemoveWorktree { .. } | Request::PruneWorktree { .. } => Some("worktree"),
            _ => None,
        }
    }

    pub fn target(&self) -> String {
        match self {
            Request::DeleteCache { path, .. }
            | Request::RemoveWorktree { path, .. }
            | Request::PruneWorktree { path, .. } => path.display().to_string(),
            Request::Signal { pid, .. } | Request::Renice { pid, .. } => format!("pid {pid}"),
            Request::ReopenApp { bundle_id, .. } => format!("app {bundle_id}"),
        }
    }
}

/// One line of a plan: what would happen to one candidate, from facts read now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    /// Send this back to execute the item.
    pub request: Request,
    pub target: String,
    pub name: String,
    pub repo_name: Option<String>,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bytes: u64,
    pub dirty_files: usize,
    pub idle_days: Option<u64>,
    pub ignored_files: usize,
    pub ignored_bytes: u64,
    /// What would be saved before removal.
    pub would_backup: Vec<String>,
    pub ready: bool,
    pub reason: Option<SkipReason>,
    pub verdict: String,
    /// Set by the engine when this target was skipped at least twice in a
    /// row before: how often, why, and what to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub past_failures: Option<crate::failures::PastFailures>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Plan {
    pub created_unix: u64,
    pub items: Vec<PlanItem>,
    /// Bytes the ready items would free.
    pub ready_bytes: u64,
}

impl Plan {
    /// The requests of every ready item.
    pub fn approved(&self) -> Vec<Request> {
        self.items
            .iter()
            .filter(|i| i.ready)
            .map(|i| i.request.clone())
            .collect()
    }
}

/// Threads that check the items of one plan. Half of the git wrapper's own
/// limit ([`git::MAX_CONCURRENT`]), so a large plan leaves room for an
/// execution or another plan running at the same time.
pub const PLAN_WORKERS: usize = 4;

/// The gate: configuration, locations, the live process table and the clock.
pub struct Gate<'a> {
    pub config: &'a Config,
    pub paths: &'a Paths,
    pub procs: &'a dyn ProcessSource,
    pub journal: Option<&'a Journal>,
    pub now: u64,
    proc_view: Mutex<Option<(Instant, ProcView)>>,
    proc_reads: AtomicUsize,
    /// How long a reopen waits for the app to exit ([`REOPEN_WAIT`]).
    reopen_wait: Duration,
    /// Tests: runs after the backups are written, before the recheck.
    #[cfg(test)]
    pub(crate) after_backup: Option<Box<dyn Fn() + Send + Sync>>,
    /// Tests: runs on `changes.patch` after it is written, before it is
    /// verified.
    #[cfg(test)]
    pub(crate) before_verify: Option<PatchHook>,
}

struct CacheFacts {
    kind: ItemKind,
    bytes: u64,
    /// The directory whose users make the cache busy, and how to match them;
    /// rechecked with a fresh process read just before the rename.
    busy_root: PathBuf,
    nested: bool,
    xcode: bool,
}

struct WorktreeFacts {
    path: PathBuf,
    repo: PathBuf,
    name: String,
    repo_name: String,
    head: String,
    branch: Option<String>,
    detached: bool,
    idle_days: Option<u64>,
    dirty_files: usize,
    status: String,
    ignored: Vec<String>,
    ignored_bytes: u64,
    bytes: u64,
}

/// What [`Gate::back_up`] wrote, and what it needs to recheck afterwards.
struct BackupSet {
    backups: Vec<Backup>,
    /// The backup set directory, when one was written.
    dir: Option<PathBuf>,
    /// The tree the patch was made from (`add -A` + `write-tree`).
    tree: Option<String>,
}

/// (type, length, mtime seconds, mtime nanoseconds, inode) of one path, or
/// `None` when it is missing.
type StatPrint = Option<(u32, u64, i64, i64, u64)>;

fn stat_print(p: &Path) -> StatPrint {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(p).ok()?;
    Some((
        m.mode() & 0o170000,
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ino(),
    ))
}

/// At most this many paths are fingerprinted; past it the recheck cannot
/// complete, which stops the removal.
const FINGERPRINT_BUDGET: usize = 200_000;

/// Stat fingerprints of every dirty path (from porcelain status) and every
/// file and directory under the ignored entries that are archived. Taken before
/// the backup and compared after it, so an edit made while the backup ran (a
/// second write to an already-dirty file, a change to an ignored `.env`) stops
/// the removal even when `git status` looks the same.
fn fingerprint(f: &WorktreeFacts) -> Option<std::collections::BTreeMap<PathBuf, StatPrint>> {
    let mut out = std::collections::BTreeMap::new();
    for line in f.status.lines() {
        let Some(rest) = line.get(3..) else { continue };
        // Renames list `old -> new`; quoted names are covered by the tree hash.
        for part in rest.split(" -> ") {
            if !part.starts_with('"') {
                let p = f.path.join(part);
                out.insert(p.clone(), stat_print(&p));
            }
        }
    }
    let mut budget = FINGERPRINT_BUDGET;
    for rel in &f.ignored {
        let mut stack = vec![f.path.join(rel)];
        while let Some(p) = stack.pop() {
            budget = budget.checked_sub(1)?;
            let st = stat_print(&p);
            let is_dir = st.is_some_and(|s| s.0 == 0o040000);
            out.insert(p.clone(), st);
            if is_dir {
                let rd = std::fs::read_dir(&p).ok()?;
                for e in rd.flatten() {
                    stack.push(e.path());
                }
            }
        }
    }
    Some(out)
}

struct PruneFacts {
    repo: PathBuf,
    entry: WorktreeEntry,
    repo_name: String,
}

impl<'a> Gate<'a> {
    pub fn new(
        config: &'a Config,
        paths: &'a Paths,
        procs: &'a dyn ProcessSource,
        now: u64,
    ) -> Self {
        Self {
            config,
            paths,
            procs,
            journal: None,
            now,
            proc_view: Mutex::new(None),
            proc_reads: AtomicUsize::new(0),
            reopen_wait: REOPEN_WAIT,
            #[cfg(test)]
            after_backup: None,
            #[cfg(test)]
            before_verify: None,
        }
    }

    /// How many times this gate has read the whole process table.
    pub fn process_table_reads(&self) -> usize {
        self.proc_reads.load(Ordering::SeqCst)
    }

    /// The process table as of at most [`PROC_VIEW_MAX_AGE`] ago, or read now
    /// with `fresh`.
    fn procs_view(&self, fresh: bool) -> ProcView {
        let mut g = self.proc_view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = g.as_ref() {
            if !fresh && at.elapsed() < PROC_VIEW_MAX_AGE {
                return v.clone();
            }
        }
        self.proc_reads.fetch_add(1, Ordering::SeqCst);
        let uid = self.procs.current_uid();
        let me = self.procs.self_pid();
        let v: ProcView = Arc::new(
            self.procs
                .list()
                .into_iter()
                .filter(|p| p.pid != me)
                .map(|p| {
                    let c = if p.uid == Some(uid) {
                        p.cwd
                            .as_ref()
                            .map(|c| c.canonicalize().unwrap_or_else(|_| c.clone()))
                    } else {
                        None
                    };
                    (p, c)
                })
                .collect(),
        );
        *g = Some((Instant::now(), v.clone()));
        v
    }

    pub fn with_journal(mut self, journal: &'a Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    // ---------------------------------------------------------------- plans

    /// Check every request against the facts as they are now, without acting.
    pub fn plan(&self, requests: &[Request]) -> Plan {
        let items = self.plan_items(requests);
        let ready_bytes = items.iter().filter(|i| i.ready).map(|i| i.bytes).sum();
        Plan {
            created_unix: self.now,
            items,
            ready_bytes,
        }
    }

    /// Check every request, in order. Planning only reads, and nearly all of
    /// its time is spent waiting for git, so a plan over several requests is
    /// checked by up to [`PLAN_WORKERS`] threads. Execution stays one item at
    /// a time.
    fn plan_items(&self, requests: &[Request]) -> Vec<PlanItem> {
        let workers = PLAN_WORKERS.min(requests.len());
        if workers < 2 {
            return requests.iter().map(|r| self.plan_item(r)).collect();
        }
        // One read of the process table serves the whole plan; take it before
        // the workers start so they share it.
        let _ = self.procs_view(false);
        let next = AtomicUsize::new(0);
        let done: Mutex<Vec<(usize, PlanItem)>> = Mutex::new(Vec::with_capacity(requests.len()));
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(r) = requests.get(i) else { break };
                    let item = self.plan_item(r);
                    done.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((i, item));
                });
            }
        });
        let mut done = done.into_inner().unwrap_or_else(|e| e.into_inner());
        done.sort_by_key(|(i, _)| *i);
        if done.len() != requests.len() {
            // A worker panicked: check again on this thread, where a panic
            // is the caller's to see.
            return requests.iter().map(|r| self.plan_item(r)).collect();
        }
        done.into_iter().map(|(_, item)| item).collect()
    }

    /// A sweep plan from scanned worktrees: every candidate the scanner marked
    /// sweepable, re-checked now. Each ready item carries the HEAD read now.
    pub fn plan_sweep(&self, worktrees: &[diskmap::Worktree]) -> Plan {
        let reqs: Vec<Request> = worktrees
            .iter()
            .filter(|w| w.sweepable)
            .map(|w| {
                if w.missing {
                    Request::PruneWorktree {
                        repo: w.repo.clone(),
                        path: w.path.clone(),
                    }
                } else {
                    Request::RemoveWorktree {
                        path: w.path.clone(),
                        expected_head: String::new(),
                        allow_dirty: true,
                        require_idle: true,
                    }
                }
            })
            .collect();
        self.plan(&reqs)
    }

    /// A reclaim plan from scanned caches: candidates the scanner marked stale,
    /// re-checked now (staleness included), largest first, until `want_bytes`.
    /// `None` takes every stale cache; `Some(0)` takes none.
    pub fn plan_reclaim(&self, items: &[reclaim::Item], want_bytes: Option<u64>) -> Plan {
        let mut cands: Vec<&reclaim::Item> = items.iter().filter(|i| i.stale).collect();
        cands.sort_by_key(|i| std::cmp::Reverse(i.bytes));
        let mut out = Vec::new();
        let mut sum = 0u64;
        for c in cands {
            if let Some(w) = want_bytes {
                if sum >= w {
                    break;
                }
            }
            let item = self.plan_item(&Request::DeleteCache {
                path: c.path.clone(),
                require_stale: true,
            });
            if item.ready {
                sum += item.bytes;
            }
            out.push(item);
        }
        let ready_bytes = out.iter().filter(|i| i.ready).map(|i| i.bytes).sum();
        Plan {
            created_unix: self.now,
            items: out,
            ready_bytes,
        }
    }

    fn plan_item(&self, req: &Request) -> PlanItem {
        let target = req.target();
        let mut item = PlanItem {
            request: req.clone(),
            name: target.clone(),
            target,
            repo_name: None,
            head: None,
            branch: None,
            bytes: 0,
            dirty_files: 0,
            idle_days: None,
            ignored_files: 0,
            ignored_bytes: 0,
            would_backup: Vec::new(),
            ready: false,
            reason: None,
            verdict: String::new(),
            past_failures: None,
        };
        let res: Result<String, Skip> = match req {
            Request::DeleteCache {
                path,
                require_stale,
            } => self.check_cache(path, *require_stale).map(|f| {
                item.bytes = f.bytes;
                item.name = f.kind.label().to_string();
                format!("{} can be cleared", f.kind.label())
            }),
            Request::RemoveWorktree {
                path,
                expected_head,
                allow_dirty,
                require_idle,
            } => {
                let expected = (!expected_head.is_empty()).then_some(expected_head.as_str());
                self.check_worktree(path, expected, *allow_dirty, *require_idle)
                    .map(|f| {
                        item.request = Request::RemoveWorktree {
                            path: f.path.clone(),
                            expected_head: f.head.clone(),
                            allow_dirty: *allow_dirty,
                            require_idle: *require_idle,
                        };
                        item.target = f.path.display().to_string();
                        item.name = f.name.clone();
                        item.repo_name = Some(f.repo_name.clone());
                        item.head = Some(f.head.clone());
                        item.branch = f.branch.clone();
                        item.bytes = f.bytes;
                        item.dirty_files = f.dirty_files;
                        item.idle_days = f.idle_days;
                        item.ignored_files = f.ignored.len();
                        item.ignored_bytes = f.ignored_bytes;
                        item.would_backup = self.would_backup(&f);
                        match f.idle_days {
                            Some(d) => format!("idle {d}d; removable"),
                            None => "removable".to_string(),
                        }
                    })
            }
            Request::PruneWorktree { repo, path } => self.check_prune(repo, path).map(|f| {
                item.name = file_name(&f.entry.path);
                item.repo_name = Some(f.repo_name);
                item.head = f.entry.head.clone();
                item.branch = f.entry.branch.clone();
                "directory is gone; git marks the entry prunable".to_string()
            }),
            Request::Signal {
                pid,
                expected_start_time,
                ..
            } => self.check_signal(*pid, *expected_start_time).map(|p| {
                item.name = p.name;
                "can be signalled".into()
            }),
            Request::Renice {
                pid,
                nice,
                expected_start_time,
            } => self
                .check_renice(*pid, *nice, *expected_start_time)
                .map(|(p, cur)| {
                    item.name = p.name;
                    format!("priority can be lowered (nice {cur} → {nice})")
                }),
            Request::ReopenApp {
                bundle_id,
                after_pid,
                ..
            } => self.check_reopen(bundle_id).map(|_| {
                item.name = bundle_id.clone();
                format!("{bundle_id} reopens once pid {after_pid} has exited")
            }),
        };
        match res {
            Ok(v) => {
                item.ready = true;
                item.verdict = v;
            }
            Err(s) => {
                item.reason = Some(s.reason);
                item.verdict = s.detail;
            }
        }
        item
    }

    fn would_backup(&self, f: &WorktreeFacts) -> Vec<String> {
        let mut v = Vec::new();
        if f.dirty_files > 0 {
            v.push(format!("patch of {} changed file(s)", f.dirty_files));
        }
        if !f.ignored.is_empty() {
            v.push(format!(
                "archive of {} ignored item(s), {}",
                f.ignored.len(),
                crate::human_bytes(f.ignored_bytes)
            ));
        }
        if f.detached {
            v.push(format!("branch {}", backup_branch_name(&f.name, &f.head)));
        }
        v
    }

    // ------------------------------------------------------------- execution

    /// Carry out each request after re-checking it now. Every result, done or
    /// skipped, is appended to the journal when the gate has one.
    ///
    /// A [`Request::ReopenApp`] is carried out only after a `Signal` earlier in
    /// the same batch quit a process of that app (same pid and start time,
    /// and an executable inside a bundle with that id).
    pub fn execute(&self, requests: &[Request], source: &str) -> Vec<ActionResult> {
        let reopens = requests
            .iter()
            .any(|r| matches!(r, Request::ReopenApp { .. }));
        let mut quit: Vec<QuitApp> = Vec::new();
        requests
            .iter()
            .map(|r| match r {
                Request::Signal {
                    pid,
                    signal: Signal::Term | Signal::Kill,
                    expected_start_time,
                } if reopens => {
                    // Read the bundle before the process goes away.
                    let bundle_id = self
                        .procs
                        .get(*pid)
                        .filter(|p| p.start_time == *expected_start_time)
                        .and_then(|p| p.exe)
                        .and_then(|e| crate::consumers::outer_bundle(&e))
                        .and_then(|b| crate::consumers::bundle_id(&b));
                    let res = self.run_one(r, source, &[]);
                    if matches!(res.outcome, Outcome::Done { .. }) {
                        quit.push(QuitApp {
                            pid: *pid,
                            start_time: *expected_start_time,
                            bundle_id,
                        });
                    }
                    res
                }
                _ => self.run_one(r, source, &quit),
            })
            .collect()
    }

    /// One request on its own. A [`Request::ReopenApp`] is always refused
    /// here: it needs the quit that precedes it in a batch ([`Gate::execute`]).
    pub fn execute_one(&self, req: &Request, source: &str) -> ActionResult {
        self.run_one(req, source, &[])
    }

    fn run_one(&self, req: &Request, source: &str, quit: &[QuitApp]) -> ActionResult {
        let outcome = {
            // A reopen waits for the app to exit without the lock, and takes
            // it itself for the re-check and the `open`.
            let _g = (!matches!(req, Request::ReopenApp { .. }))
                .then(|| EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
            let res = match req {
                Request::DeleteCache {
                    path,
                    require_stale,
                } => self.delete_cache(path, *require_stale),
                Request::RemoveWorktree {
                    path,
                    expected_head,
                    allow_dirty,
                    require_idle,
                } => {
                    if expected_head.is_empty() {
                        skip(
                            SkipReason::MissingExpectedHead,
                            "the request does not name the HEAD commit the caller saw",
                        )
                    } else {
                        self.remove_worktree(path, expected_head, *allow_dirty, *require_idle)
                    }
                }
                Request::PruneWorktree { repo, path } => self.prune_worktree(repo, path),
                Request::Signal {
                    pid,
                    signal,
                    expected_start_time,
                } => self.send_signal(*pid, *signal, *expected_start_time),
                Request::Renice {
                    pid,
                    nice,
                    expected_start_time,
                } => self.renice(*pid, *nice, *expected_start_time),
                Request::ReopenApp {
                    bundle_id,
                    after_pid,
                    after_start_time,
                } => self.reopen_app(bundle_id, *after_pid, *after_start_time, quit),
            };
            match res {
                Ok(o) => o,
                Err(s) => Outcome::Skipped {
                    reason: s.reason,
                    detail: s.detail,
                },
            }
        };
        let mut result = ActionResult {
            action: req.action(),
            target: req.target(),
            outcome,
            journal_error: None,
        };
        if let Some(j) = self.journal {
            let sig = match req {
                Request::Signal { signal, .. } => Some(*signal),
                _ => None,
            };
            if let Err(e) = j.append(&result.to_action(self.now, source, sig)) {
                result.journal_error = Some(format!("not journaled: {e}"));
            }
        }
        result
    }

    // ------------------------------------------------------------------ cache

    /// A direct child of `systemd --user` whose working directory is in no
    /// scan root: a user service of the desktop session.
    pub(crate) fn user_service(&self, p: &ProcFacts) -> Option<String> {
        let parent = self.procs.get(p.ppid)?;
        if !crate::consumers::is_user_manager(&parent.name, &parent.cmd) {
            return None;
        }
        let in_workspace = p.cwd.as_deref().is_some_and(|c| {
            let c = c.canonicalize().unwrap_or_else(|_| c.to_path_buf());
            self.canonical_roots().iter().any(|r| c.starts_with(r))
        });
        (!in_workspace).then(|| crate::consumers::user_service_reason(&p.name))
    }

    fn canonical_roots(&self) -> Vec<PathBuf> {
        self.config
            .roots(self.paths)
            .iter()
            .filter_map(|r| r.canonicalize().ok())
            .collect()
    }

    fn check_cache(&self, path: &Path, require_stale: bool) -> Result<CacheFacts, Skip> {
        if !path.is_absolute() {
            return skip(SkipReason::NotAbsolute, "path must be absolute");
        }
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => return skip(SkipReason::NotFound, e.to_string()),
        };
        if meta.file_type().is_symlink() {
            return skip(SkipReason::Symlink, "path is a symbolic link");
        }
        if !meta.is_dir() {
            return skip(SkipReason::NotRecognisedCache, "not a directory");
        }
        let canon = match path.canonicalize() {
            Ok(c) => c,
            Err(e) => return skip(SkipReason::NotCanonical, e.to_string()),
        };
        if canon != path {
            return skip(
                SkipReason::NotCanonical,
                format!("path resolves to {}", canon.display()),
            );
        }

        let dd = self.paths.derived_data().canonicalize().ok();
        if dd.as_deref() == Some(path) {
            if !self.config.include_xcode {
                return skip(SkipReason::XcodeDisabled, "include_xcode is off");
            }
            self.xcode_check(false)?;
            self.busy_check(path, false)?;
            if require_stale {
                self.stale_check(path, false)?;
            }
            return Ok(CacheFacts {
                kind: ItemKind::XcodeDerivedData,
                bytes: dir_size(path),
                busy_root: path.to_path_buf(),
                nested: false,
                xcode: true,
            });
        }

        let roots = self.canonical_roots();
        let Some(root) = roots
            .iter()
            .filter(|r| path.starts_with(r) && path != r.as_path())
            .max_by_key(|r| r.components().count())
        else {
            return skip(
                SkipReason::OutsideScanRoots,
                "path is outside the configured scan roots",
            );
        };
        if path == self.paths.home || path.components().count() < 4 {
            return skip(SkipReason::OutsideScanRoots, "path is too shallow");
        }
        let kind = recognise_cache(path, self.config)?;
        let parent = path.parent().unwrap_or(path);
        let (project, in_worktree) = project_root(parent, root);
        self.busy_check(&project, true)?;
        if require_stale {
            self.stale_check(path, in_worktree)?;
        }
        Ok(CacheFacts {
            kind,
            bytes: dir_size(path),
            busy_root: project,
            nested: true,
            xcode: false,
        })
    }

    fn xcode_check(&self, fresh: bool) -> Result<(), Skip> {
        if let Some((p, _)) = self
            .procs_view(fresh)
            .iter()
            .find(|(p, _)| matches!(p.name.to_ascii_lowercase().as_str(), "xcode" | "xcodebuild"))
        {
            return skip(
                SkipReason::Busy,
                format!("{} (pid {}) is running", p.name, p.pid),
            );
        }
        Ok(())
    }

    /// Skip when an own process's working directory, read now, is at or under
    /// `root`. With `nested`, directories inside another git checkout below `root`
    /// (a worktree inside the repo) belong to that checkout, not to `root`.
    fn busy_check(&self, root: &Path, nested: bool) -> Result<(), Skip> {
        self.busy_check_with(root, nested, false)
    }

    fn busy_check_with(&self, root: &Path, nested: bool, fresh: bool) -> Result<(), Skip> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let view = self.procs_view(fresh);
        let uid = self.procs.current_uid();
        let found = view
            .iter()
            .find(|(_, c)| c.as_deref().is_some_and(|c| cwd_in(c, &root, nested)));
        // An own process whose working directory cannot be read cannot be
        // cleared by it; when its arguments name a path under `root`, count it
        // as busy.
        let unreadable = view.iter().find(|(p, c)| {
            c.is_none()
                && p.uid == Some(uid)
                && p.cmd.iter().skip(1).any(|a| {
                    let a = a.split_once('=').map(|(_, v)| v).unwrap_or(a);
                    Path::new(a).is_absolute() && Path::new(a).starts_with(&root)
                })
        });
        if let Some((p, _)) = unreadable {
            return skip(
                SkipReason::Busy,
                format!(
                    "{} (pid {}) names {} on its command line and its working directory cannot be read",
                    p.name,
                    p.pid,
                    root.display()
                ),
            );
        }
        if let Some((p, _)) = found {
            return skip(
                SkipReason::Busy,
                format!(
                    "{} (pid {}) is working in {}",
                    p.name,
                    p.pid,
                    p.cwd.as_deref().unwrap_or(&root).display()
                ),
            );
        }
        Ok(())
    }

    fn stale_check(&self, path: &Path, in_worktree: bool) -> Result<(), Skip> {
        let hours = if in_worktree {
            self.config.worktree_target_stale_hours
        } else {
            self.config.stale_hours
        };
        let last = reclaim::last_used(path);
        let age = self.now.saturating_sub(last);
        if age < hours * 3600 {
            return skip(
                SkipReason::NotStale,
                format!("used {}h ago; the stale window is {hours}h", age / 3600),
            );
        }
        Ok(())
    }

    fn delete_cache(&self, path: &Path, require_stale: bool) -> Result<Outcome, Skip> {
        let facts = self.check_cache(path, require_stale)?;
        // The checks above may have used a process view up to
        // PROC_VIEW_MAX_AGE old; read the table again right before acting.
        if facts.xcode {
            self.xcode_check(true)?;
        }
        self.busy_check_with(&facts.busy_root, facts.nested, true)?;
        let parent = path.parent().unwrap_or(path);
        let name = file_name(path);
        let suffix = random_hex(6).map_err(|e| Skip {
            reason: SkipReason::RemoveFailed,
            detail: e.to_string(),
        })?;
        // Move aside first: a build that starts now gets a fresh directory instead
        // of a half-deleted one.
        let trash = parent.join(format!(".{name}.mulchie-delete-{suffix}"));
        if let Err(e) = std::fs::rename(path, &trash) {
            return skip(
                SkipReason::RemoveFailed,
                format!("could not move aside: {e}"),
            );
        }
        if let Err(e) = std::fs::remove_dir_all(&trash) {
            let left = dir_size(&trash);
            return skip(
                SkipReason::RemoveFailed,
                format!(
                    "moved to {} but deleting failed ({e}); {} left there",
                    trash.display(),
                    crate::human_bytes(left)
                ),
            );
        }
        Ok(Outcome::Done {
            freed_bytes: facts.bytes,
            backups: Vec::new(),
        })
    }

    // --------------------------------------------------------------- worktree

    fn git_skip(e: git::GitError) -> Skip {
        Skip {
            reason: SkipReason::GitFailed,
            detail: e.to_string(),
        }
    }

    fn check_worktree(
        &self,
        path: &Path,
        expected_head: Option<&str>,
        allow_dirty: bool,
        require_idle: bool,
    ) -> Result<WorktreeFacts, Skip> {
        if !path.is_absolute() {
            return skip(SkipReason::NotAbsolute, "path must be absolute");
        }
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                return skip(
                    SkipReason::NotFound,
                    format!("{e}; a worktree whose directory is gone is pruned, not removed"),
                )
            }
        };
        if meta.file_type().is_symlink() {
            return skip(SkipReason::Symlink, "path is a symbolic link");
        }
        if !meta.is_dir() {
            return skip(SkipReason::NotAWorktree, "not a directory");
        }
        let canon = path.canonicalize().map_err(|e| Skip {
            reason: SkipReason::NotCanonical,
            detail: e.to_string(),
        })?;

        let list = git::worktree_list(self.paths, &canon).map_err(Self::git_skip)?;
        let Some(main) = list.first() else {
            return skip(SkipReason::NotAWorktree, "git lists no worktrees");
        };
        let repo = main.path.clone();
        let Some(entry) = list
            .iter()
            .skip(1)
            .find(|e| !e.bare && e.path.canonicalize().ok().as_deref() == Some(canon.as_path()))
            .cloned()
        else {
            return skip(
                SkipReason::NotAWorktree,
                "not a linked worktree (the main checkout is never removed)",
            );
        };
        let roots = self.canonical_roots();
        let repo_canon = repo.canonicalize().unwrap_or_else(|_| repo.clone());
        if !roots
            .iter()
            .any(|r| canon.starts_with(r) || repo_canon.starts_with(r))
        {
            return skip(
                SkipReason::OutsideScanRoots,
                "neither the worktree nor its repository is under a scan root",
            );
        }
        if entry.prunable {
            return skip(
                SkipReason::GitFailed,
                "git reports this worktree as prunable",
            );
        }
        let Some(head) = entry.head.clone() else {
            return skip(SkipReason::GitFailed, "git did not report a HEAD");
        };
        if let Some(exp) = expected_head {
            if exp != head {
                return skip(
                    SkipReason::HeadMoved,
                    format!("HEAD moved from {} to {}", short(exp), short(&head)),
                );
            }
        }
        if entry.locked {
            return skip(SkipReason::Locked, "locked by git");
        }
        self.busy_check(&canon, false)?;
        let name = file_name(&canon);
        let agent = is_agent_worktree(&canon);
        let mut idle_days = None;
        if require_idle {
            if !agent {
                return skip(SkipReason::NotAgent, "sweeps only remove agent worktrees");
            }
            let last_commit: u64 =
                git::git(self.paths, &canon, &["log", "-1", "--format=%ct", "HEAD"])
                    .map_err(Self::git_skip)?
                    .trim()
                    .parse()
                    .unwrap_or(0);
            let Some(walked) = last_activity_checked(&canon) else {
                return skip(
                    SkipReason::NotIdle,
                    format!(
                        "more than {} files; cannot tell when it was last used",
                        diskmap::ACTIVITY_BUDGET
                    ),
                );
            };
            let last = walked.max(last_commit);
            let days = self.now.saturating_sub(last) / 86_400;
            if days < self.config.worktree_idle_days {
                return skip(
                    SkipReason::NotIdle,
                    format!(
                        "active {days}d ago; the idle window is {}d",
                        self.config.worktree_idle_days
                    ),
                );
            }
            idle_days = Some(days);
        }
        if let Some(sub) = initialized_submodule(self.paths, &canon)? {
            return skip(
                SkipReason::HasSubmodules,
                format!("submodule {sub} is checked out; its changes cannot be backed up"),
            );
        }
        if let Some(nested) = nested_repo(self.paths, &canon)? {
            return skip(
                SkipReason::NestedRepo,
                format!(
                    "{nested} is a separate git repository; its commits and files would be lost"
                ),
            );
        }
        if let Some((n, first)) = hidden_changes(self.paths, &canon)? {
            return skip(
                SkipReason::HiddenChanges,
                format!(
                    "{n} file(s) marked skip-worktree or assume-unchanged have local edits \
                     (first: {first}); git hides them from a backup"
                ),
            );
        }
        let status = git::git(
            self.paths,
            &canon,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=all",
            ],
        )
        .map_err(Self::git_skip)?;
        let dirty_files = status.lines().filter(|l| !l.is_empty()).count();
        if dirty_files > 0 && !allow_dirty {
            return skip(
                SkipReason::Dirty,
                format!(
                    "{dirty_files} uncommitted file(s); allow backup to save a patch and remove"
                ),
            );
        }
        let (ignored, ignored_bytes) = ignored_to_archive(self.paths, &canon)?;
        let max = self
            .config
            .backup_ignored_max_mb
            .saturating_mul(1024 * 1024);
        if ignored_bytes > max {
            return skip(
                SkipReason::IgnoredTooLarge,
                format!(
                    "{} of gitignored files that are not caches; the backup limit is {} MB",
                    crate::human_bytes(ignored_bytes),
                    self.config.backup_ignored_max_mb
                ),
            );
        }
        Ok(WorktreeFacts {
            bytes: dir_size(&canon),
            repo_name: file_name(&repo),
            path: canon,
            repo,
            name,
            head,
            branch: entry.branch.clone(),
            detached: entry.detached,
            idle_days,
            dirty_files,
            status,
            ignored,
            ignored_bytes,
        })
    }

    fn remove_worktree(
        &self,
        path: &Path,
        expected_head: &str,
        allow_dirty: bool,
        require_idle: bool,
    ) -> Result<Outcome, Skip> {
        let f = self.check_worktree(path, Some(expected_head), allow_dirty, require_idle)?;
        let Some(before) = fingerprint(&f) else {
            return skip(
                SkipReason::BackupFailed,
                "could not record the state of changed and ignored files; nothing was removed",
            );
        };
        let set = self.back_up(&f).map_err(|e| Skip {
            reason: SkipReason::BackupFailed,
            detail: format!("{e}; nothing was removed"),
        })?;
        #[cfg(test)]
        if let Some(h) = &self.after_backup {
            h();
        }
        // Until `git worktree remove` starts, a skip leaves the worktree as it
        // was: the backup set written for it is discarded, so a removal that
        // keeps failing does not add a copy on every try.
        if let Err(mut s) = self.recheck_after_backup(&f, &set, &before) {
            s.detail
                .push_str(&self.discard_backup_set(set.dir.as_deref()));
            return Err(s);
        }
        // From here git may have deleted part of the worktree even when it
        // fails, so the backups are kept and the skip says where they are.
        let kept = |mut s: Skip| {
            if let Some(d) = &set.dir {
                s.detail
                    .push_str(&format!("; its backups are kept in {}", d.display()));
            }
            s
        };

        let mut g = Git::new(self.paths, &f.repo).args(["worktree", "remove"]);
        if f.dirty_files > 0 {
            // Uncommitted work is in the patch; git refuses otherwise.
            g = g.arg("--force");
        }
        g.arg(&f.path).timeout(BACKUP_TIMEOUT).run().map_err(|e| {
            kept(Skip {
                reason: SkipReason::RemoveFailed,
                detail: e.to_string(),
            })
        })?;
        if f.path.exists() {
            return Err(kept(Skip {
                reason: SkipReason::RemoveFailed,
                detail: "git reported success but the directory is still there".into(),
            }));
        }
        Ok(Outcome::Done {
            freed_bytes: f.bytes,
            backups: set.backups,
        })
    }

    /// Remove a backup set this gate has just written for a removal that did
    /// not happen, and say so (the text is appended to the skip's detail).
    /// Only a directory directly inside the backups directory, as
    /// [`Gate::backup_set_dir`] creates them, is removed; anything else, or a
    /// failure, is reported and left.
    fn discard_backup_set(&self, dir: Option<&Path>) -> String {
        let Some(dir) = dir else {
            return String::new();
        };
        let ours = dir.parent() == Some(self.paths.backups_dir.as_path())
            && std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
        if ours && std::fs::remove_dir_all(dir).is_ok() {
            "; the backup written for it was discarded (the worktree is untouched)".into()
        } else {
            format!("; the unused backup is in {}", dir.display())
        }
    }

    /// After the backups are written and before anything is removed: the
    /// worktree must still be what was backed up.
    fn recheck_after_backup(
        &self,
        f: &WorktreeFacts,
        set: &BackupSet,
        before: &std::collections::BTreeMap<PathBuf, StatPrint>,
    ) -> Result<(), Skip> {
        // The backups describe the worktree as it was a moment ago. If anything
        // changed while they were written, stop.
        let list = git::worktree_list(self.paths, &f.path).map_err(Self::git_skip)?;
        let now_head = list
            .iter()
            .find(|e| e.path.canonicalize().ok().as_deref() == Some(f.path.as_path()))
            .and_then(|e| e.head.clone());
        if now_head.as_deref() != Some(f.head.as_str()) {
            return skip(SkipReason::HeadMoved, "HEAD moved while backing up");
        }
        let status = git::git(
            self.paths,
            &f.path,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=all",
            ],
        )
        .map_err(Self::git_skip)?;
        if status != f.status {
            return skip(
                SkipReason::ChangedDuringBackup,
                "files changed while backing up; nothing was removed",
            );
        }
        // Status does not see a second edit to a dirty file or any edit to an
        // ignored one. Compare stat fingerprints, the set of ignored entries, and
        // (when a patch was written) the tree a fresh `add -A` produces now.
        if fingerprint(f).as_ref() != Some(before) {
            return skip(
                SkipReason::ChangedDuringBackup,
                "changed or ignored files were modified while backing up; nothing was removed",
            );
        }
        let (ignored_now, _) = ignored_to_archive(self.paths, &f.path)?;
        if ignored_now != f.ignored {
            return skip(
                SkipReason::ChangedDuringBackup,
                "ignored files appeared or disappeared while backing up; nothing was removed",
            );
        }
        if let (Some(tree), Some(dir)) = (&set.tree, &set.dir) {
            let index = dir.join("recheck.index.tmp");
            let now = self.staged_tree(f, &index);
            let _ = std::fs::remove_file(&index);
            match now {
                Ok(t) if &t == tree => {}
                Ok(_) => {
                    return skip(
                        SkipReason::ChangedDuringBackup,
                        "file contents changed while backing up; nothing was removed",
                    )
                }
                Err(e) => {
                    return skip(
                        SkipReason::ChangedDuringBackup,
                        format!("could not recheck the worktree after the backup: {e}"),
                    )
                }
            }
        }
        self.busy_check_with(&f.path, false, true)
    }

    /// Write every backup the worktree needs. Any failure fails the whole
    /// step, and a backup set left half written is discarded (the worktree is
    /// untouched; the error says so).
    fn back_up(&self, f: &WorktreeFacts) -> Result<BackupSet, String> {
        let mut out = Vec::new();
        let mut set_dir = None;
        let mut tree = None;
        let needs_dir = f.dirty_files > 0 || !f.ignored.is_empty();
        if needs_dir {
            let dir = self.backup_set_dir(f)?;
            match self.fill_backup_set(f, &dir, &mut out) {
                Ok(t) => tree = t,
                Err(e) => return Err(format!("{e}{}", self.discard_backup_set(Some(&dir)))),
            }
            set_dir = Some(dir);
        }
        if f.detached {
            match self.ensure_backup_branch(f) {
                Ok(b) => out.push(Backup {
                    kind: BackupKind::Branch,
                    location: b,
                }),
                Err(e) => {
                    return Err(format!(
                        "{e}{}",
                        self.discard_backup_set(set_dir.as_deref())
                    ))
                }
            }
        }
        Ok(BackupSet {
            backups: out,
            dir: set_dir,
            tree,
        })
    }

    /// The patch, the archive of ignored files and the manifest, written into
    /// `dir`. Returns the tree the patch was made from, when there is one.
    fn fill_backup_set(
        &self,
        f: &WorktreeFacts,
        dir: &Path,
        out: &mut Vec<Backup>,
    ) -> Result<Option<String>, String> {
        let mut tree = None;
        if f.dirty_files > 0 {
            let (p, t) = self.write_patch(f, dir)?;
            tree = Some(t);
            out.push(Backup {
                kind: BackupKind::Patch,
                location: p.display().to_string(),
            });
        }
        if !f.ignored.is_empty() {
            let p = write_ignored_archive(f, dir)?;
            out.push(Backup {
                kind: BackupKind::IgnoredArchive,
                location: p.display().to_string(),
            });
        }
        let manifest = serde_json::json!({
            "worktree": f.path,
            "repo": f.repo,
            "head": f.head,
            "branch": f.branch,
            "created_unix": self.now,
            "backups": out,
            "restore": "git worktree add <path> <head>; cd <path>; git apply --index changes.patch; tar -xzf ignored.tar.gz",
        });
        create_private_file(
            &dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        )
        .map_err(|e| format!("could not write the backup manifest: {e}"))?;
        Ok(tree)
    }

    /// A new directory `<backups>/<repo>-<name>-<now>-<random>`, mode 0700, created
    /// exclusively.
    fn backup_set_dir(&self, f: &WorktreeFacts) -> Result<PathBuf, String> {
        use std::os::unix::fs::DirBuilderExt;
        create_private_dir(&self.paths.backups_dir)
            .map_err(|e| format!("backups directory unavailable: {e}"))?;
        for _ in 0..8 {
            let r = random_hex(6).map_err(|e| e.to_string())?;
            let d = self.paths.backups_dir.join(format!(
                "{}-{}-{}-{r}",
                sanitize(&f.repo_name),
                sanitize(&f.name),
                self.now
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&d) {
                Ok(()) => return Ok(d),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("could not create a backup directory: {e}")),
            }
        }
        Err("could not pick a unique backup directory".into())
    }

    /// Stage everything (`add -A`) into a private copy of the worktree index at
    /// `index` and return the tree it describes. The caller removes `index`.
    fn staged_tree(&self, f: &WorktreeFacts, index: &Path) -> Result<String, String> {
        let real_index = git::git(self.paths, &f.path, &["rev-parse", "--git-path", "index"])
            .map_err(|e| e.to_string())?;
        let real_index = PathBuf::from(real_index.trim());
        let real_index = if real_index.is_absolute() {
            real_index
        } else {
            f.path.join(real_index)
        };
        let bytes = std::fs::read(&real_index)
            .map_err(|e| format!("could not read the worktree index: {e}"))?;
        create_private_file(index, &bytes).map_err(|e| e.to_string())?;
        Git::new(self.paths, &f.path)
            .env("GIT_INDEX_FILE", index)
            .args(["add", "-A"])
            .timeout(BACKUP_TIMEOUT)
            .run()
            .map_err(|e| format!("could not stage a backup: {e}"))?;
        let tree = Git::new(self.paths, &f.path)
            .env("GIT_INDEX_FILE", index)
            .args(["write-tree"])
            .timeout(BACKUP_TIMEOUT)
            .text()
            .map_err(|e| format!("could not record the backed-up tree: {e}"))?;
        Ok(tree.trim().to_string())
    }

    /// `changes.patch` (HEAD → everything in the worktree, `add -A`) and the tree
    /// it was made from.
    fn write_patch(&self, f: &WorktreeFacts, dir: &Path) -> Result<(PathBuf, String), String> {
        let index = dir.join("index.tmp");
        let res = (|| {
            let tree = self.staged_tree(f, &index)?;
            Git::new(self.paths, &f.path)
                .env("GIT_INDEX_FILE", &index)
                .args([
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "HEAD",
                ])
                .timeout(BACKUP_TIMEOUT)
                .run()
                .map_err(|e| format!("could not write the patch: {e}"))
                .map(|o| (o, tree))
        })();
        let _ = std::fs::remove_file(&index);
        let (out, tree) = res?;
        if out.stdout.is_empty() {
            return Err(format!(
                "{} file(s) are changed but the patch came out empty",
                f.dirty_files
            ));
        }
        if patch_has_gitlink(&out.stdout) {
            return Err(
                "the patch records a nested repository (a gitlink), not its contents".into(),
            );
        }
        let p = dir.join("changes.patch");
        create_private_file(&p, &out.stdout).map_err(|e| e.to_string())?;
        #[cfg(test)]
        if let Some(h) = &self.before_verify {
            h(&p);
        }
        self.verify_patch(f, dir, &p)?;
        Ok((p, tree))
    }

    /// The patch must apply cleanly to HEAD: `git apply --cached --check` against
    /// a fresh index read from HEAD. A patch that does not verify is a failed
    /// backup, so nothing is removed.
    fn verify_patch(&self, f: &WorktreeFacts, dir: &Path, patch: &Path) -> Result<(), String> {
        let index = dir.join("verify.index.tmp");
        let res = (|| {
            Git::new(self.paths, &f.path)
                .env("GIT_INDEX_FILE", &index)
                .args(["read-tree", &f.head])
                .timeout(BACKUP_TIMEOUT)
                .run()
                .map_err(|e| format!("could not prepare to verify the patch: {e}"))?;
            Git::new(self.paths, &f.path)
                .env("GIT_INDEX_FILE", &index)
                .args(["apply", "--cached", "--check", "--whitespace=nowarn"])
                .arg(patch)
                .timeout(BACKUP_TIMEOUT)
                .run()
                .map_err(|e| format!("the patch does not apply to {}: {e}", short(&f.head)))
        })();
        let _ = std::fs::remove_file(&index);
        res.map(|_| ())
    }

    fn ensure_backup_branch(&self, f: &WorktreeFacts) -> Result<String, String> {
        self.backup_branch_at(&f.repo, &f.name, &f.head)
    }

    /// Whether any ref (branch, tag, remote, stash…) reaches `commit`.
    fn reachable_from_a_ref(&self, repo: &Path, commit: &str) -> Result<bool, Skip> {
        let out = git::git(
            self.paths,
            repo,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "--contains",
                commit,
            ],
        )
        .map_err(Self::git_skip)?;
        Ok(!out.trim().is_empty())
    }

    /// `backup/<name>-<head>` at `head` in `repo`: created, or found already
    /// there at the same commit. Never moved.
    fn backup_branch_at(&self, repo: &Path, wt_name: &str, head: &str) -> Result<String, String> {
        let name = backup_branch_name(wt_name, head);
        let full = format!("refs/heads/{name}");
        match git::git(
            self.paths,
            repo,
            &["rev-parse", "--verify", "--quiet", &full],
        ) {
            Ok(existing) if existing.trim() == head => return Ok(name),
            Ok(existing) => {
                return Err(format!(
                    "branch {name} already exists at {}; it is never moved",
                    short(existing.trim())
                ))
            }
            Err(git::GitError::Failed { code: Some(1), .. }) => {}
            Err(e) => return Err(e.to_string()),
        }
        git::git(self.paths, repo, &["branch", &name, head])
            .map_err(|e| format!("could not create branch {name}: {e}"))?;
        Ok(name)
    }

    // ------------------------------------------------------------------ prune

    fn check_prune(&self, repo: &Path, path: &Path) -> Result<PruneFacts, Skip> {
        if !repo.is_absolute() || !path.is_absolute() {
            return skip(SkipReason::NotAbsolute, "paths must be absolute");
        }
        let repo = repo.canonicalize().map_err(|e| Skip {
            reason: SkipReason::NotFound,
            detail: format!("repository: {e}"),
        })?;
        if !self.canonical_roots().iter().any(|r| repo.starts_with(r)) {
            return skip(
                SkipReason::OutsideScanRoots,
                "the repository is not under a scan root",
            );
        }
        let list = git::worktree_list(self.paths, &repo).map_err(Self::git_skip)?;
        let Some(entry) = list.iter().skip(1).find(|e| e.path == path).cloned() else {
            return skip(SkipReason::NotAWorktree, "git does not list this worktree");
        };
        if !entry.prunable {
            return skip(
                SkipReason::NotPrunable,
                "git does not mark this worktree prunable",
            );
        }
        if entry.locked {
            return skip(SkipReason::Locked, "locked by git");
        }
        if !is_agent_worktree(&entry.path) {
            return skip(
                SkipReason::NotAgent,
                "only agent worktree entries are pruned",
            );
        }
        let repo_name = file_name(list.first().map(|m| m.path.as_path()).unwrap_or(&repo));
        Ok(PruneFacts {
            repo,
            entry,
            repo_name,
        })
    }

    fn prune_worktree(&self, repo: &Path, path: &Path) -> Result<Outcome, Skip> {
        let f = self.check_prune(repo, path)?;
        // `git worktree prune` deletes the entry's HEAD and reflog. A detached
        // HEAD that no ref reaches would become unreachable (and later garbage
        // collected): keep it on a backup branch first, or do not prune.
        let mut backups = Vec::new();
        if f.entry.detached {
            let Some(head) = f.entry.head.as_deref() else {
                return skip(SkipReason::GitFailed, "git did not report the entry's HEAD");
            };
            if !self.reachable_from_a_ref(&f.repo, head)? {
                let name = file_name(&f.entry.path);
                let b = self
                    .backup_branch_at(&f.repo, &name, head)
                    .map_err(|e| Skip {
                        reason: SkipReason::BackupFailed,
                        detail: format!("{e}; nothing was pruned"),
                    })?;
                backups.push(Backup {
                    kind: BackupKind::Branch,
                    location: b,
                });
            }
        }
        // `git worktree prune` has no per-entry form. Lock every other prunable
        // entry so only the requested one can go, then unlock them.
        let list = git::worktree_list(self.paths, &f.repo).map_err(Self::git_skip)?;
        let mut held: Vec<PathBuf> = Vec::new();
        let mut lock_err = None;
        for e in list
            .iter()
            .skip(1)
            .filter(|e| e.prunable && !e.locked && e.path != f.entry.path)
        {
            match Git::new(self.paths, &f.repo)
                .args([
                    "worktree",
                    "lock",
                    "--reason",
                    "mulchie: held during a prune",
                ])
                .arg(&e.path)
                .run()
            {
                Ok(_) => held.push(e.path.clone()),
                Err(err) => {
                    lock_err = Some(err);
                    break;
                }
            }
        }
        let res = match lock_err {
            Some(e) => Err(Skip {
                reason: SkipReason::GitFailed,
                detail: format!("could not hold other stale entries: {e}"),
            }),
            None => Git::new(self.paths, &f.repo)
                .args(["worktree", "prune"])
                .arg(format!(
                    "--expire={}.days.ago",
                    self.config.worktree_idle_days
                ))
                .run()
                .map(|_| ())
                .map_err(Self::git_skip),
        };
        // An entry left locked by a failed unlock is protected, not lost; its lock
        // reason names Mulchie.
        for p in &held {
            let _ = Git::new(self.paths, &f.repo)
                .args(["worktree", "unlock"])
                .arg(p)
                .run();
        }
        res?;
        let after = git::worktree_list(self.paths, &f.repo).map_err(Self::git_skip)?;
        if after.iter().any(|e| e.path == f.entry.path) {
            return skip(
                SkipReason::PruneDeferred,
                format!(
                    "git kept the entry: it is newer than the {}-day expiry{}",
                    self.config.worktree_idle_days,
                    backups
                        .first()
                        .map(|b| format!(" (its commit is kept on branch {})", b.location))
                        .unwrap_or_default()
                ),
            );
        }
        Ok(Outcome::Done {
            freed_bytes: 0,
            backups,
        })
    }

    // ----------------------------------------------------------------- signal

    fn check_signal(&self, pid: u32, expected_start_time: u64) -> Result<ProcFacts, Skip> {
        if pid <= 1 || pid > i32::MAX as u32 {
            return skip(
                SkipReason::ProtectedProcess,
                "pid 0, pid 1 and pids that are not positive i32 values are never signalled",
            );
        }
        if pid == self.procs.self_pid() {
            return skip(SkipReason::ProtectedProcess, "Mulchie never signals itself");
        }
        let Some(p) = self.procs.get(pid) else {
            return skip(SkipReason::ProcessGone, "no such process");
        };
        if p.uid != Some(self.procs.current_uid()) {
            return skip(
                SkipReason::ForeignProcess,
                "process belongs to another user",
            );
        }
        if is_mulchie_process(&p) {
            return skip(
                SkipReason::ProtectedProcess,
                format!("{} is a Mulchie process", p.name),
            );
        }
        if let Some(r) = crate::consumers::protected_reason(
            p.pid,
            &p.name,
            p.exe.as_deref(),
            self.procs.self_pid(),
        ) {
            return skip(SkipReason::ProtectedProcess, r);
        }
        if let Some(r) = self.user_service(&p) {
            return skip(SkipReason::ProtectedProcess, r);
        }
        if self.engine_ancestors().contains(&pid) {
            return skip(
                SkipReason::ProtectedProcess,
                format!(
                    "{} started Mulchie; signalling it would stop Mulchie too",
                    p.name
                ),
            );
        }
        if p.start_time != expected_start_time {
            return skip(
                SkipReason::StartTimeMismatch,
                "the pid now belongs to a different process",
            );
        }
        Ok(p)
    }

    /// The engine's parent, grandparent, … (not pid 1).
    fn engine_ancestors(&self) -> Vec<u32> {
        let view = self.procs_view(false);
        let ppid: HashMap<u32, u32> = view.iter().map(|(p, _)| (p.pid, p.ppid)).collect();
        let me = self.procs.self_pid();
        // The view leaves out the engine itself: ask for it directly.
        let mut cur = self.procs.get(me).map(|p| p.ppid).unwrap_or(0);
        let mut out = Vec::new();
        while cur > 1 && out.len() < 32 {
            out.push(cur);
            cur = ppid.get(&cur).copied().unwrap_or(0);
        }
        out
    }

    /// The process may be signalled, and `nice` is a lowering. Returns the
    /// facts and the current nice value.
    fn check_renice(
        &self,
        pid: u32,
        nice: i32,
        expected_start_time: u64,
    ) -> Result<(ProcFacts, i32), Skip> {
        if !(0..=20).contains(&nice) {
            return skip(SkipReason::CommandFailed, "nice must be between 0 and 20");
        }
        let p = self.check_signal(pid, expected_start_time)?;
        let cur = crate::engine::nice_of(pid).ok_or(Skip {
            reason: SkipReason::ProcessGone,
            detail: "could not read the priority".into(),
        })?;
        if cur >= nice {
            return skip(
                SkipReason::CommandFailed,
                format!("already at nice {cur}; Mulchie only lowers priority"),
            );
        }
        Ok((p, cur))
    }

    fn renice(&self, pid: u32, nice: i32, expected_start_time: u64) -> Result<Outcome, Skip> {
        self.check_renice(pid, nice, expected_start_time)?;
        crate::engine::set_nice(pid, nice).map_err(|e| Skip {
            reason: SkipReason::SignalFailed,
            detail: e.to_string(),
        })?;
        Ok(Outcome::Done {
            freed_bytes: 0,
            backups: Vec::new(),
        })
    }

    fn check_reopen(&self, bundle_id: &str) -> Result<(), Skip> {
        if !cfg!(target_os = "macos") {
            return skip(
                SkipReason::CommandFailed,
                "reopening an app needs macOS (open -b)",
            );
        }
        if !crate::consumers::valid_bundle_id(bundle_id) {
            return skip(SkipReason::CommandFailed, "not a valid bundle id");
        }
        Ok(())
    }

    /// Wait for the app's main process to exit (a `Signal` earlier in the
    /// same batch quit it), then `open -b`. The wait does not hold the
    /// execution lock; the facts are checked again under it before `open`.
    fn reopen_app(
        &self,
        bundle_id: &str,
        after_pid: u32,
        after_start_time: u64,
        quit: &[QuitApp],
    ) -> Result<Outcome, Skip> {
        self.check_reopen(bundle_id)?;
        let quit_here = quit.iter().any(|q| {
            q.pid == after_pid
                && q.start_time == after_start_time
                && q.bundle_id.as_deref() == Some(bundle_id)
        });
        if !quit_here {
            return skip(
                SkipReason::ReopenNotQuit,
                format!(
                    "no process of {bundle_id} (pid {after_pid}) was quit earlier in this batch"
                ),
            );
        }
        let running = || {
            self.procs
                .get(after_pid)
                .is_some_and(|p| p.start_time == after_start_time)
        };
        let deadline = Instant::now() + self.reopen_wait;
        while running() {
            if Instant::now() >= deadline {
                return skip(
                    SkipReason::TimedOut,
                    format!(
                        "pid {after_pid} did not exit within {}s; not reopened",
                        self.reopen_wait.as_secs()
                    ),
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let _g = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.check_reopen(bundle_id)?;
        if running() {
            return skip(
                SkipReason::Busy,
                format!("pid {after_pid} is running again; not reopened"),
            );
        }
        let mut cmd = std::process::Command::new("/usr/bin/open");
        cmd.arg("-b").arg(bundle_id);
        match git::run_with_timeout(cmd, Duration::from_secs(15)) {
            Ok(o) if o.success => Ok(Outcome::Done {
                freed_bytes: 0,
                backups: Vec::new(),
            }),
            Ok(o) => skip(
                SkipReason::CommandFailed,
                format!("open -b failed: {}", o.stderr.trim()),
            ),
            Err(e) => skip(SkipReason::CommandFailed, format!("open -b: {e}")),
        }
    }

    fn send_signal(
        &self,
        pid: u32,
        signal: Signal,
        expected_start_time: u64,
    ) -> Result<Outcome, Skip> {
        self.check_signal(pid, expected_start_time)?;
        let rc = unsafe { libc::kill(pid as i32, signal.number()) };
        if rc != 0 {
            return skip(
                SkipReason::SignalFailed,
                std::io::Error::last_os_error().to_string(),
            );
        }
        Ok(Outcome::Done {
            freed_bytes: 0,
            backups: Vec::new(),
        })
    }
}

// ----------------------------------------------------------------- helpers

/// Whether `path` is a recognised build cache under the config's flags.
pub fn recognise_cache(path: &Path, cfg: &Config) -> Result<ItemKind, Skip> {
    let parent = path.parent().unwrap_or(path);
    match file_name(path).as_str() {
        "target" if parent.join("Cargo.toml").is_file() => {
            if has_cachedir_tag(path) {
                Ok(ItemKind::CargoTarget)
            } else {
                skip(
                    SkipReason::MissingCachedirTag,
                    "target/ has no CACHEDIR.TAG, so it may not be Cargo's",
                )
            }
        }
        "node_modules" if parent.join("package.json").is_file() => {
            if cfg.include_node_modules {
                Ok(ItemKind::NodeModules)
            } else {
                skip(
                    SkipReason::NodeModulesDisabled,
                    "include_node_modules is off",
                )
            }
        }
        ".next" if parent.join("package.json").is_file() => Ok(ItemKind::NextCache),
        _ => skip(
            SkipReason::NotRecognisedCache,
            "not a recognised build cache",
        ),
    }
}

fn has_cachedir_tag(dir: &Path) -> bool {
    let p = dir.join("CACHEDIR.TAG");
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.is_file() => {}
        _ => return false,
    }
    std::fs::read(&p)
        .map(|b| b.starts_with(CACHEDIR_TAG_SIGNATURE.as_bytes()))
        .unwrap_or(false)
}

/// The checkout a cache belongs to: the nearest directory at or above `dir`, and
/// not above `limit`, that has a `.git`; else `dir` itself. The flag says the
/// checkout is a linked worktree (`.git` is a file).
pub fn project_root(dir: &Path, limit: &Path) -> (PathBuf, bool) {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if !d.starts_with(limit) {
            break;
        }
        if let Ok(m) = std::fs::symlink_metadata(d.join(".git")) {
            return (d.to_path_buf(), m.is_file());
        }
        cur = d.parent();
    }
    (dir.to_path_buf(), false)
}

/// Whether `cwd` counts as working in `root`: a component-wise prefix match.
/// With `nested`, a `.git` strictly between `root` and `cwd` means `cwd` belongs to
/// that inner checkout instead.
pub fn cwd_in(cwd: &Path, root: &Path, nested: bool) -> bool {
    if !cwd.starts_with(root) {
        return false;
    }
    if !nested {
        return true;
    }
    let mut cur = Some(cwd);
    while let Some(d) = cur {
        if d == root {
            return true;
        }
        if std::fs::symlink_metadata(d.join(".git")).is_ok() {
            return false;
        }
        cur = d.parent();
    }
    true
}

/// The first own process (other than Mulchie itself) working at or under `root`,
/// with working directories read now and canonicalized.
pub fn busy_process(src: &dyn ProcessSource, root: &Path, nested: bool) -> Option<ProcFacts> {
    let uid = src.current_uid();
    let me = src.self_pid();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    src.list().into_iter().find(|p| {
        if p.uid != Some(uid) || p.pid == me {
            return false;
        }
        let Some(cwd) = &p.cwd else { return false };
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
        cwd_in(&cwd, &root, nested)
    })
}

/// Whether any of `cwds` is at or under `root` (see [`cwd_in`]).
pub fn any_cwd_in(cwds: &[PathBuf], root: &Path, nested: bool) -> bool {
    cwds.iter().any(|c| cwd_in(c, root, nested))
}

/// Agent worktrees: named `agent-*`, or under a `.claude/worktrees/` directory.
pub fn is_agent_worktree(path: &Path) -> bool {
    if file_name(path).starts_with("agent-") {
        return true;
    }
    let comps: Vec<Component> = path.components().collect();
    comps
        .windows(2)
        .any(|w| w[0].as_os_str() == ".claude" && w[1].as_os_str() == "worktrees")
        && comps.len() >= 3
}

pub fn is_mulchie_process(p: &ProcFacts) -> bool {
    let starts = |s: &str| s.to_ascii_lowercase().starts_with(PROTECTED_PREFIX);
    starts(&p.name)
        || p.exe
            .as_deref()
            .map(|e| starts(&file_name(e)))
            .unwrap_or(false)
        || p.cmd
            .first()
            .map(|a| starts(a.rsplit('/').next().unwrap_or(a)))
            .unwrap_or(false)
}

/// Whether `pid` exists (signal 0). `EPERM` means it exists but belongs to
/// someone else, so it counts as alive. Values that would be negative as a
/// `pid_t` (process groups, or "every process" for -1) are never alive.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `backup/<name>-<12-char sha>`.
pub fn backup_branch_name(name: &str, head: &str) -> String {
    format!("backup/{}-{}", sanitize(name), short(head))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Safe for file names and branch names: `[A-Za-z0-9._-]`, no leading dot or dash.
fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let out = out.trim_start_matches(['.', '-']).trim_end_matches('.');
    let out = out.strip_suffix(".lock").unwrap_or(out);
    if out.is_empty() {
        "worktree".into()
    } else {
        out.to_string()
    }
}

fn create_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// The first checked-out submodule in `wt`, if any.
fn initialized_submodule(paths: &Paths, wt: &Path) -> Result<Option<String>, Skip> {
    if !wt.join(".gitmodules").is_file() {
        return Ok(None);
    }
    let out = match git::git(
        paths,
        wt,
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    ) {
        Ok(o) => o,
        Err(git::GitError::Failed { code: Some(1), .. }) => return Ok(None),
        Err(e) => return Err(Gate::git_skip(e)),
    };
    for line in out.lines() {
        if let Some((_, p)) = line.split_once(' ') {
            if std::fs::symlink_metadata(wt.join(p).join(".git")).is_ok() {
                return Ok(Some(p.to_string()));
            }
        }
    }
    Ok(None)
}

/// At most this many directory entries are looked at for nested repositories;
/// more than that and the check cannot complete, which is a skip.
const NESTED_SCAN_BUDGET: usize = 1_000_000;

/// A git repository inside `wt` that removing the worktree would destroy: a
/// gitlink in the index whose directory holds a `.git`, or a `.git` (directory
/// or file) at any depth under an untracked or ignored entry that is not
/// provably disposable (see [`nested_disposable`]). Where the repository sits
/// never exempts it: one inside an ignored `build/` or `node_modules/` is
/// checked like any other. Returns the repository and why it blocks.
fn nested_repo(paths: &Paths, wt: &Path) -> Result<Option<String>, Skip> {
    // Gitlinks recorded in the index, with or without `.gitmodules`.
    let staged = Git::new(paths, wt)
        .args(["ls-files", "-z", "--stage"])
        .run()
        .map_err(Gate::git_skip)?;
    for raw in staged.stdout.split(|b| *b == 0) {
        let Ok(line) = std::str::from_utf8(raw) else {
            continue;
        };
        if !line.starts_with("160000 ") {
            continue;
        }
        if let Some((_, p)) = line.split_once('\t') {
            if std::fs::symlink_metadata(wt.join(p).join(".git")).is_ok() {
                return Ok(Some(p.to_string()));
            }
        }
    }
    // Untracked and ignored entries (no exclude rules: everything not tracked),
    // directories collapsed.
    let out = Git::new(paths, wt)
        .args(["ls-files", "-z", "--others", "--directory"])
        .run()
        .map_err(Gate::git_skip)?;
    let mut budget = NESTED_SCAN_BUDGET;
    let mut found: Vec<PathBuf> = Vec::new();
    for raw in out.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        use std::os::unix::ffi::OsStrExt;
        let rel = Path::new(std::ffi::OsStr::from_bytes(raw));
        if rel.file_name().is_some_and(|n| n == ".git") {
            found.push(wt.join(rel).parent().unwrap_or(wt).to_path_buf());
            continue;
        }
        let abs = wt.join(rel);
        let Ok(m) = std::fs::symlink_metadata(&abs) else {
            continue;
        };
        if !m.is_dir() {
            continue;
        }
        let mut stack = vec![abs];
        while let Some(d) = stack.pop() {
            let rd = std::fs::read_dir(&d).map_err(|e| Skip {
                reason: SkipReason::NestedRepo,
                detail: format!(
                    "could not check {} for nested repositories: {e}",
                    d.display()
                ),
            })?;
            for e in rd.flatten() {
                budget = budget.saturating_sub(1);
                if budget == 0 {
                    return skip(
                        SkipReason::NestedRepo,
                        "too many untracked or ignored files to check for nested repositories",
                    );
                }
                let name = e.file_name();
                if name == ".git" {
                    found.push(d.clone());
                    if found.len() > MAX_NESTED_REPOS {
                        return skip(
                            SkipReason::NestedRepo,
                            format!(
                                "more than {MAX_NESTED_REPOS} nested repositories; too many to verify"
                            ),
                        );
                    }
                    continue;
                }
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    stack.push(e.path());
                }
            }
        }
    }
    for repo in found {
        if let Err(why) = nested_disposable(paths, &repo) {
            let shown = repo
                .strip_prefix(wt)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| repo.display().to_string());
            return Ok(Some(format!("{shown} ({why})")));
        }
    }
    Ok(None)
}

/// Nested repositories verified per worktree; more is a skip.
const MAX_NESTED_REPOS: usize = 32;

/// A nested repository is disposable only when nothing in it that exists
/// nowhere else would be lost: a clean working tree (no changes, no untracked
/// files), no ignored files apart from its own known cache directories, no
/// skip-worktree or assume-unchanged edits, no stashes, and every commit
/// reachable from its refs or its reflogs also reachable from one of its
/// remotes' refs. Anything that cannot be checked (a `.git` file pointing
/// elsewhere, git failing) is not disposable. Such a repository blocks the
/// removal; it is never archived instead.
fn nested_disposable(paths: &Paths, repo: &Path) -> Result<(), String> {
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let o = Git::new(paths, repo)
            .args(args.iter().copied())
            .run()
            .map_err(|e| format!("git could not read it: {e}"))?;
        Ok(o.stdout)
    };
    // The repository git sees must be this directory, not an enclosing one.
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let same = match (top.canonicalize(), repo.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err("git does not read it as its own repository".into());
    }
    if !git(&["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        return Err("it has uncommitted changes or untracked files".into());
    }
    if !git(&["stash", "list"])?.is_empty() {
        return Err("it has stashes".into());
    }
    if !git(&["rev-list", "--all", "--not", "--remotes", "--max-count=1"])?.is_empty() {
        return Err("it has commits that are on none of its remotes".into());
    }
    if !git(&[
        "rev-list",
        "--reflog",
        "--not",
        "--remotes",
        "--max-count=1",
    ])?
    .is_empty()
    {
        return Err(
            "its reflog holds commits that are on none of its remotes (reset or amended away)"
                .into(),
        );
    }
    let (ignored, _) = ignored_to_archive(paths, repo).map_err(|s| s.detail)?;
    if let Some(first) = ignored.first() {
        return Err(format!(
            "it has {} ignored file(s) or folder(s) that are not build caches, e.g. {first}",
            ignored.len()
        ));
    }
    if let Some((n, first)) = hidden_changes(paths, repo).map_err(|s| s.detail)? {
        return Err(format!(
            "it has {n} skip-worktree or assume-unchanged file(s) with local edits, e.g. {first}"
        ));
    }
    Ok(())
}

/// Files marked skip-worktree (`S`) or assume-unchanged (a lowercase tag) whose
/// content on disk differs from the index. git hides these from `status`,
/// `add -A` and `ls-files --others`, so a patch would not hold them. Returns the
/// count and the first path. A sparse-checkout entry that is not on disk is no
/// change.
fn hidden_changes(paths: &Paths, wt: &Path) -> Result<Option<(usize, String)>, Skip> {
    let out = Git::new(paths, wt)
        .args(["ls-files", "-z", "-v", "-s"])
        .run()
        .map_err(Gate::git_skip)?;
    // (path, index blob, mode)
    let mut flagged: Vec<(String, String, String)> = Vec::new();
    for raw in out.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let Ok(line) = std::str::from_utf8(raw) else {
            // A non-UTF-8 name with a hidden flag cannot be checked: skip.
            if raw
                .first()
                .is_some_and(|t| *t == b'S' || t.is_ascii_lowercase())
            {
                return skip(
                    SkipReason::HiddenChanges,
                    "a skip-worktree or assume-unchanged file name is not UTF-8",
                );
            }
            continue;
        };
        let tag = line.as_bytes()[0];
        if !(tag == b'S' || tag.is_ascii_lowercase()) {
            continue;
        }
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut f = meta.split(' ');
        let (_, mode, blob) = (f.next(), f.next().unwrap_or(""), f.next().unwrap_or(""));
        if std::fs::symlink_metadata(wt.join(path)).is_err() {
            continue;
        }
        flagged.push((path.to_string(), blob.to_string(), mode.to_string()));
    }
    let mut changed = Vec::new();
    for chunk in flagged.chunks(256) {
        let (files, links): (Vec<_>, Vec<_>) =
            chunk.iter().partition(|(_, _, m)| m.starts_with("100"));
        // A symbolic link, a gitlink or anything not a regular file cannot be
        // compared by hash-object; it counts as changed.
        changed.extend(links.iter().map(|(p, _, _)| p.clone()));
        if files.is_empty() {
            continue;
        }
        let hashed = Git::new(paths, wt)
            .args(["hash-object", "--"])
            .args(files.iter().map(|(p, _, _)| p.as_str()))
            .timeout(BACKUP_TIMEOUT)
            .text()
            .map_err(Gate::git_skip)?;
        let got: Vec<&str> = hashed.lines().collect();
        if got.len() != files.len() {
            return skip(
                SkipReason::HiddenChanges,
                "could not compare skip-worktree or assume-unchanged files",
            );
        }
        for ((p, blob, _), h) in files.iter().zip(got) {
            if h.trim() != blob {
                changed.push(p.clone());
            }
        }
    }
    Ok(changed.first().cloned().map(|f| (changed.len(), f)))
}

/// Whether a binary patch records a gitlink (mode 160000): such a patch holds
/// only a commit id, not the nested repository's history or files.
fn patch_has_gitlink(patch: &[u8]) -> bool {
    patch.split(|b| *b == b'\n').any(|l| {
        let Ok(l) = std::str::from_utf8(l) else {
            return false;
        };
        let header = l.starts_with("new file mode ")
            || l.starts_with("deleted file mode ")
            || l.starts_with("old mode ")
            || l.starts_with("new mode ")
            || l.starts_with("index ");
        header && l.trim_end().ends_with(" 160000")
    })
}

/// Gitignored entries in `wt` that are not under a [`KNOWN_CACHE_DIRS`] directory,
/// and their total apparent size.
fn ignored_to_archive(paths: &Paths, wt: &Path) -> Result<(Vec<String>, u64), Skip> {
    let out = Git::new(paths, wt)
        .args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ])
        .run()
        .map_err(Gate::git_skip)?;
    let mut keep = Vec::new();
    let mut bytes = 0u64;
    for raw in out.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(raw) else {
            return skip(
                SkipReason::BackupFailed,
                "an ignored file name is not UTF-8 and cannot be archived safely",
            );
        };
        let rel_path = Path::new(rel.trim_end_matches('/'));
        if rel_path
            .components()
            .any(|c| KNOWN_CACHE_DIRS.iter().any(|k| c.as_os_str() == *k))
        {
            continue;
        }
        bytes += apparent_size(&wt.join(rel_path));
        keep.push(rel.trim_end_matches('/').to_string());
    }
    keep.sort();
    Ok((keep, bytes))
}

/// Sum of file lengths under `p`, not following symlinks.
fn apparent_size(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

/// `tar.gz` of the ignored entries, written to a file we created exclusively.
fn write_ignored_archive(f: &WorktreeFacts, dir: &Path) -> Result<PathBuf, String> {
    let list = dir.join("ignored.list");
    let mut data = Vec::new();
    for e in &f.ignored {
        // `./` first: GNU tar reads a `-T` entry beginning with `-` as an
        // option (bsdtar does not); `./-x` is always a name.
        data.extend_from_slice(b"./");
        data.extend_from_slice(e.as_bytes());
        data.push(0);
    }
    create_private_file(&list, &data).map_err(|e| e.to_string())?;
    let archive = dir.join("ignored.tar.gz");
    create_private_file(&archive, b"").map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("tar");
    cmd.env("COPYFILE_DISABLE", "1")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&f.path)
        .arg("--null")
        .arg("-T")
        .arg(&list);
    let res = git::run_with_timeout(cmd, BACKUP_TIMEOUT);
    let _ = std::fs::remove_file(&list);
    match res {
        Ok(o) if o.success => {}
        Ok(o) => return Err(format!("tar failed: {}", o.stderr.trim())),
        Err(e) => return Err(format!("tar: {e}")),
    }
    let mut check = std::process::Command::new("tar");
    check.arg("-tzf").arg(&archive);
    match git::run_with_timeout(check, BACKUP_TIMEOUT) {
        Ok(o) if o.success && !o.stdout.is_empty() => Ok(archive),
        Ok(o) => Err(format!("archive did not verify: {}", o.stderr.trim())),
        Err(e) => Err(format!("archive did not verify: {e}")),
    }
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
