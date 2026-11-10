//! The engine behind the app's commands.
//!
//! One engine runs per user (the engine lock). When the app takes the lock it
//! runs that engine itself and serves the HTTP API. When another engine holds
//! it (usually `mulchie serve` as a service), the app is a client of that
//! engine through [`api::Client`] for state and for every action. If that
//! engine goes away, the app shows itself disconnected, reconnects when the
//! engine comes back, and takes the lock and becomes the engine if it stays
//! gone for [`Options::takeover_after`].
//!
//! Every method here blocks (remote calls, git, deletes); Tauri commands call
//! them on a blocking thread.

use mulchie_core::api::{self, ApiError, Client, Server};
use mulchie_core::engine::{ProcAction, Sample};
use mulchie_core::lock::{self, EngineLock, LockError};
use mulchie_core::process::ProcessSource;
use mulchie_core::procplan::ProcessPlanRequest;
use mulchie_core::{ActionResult, Config, Engine, LoadedConfig, Paths, Plan, Request};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How the app reaches the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// This process holds the engine lock and runs the engine.
    Engine,
    /// Another process runs the engine; the app is its client.
    Client,
    /// No engine is reachable. The app retries every few seconds.
    Disconnected,
}

/// What the UI shows about the connection.
#[derive(Debug, Clone, Serialize)]
pub struct ConnInfo {
    pub mode: Mode,
    /// The API port: ours in engine mode, the engine's in client mode.
    pub port: Option<u16>,
    /// The pid of the engine process (ours in engine mode).
    pub engine_pid: Option<u32>,
    /// In engine mode, why the API is not running; when disconnected, why.
    pub detail: Option<String>,
    /// When this mode began.
    pub since_unix: u64,
}

/// The kinds of destructive batch the UI can execute. Each accepts only the
/// requests of its own kind and only requests from a plan this backend issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Batch {
    Clean,
    Worktree,
    Sweep,
    /// Signals, renices and app reopens from a process or group plan.
    Process,
}

impl Batch {
    pub fn allows(self, r: &Request) -> bool {
        match self {
            Batch::Clean => matches!(r, Request::DeleteCache { .. }),
            Batch::Worktree | Batch::Sweep => matches!(
                r,
                Request::RemoveWorktree { .. } | Request::PruneWorktree { .. }
            ),
            Batch::Process => matches!(
                r,
                Request::Signal { .. } | Request::Renice { .. } | Request::ReopenApp { .. }
            ),
        }
    }

    fn route(self) -> &'static str {
        match self {
            Batch::Clean => "/v1/clean",
            Batch::Worktree => "/v1/worktree/remove",
            Batch::Sweep => "/v1/sweep",
            Batch::Process => "/v1/process/execute",
        }
    }
}

/// The ready requests of a process plan (`ProcessPlan` as JSON), in order.
pub fn ready_requests(plan: &Value) -> Vec<Request> {
    plan["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["ready"].as_bool() == Some(true))
        .filter_map(|i| serde_json::from_value(i["request"].clone()).ok())
        .collect()
}

/// What the UI asks a plan for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanRequest {
    /// Exactly these caches.
    Clean { paths: Vec<PathBuf> },
    /// Stale caches, up to `want_bytes` (all of them when absent).
    Stale {
        #[serde(default)]
        want_bytes: Option<u64>,
    },
    /// Exactly these worktrees.
    Worktrees {
        paths: Vec<PathBuf>,
        #[serde(default)]
        allow_dirty: bool,
    },
    /// Idle agent worktrees.
    Sweep,
}

impl PlanRequest {
    pub fn batch(&self) -> Batch {
        match self {
            PlanRequest::Clean { .. } | PlanRequest::Stale { .. } => Batch::Clean,
            PlanRequest::Worktrees { .. } => Batch::Worktree,
            PlanRequest::Sweep => Batch::Sweep,
        }
    }
}

/// The ready requests of the last plan issued for each batch kind (for
/// process plans, the last plan for each target: a pid, a group, or a bulk
/// selection reviewed as one). An execute must name only these, so the UI can
/// approve a subset of a plan but never add to it.
#[derive(Debug, Default)]
pub struct PlanBook {
    /// Each entry with the order it was recorded in.
    ready: HashMap<(Batch, String), (u64, Vec<Request>)>,
    seq: u64,
}

/// The most processes one bulk review may plan.
pub const MAX_BULK: usize = 500;

/// Process plans kept at once. Recording one more forgets only the oldest.
pub const PROCESS_PLANS_KEPT: usize = 32;

impl PlanBook {
    pub fn record(&mut self, batch: Batch, plan: &Plan) {
        self.seq += 1;
        self.ready
            .insert((batch, String::new()), (self.seq, plan.approved()));
    }

    /// Record the ready items of a process plan for its `target`, replacing
    /// that target's earlier plan. Beyond [`PROCESS_PLANS_KEPT`] the oldest
    /// process plan is forgotten, one at a time.
    pub fn record_process(&mut self, target: &str, ready: Vec<Request>) {
        self.seq += 1;
        self.ready
            .insert((Batch::Process, target.into()), (self.seq, ready));
        while self.ready.keys().filter(|k| k.0 == Batch::Process).count() > PROCESS_PLANS_KEPT {
            let oldest = self
                .ready
                .iter()
                .filter(|(k, _)| k.0 == Batch::Process)
                .min_by_key(|(_, v)| v.0)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.ready.remove(&k),
                None => break,
            };
        }
    }

    /// Forget every plan: the engine behind them changed (a takeover or a
    /// reconnect), so none was checked by the engine that would run it.
    pub fn clear(&mut self) {
        self.ready.clear();
    }

    /// Whether a process plan for `target` is kept.
    #[cfg(test)]
    pub fn has_process(&self, target: &str) -> bool {
        self.ready.contains_key(&(Batch::Process, target.into()))
    }

    /// Check `items` against the plans recorded for `batch`. On success the
    /// items leave the book, so the same approval cannot be sent twice.
    pub fn take(&mut self, batch: Batch, items: &[Request]) -> Result<(), String> {
        if items.is_empty() {
            return Err("nothing was selected".into());
        }
        if let Some(i) = items.iter().position(|r| !batch.allows(r)) {
            return Err(format!(
                "item {} ({}) does not belong in this kind of batch",
                i + 1,
                items[i].target()
            ));
        }
        let known = |r: &Request| {
            self.ready
                .iter()
                .any(|(k, (_, list))| k.0 == batch && list.contains(r))
        };
        if let Some(r) = items.iter().find(|r| !known(r)) {
            return Err(format!(
                "{} is not a ready item of the current plan; review the plan again",
                r.target()
            ));
        }
        for (k, (_, list)) in self.ready.iter_mut() {
            if k.0 == batch {
                list.retain(|r| !items.contains(r));
            }
        }
        Ok(())
    }
}

/// The figures the tray title and tooltip need, without the whole state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrayInfo {
    pub health: String,
    pub disk_free: u64,
    pub disk_total: u64,
    pub cpu_pct: f64,
    pub load1: f64,
    pub agents: u64,
    pub builds: u64,
    /// Below the alert threshold; `None` in client mode, where the engine that
    /// runs the service sends its own notifications.
    pub low_disk: Option<bool>,
}

impl TrayInfo {
    pub fn from_health(v: &Value) -> Self {
        Self {
            health: v["health"].as_str().unwrap_or("").into(),
            disk_free: v["disk_free"].as_u64().unwrap_or(0),
            disk_total: v["disk_total"].as_u64().unwrap_or(0),
            cpu_pct: v["cpu_pct"].as_f64().unwrap_or(0.0),
            load1: v["load"][0].as_f64().unwrap_or(0.0),
            agents: v["agents"].as_u64().unwrap_or(0),
            builds: v["builds"].as_u64().unwrap_or(0),
            low_disk: None,
        }
    }
}

/// Construction options. [`Options::default`] is what the app uses.
#[derive(Clone)]
pub struct Options {
    /// The API port for an engine this process runs; `None` means the
    /// config's (or `MULCHIE_PORT`). 0 picks a free port (tests).
    pub port: Option<u16>,
    /// Start the sample and scan loops of an engine this process runs.
    pub start_loops: bool,
    /// The process table for an engine this process runs (tests use a fake).
    pub procs: Option<Arc<dyn ProcessSource>>,
    /// The config for an engine this process runs, instead of the file
    /// (tests).
    pub config: Option<Config>,
    /// How long no engine must be reachable before this process takes the
    /// lock itself. A service restarting under launchd gets this long to come
    /// back first.
    pub takeover_after: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            port: None,
            start_loops: true,
            procs: None,
            config: None,
            takeover_after: Duration::from_secs(10),
        }
    }
}

#[derive(Clone)]
enum Conn {
    Local(Engine),
    Remote(Client),
    Down,
}

struct Inner {
    conn: Conn,
    info: ConnInfo,
    lock: Option<EngineLock>,
    server: Option<Server>,
    down_since: Option<Instant>,
    /// Consecutive soft failures (timeouts) of a remote engine.
    fails: u32,
}

pub struct Backend {
    paths: Paths,
    opts: Options,
    inner: Mutex<Inner>,
    plans: Mutex<PlanBook>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How a failed remote call bears on the connection: `None` when the engine
/// answered (the call itself failed), `Some(true)` when the connection is
/// gone for sure (nothing listens, or the token is rejected), `Some(false)`
/// for a timeout or I/O error, which counts only when it happens twice in a
/// row so a slow but healthy engine is not dropped.
pub fn failure_kind(e: &ApiError) -> Option<bool> {
    match (e.status, e.code.as_str()) {
        (0, "unreachable") => Some(true),
        (0, _) => Some(false),
        (401, _) => Some(true),
        _ => None,
    }
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn from_value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("unexpected answer from the engine: {e}"))
}

impl Backend {
    /// Take the engine lock and run the engine, or connect to the engine that
    /// holds it. Never fails: problems show as [`Mode::Disconnected`].
    pub fn start(paths: Paths, opts: Options) -> Self {
        let b = Self {
            paths,
            opts,
            inner: Mutex::new(Inner {
                conn: Conn::Down,
                info: ConnInfo {
                    mode: Mode::Disconnected,
                    port: None,
                    engine_pid: None,
                    detail: Some("starting".into()),
                    since_unix: now_unix(),
                },
                lock: None,
                server: None,
                down_since: Some(Instant::now()),
                fails: 0,
            }),
            plans: Mutex::new(PlanBook::default()),
        };
        {
            let mut inner = guard(&b.inner);
            match EngineLock::acquire(&b.paths, now_unix()) {
                Ok(lk) => b.become_engine(&mut inner, lk),
                Err(LockError::Held(h)) => b.connect(&mut inner, h.and_then(|h| h.port)),
                Err(e) => b.set_down(&mut inner, e.to_string()),
            }
        }
        b
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn info(&self) -> ConnInfo {
        guard(&self.inner).info.clone()
    }

    pub fn mode(&self) -> Mode {
        guard(&self.inner).info.mode
    }

    fn conn(&self) -> Conn {
        guard(&self.inner).conn.clone()
    }

    fn become_engine(&self, inner: &mut Inner, mut lk: EngineLock) {
        let loaded = match &self.opts.config {
            Some(c) => LoadedConfig {
                config: c.clone(),
                error: None,
            },
            None => Config::load_from(&self.paths),
        };
        let port = self
            .opts
            .port
            .unwrap_or_else(|| mulchie_core::config::effective_port(&loaded.config));
        let engine = match &self.opts.procs {
            Some(p) => Engine::with_processes(self.paths.clone(), loaded, p.clone()),
            None => Engine::new(self.paths.clone(), loaded),
        };
        if self.opts.start_loops {
            engine.start();
        }
        let (server, detail) = match api::serve(engine.clone(), port) {
            Ok(s) => {
                let _ = lk.set_port(Some(s.port()));
                (Some(s), None)
            }
            Err(e) => (
                None,
                Some(format!(
                    "the API could not start on port {port} ({e}); agents and the CLI cannot reach Mulchie"
                )),
            ),
        };
        inner.info = ConnInfo {
            mode: Mode::Engine,
            port: server.as_ref().map(Server::port),
            engine_pid: Some(std::process::id()),
            detail,
            since_unix: now_unix(),
        };
        inner.conn = Conn::Local(engine);
        guard(&self.plans).clear();
        inner.lock = Some(lk);
        inner.server = server;
        inner.down_since = None;
    }

    /// Reach the engine holding the lock, which announced `port`. Network
    /// I/O: never call it with `inner` locked.
    fn dial(&self, port: Option<u16>) -> Result<(Client, Option<u32>), String> {
        let holder = lock::running_engine(&self.paths);
        let port = port.or(holder.as_ref().and_then(|h| h.port));
        let Some(port) = port else {
            return Err(match &holder {
                Some(h) => format!("the engine (pid {}) has not opened its API yet", h.pid),
                None => "no Mulchie engine is running".into(),
            });
        };
        Client::connect_in(&self.paths, port).map(|c| (c, holder.map(|h| h.pid)))
    }

    fn apply_dial(&self, inner: &mut Inner, r: Result<(Client, Option<u32>), String>) {
        match r {
            Ok((c, pid)) => {
                inner.info = ConnInfo {
                    mode: Mode::Client,
                    port: Some(c.port()),
                    engine_pid: pid,
                    detail: None,
                    since_unix: now_unix(),
                };
                inner.conn = Conn::Remote(c);
                guard(&self.plans).clear();
                inner.down_since = None;
                inner.fails = 0;
            }
            Err(e) => self.set_down(inner, e),
        }
    }

    /// Connect to the engine holding the lock, which announced `port`.
    fn connect(&self, inner: &mut Inner, port: Option<u16>) {
        let r = self.dial(port);
        self.apply_dial(inner, r);
    }

    /// Connect without holding `inner` across the network I/O; applies the
    /// result only if nothing else changed the connection meanwhile.
    fn connect_unlocked(&self, port: Option<u16>) {
        let r = self.dial(port);
        let mut inner = guard(&self.inner);
        if matches!(inner.conn, Conn::Down) {
            self.apply_dial(&mut inner, r);
        }
    }

    fn set_down(&self, inner: &mut Inner, why: String) {
        if inner.info.mode != Mode::Disconnected || inner.down_since.is_none() {
            inner.down_since = Some(Instant::now());
            inner.info.since_unix = now_unix();
        }
        inner.info.mode = Mode::Disconnected;
        inner.info.detail = Some(why);
        inner.info.port = None;
        inner.info.engine_pid = None;
        inner.conn = Conn::Down;
        inner.fails = 0;
    }

    /// A remote call failed: drop the connection when [`failure_kind`] says
    /// it is gone (twice in a row for timeouts). The supervisor then
    /// reconnects, re-reading the token, or takes over.
    fn note_failure(&self, e: &ApiError) {
        let Some(hard) = failure_kind(e) else {
            return;
        };
        let mut inner = guard(&self.inner);
        if !matches!(inner.conn, Conn::Remote(_)) {
            return;
        }
        inner.fails += 1;
        if hard || inner.fails >= 2 {
            let why = if e.status == 401 {
                "the engine rejected this app's token; reconnecting with the current one"
                    .to_string()
            } else {
                e.message.clone()
            };
            self.set_down(&mut inner, format!("lost the engine: {why}"));
        }
    }

    fn note_ok(&self) {
        let mut inner = guard(&self.inner);
        if matches!(inner.conn, Conn::Remote(_)) {
            inner.fails = 0;
        }
    }

    /// One supervision step (every few seconds): check a remote engine with
    /// an authenticated call, and when disconnected, reconnect or take over.
    pub fn tick(&self) {
        match self.conn() {
            Conn::Local(_) => {}
            Conn::Remote(c) => match c.request("GET", "/v1/advice", None, api::HEALTH_TIMEOUT) {
                Ok(_) => self.note_ok(),
                Err(e) => self.note_failure(&e),
            },
            Conn::Down => {
                if let Some(h) = lock::running_engine(&self.paths) {
                    return self.connect_unlocked(h.port);
                }
                let waited = guard(&self.inner)
                    .down_since
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::MAX);
                if waited < self.opts.takeover_after {
                    return;
                }
                let mut inner = guard(&self.inner);
                if !matches!(inner.conn, Conn::Down) {
                    return;
                }
                match EngineLock::acquire(&self.paths, now_unix()) {
                    Ok(lk) => self.become_engine(&mut inner, lk),
                    Err(LockError::Held(h)) => {
                        drop(inner);
                        self.connect_unlocked(h.and_then(|h| h.port));
                    }
                    Err(e) => self.set_down(&mut inner, e.to_string()),
                }
            }
        }
    }

    /// Stop an engine this process runs (restoring build priorities) and
    /// release the lock. Idempotent.
    pub fn shutdown(&self) {
        let (conn, server, lk) = {
            let mut inner = guard(&self.inner);
            let conn = std::mem::replace(&mut inner.conn, Conn::Down);
            (conn, inner.server.take(), inner.lock.take())
        };
        if let Some(s) = server {
            s.stop();
        }
        if let Conn::Local(e) = conn {
            e.shutdown(Duration::from_secs(5));
        }
        drop(lk);
    }

    fn remote(
        &self,
        c: &Client,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        match c.request(method, path, body.as_ref(), api::timeout_for(method, path)) {
            Ok(v) => {
                self.note_ok();
                Ok(v)
            }
            Err(e) => {
                self.note_failure(&e);
                Err(e.message)
            }
        }
    }

    fn down_error(&self) -> String {
        let why = self.info().detail.unwrap_or_default();
        format!("Mulchie's engine is not reachable ({why}); retrying")
    }

    // ----------------------------------------------------------------- reads

    /// The engine's whole state as JSON.
    pub fn state(&self) -> Result<Value, String> {
        match self.conn() {
            Conn::Local(e) => to_value(&e.snapshot_state()),
            Conn::Remote(c) => self.remote(&c, "GET", "/v1/state", None),
            Conn::Down => Err(self.down_error()),
        }
    }

    /// History samples newer than `since` (unix seconds; 0 for all).
    pub fn history_since(&self, since: u64) -> Result<Vec<Value>, String> {
        let all: Vec<Value> = match self.conn() {
            Conn::Local(e) => {
                let h: Vec<Sample> = e.history().into_iter().filter(|s| s.t > since).collect();
                return h.iter().map(to_value).collect();
            }
            Conn::Remote(c) => from_value(self.remote(&c, "GET", "/v1/history", None)?)?,
            Conn::Down => return Err(self.down_error()),
        };
        Ok(all
            .into_iter()
            .filter(|s| s["t"].as_u64().unwrap_or(0) > since)
            .collect())
    }

    pub fn tray_info(&self) -> Result<TrayInfo, String> {
        match self.conn() {
            Conn::Local(e) => {
                let alert = e.config().alert_below_gb;
                let mut t = TrayInfo::from_health(&to_value(&e.health_summary())?);
                t.low_disk =
                    Some(t.disk_total > 0 && t.disk_free < alert.saturating_mul(1_000_000_000));
                Ok(t)
            }
            Conn::Remote(c) => Ok(TrayInfo::from_health(&self.remote(
                &c,
                "GET",
                "/v1/health",
                None,
            )?)),
            Conn::Down => Err(self.down_error()),
        }
    }

    // --------------------------------------------------------------- actions

    pub fn rescan(&self) -> Result<(), String> {
        match self.conn() {
            Conn::Local(e) => {
                e.request_scan();
                Ok(())
            }
            Conn::Remote(c) => self.remote(&c, "POST", "/v1/scan", None).map(|_| ()),
            Conn::Down => Err(self.down_error()),
        }
    }

    /// A plan, checked against the facts now. Its ready items become the only
    /// requests [`Backend::execute`] accepts for that kind of batch.
    pub fn plan(&self, req: &PlanRequest) -> Result<Plan, String> {
        let plan = match self.conn() {
            Conn::Local(e) => match req {
                PlanRequest::Clean { paths } => e.clean_plan(paths),
                PlanRequest::Stale { want_bytes } => e.reclaim_plan(*want_bytes),
                PlanRequest::Worktrees { paths, allow_dirty } => {
                    e.worktree_plan(paths, *allow_dirty)
                }
                PlanRequest::Sweep => e.sweep_plan(),
            },
            Conn::Remote(c) => {
                let v = match req {
                    PlanRequest::Clean { paths } => self.remote(
                        &c,
                        "POST",
                        "/v1/clean/plan",
                        Some(json!({ "paths": paths })),
                    )?,
                    PlanRequest::Stale { want_bytes } => self.remote(
                        &c,
                        "POST",
                        "/v1/clean/plan",
                        Some(json!({ "stale": true, "want_bytes": want_bytes })),
                    )?,
                    PlanRequest::Worktrees { paths, allow_dirty } => self.remote(
                        &c,
                        "POST",
                        "/v1/worktree/plan",
                        Some(json!({ "paths": paths, "allow_dirty": allow_dirty })),
                    )?,
                    PlanRequest::Sweep => self.remote(&c, "GET", "/v1/sweep", None)?,
                };
                from_value(v)?
            }
            Conn::Down => return Err(self.down_error()),
        };
        guard(&self.plans).record(req.batch(), &plan);
        Ok(plan)
    }

    /// Execute exactly `items`, which must be ready items of the last plan of
    /// this kind. Each is re-checked by the safety gate now. The items leave
    /// the plan book before they are sent, so a failure after that cannot be
    /// retried blindly; [`execute_error`] says what may have happened.
    pub fn execute(&self, batch: Batch, items: Vec<Request>) -> Result<Vec<ActionResult>, String> {
        let conn = self.conn();
        if matches!(conn, Conn::Down) {
            return Err(self.down_error());
        }
        guard(&self.plans).take(batch, &items)?;
        match conn {
            Conn::Local(e) => Ok(e.execute(&items, "manual")),
            Conn::Remote(c) => {
                let path = batch.route();
                let body = json!({ "items": items });
                match c.request("POST", path, Some(&body), api::timeout_for("POST", path)) {
                    Ok(v) => {
                        self.note_ok();
                        from_value(v)
                    }
                    Err(e) => {
                        self.note_failure(&e);
                        Err(execute_error(&e))
                    }
                }
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// A package manager's prune command: `{key, ok, freed, output, reason}`.
    pub fn prune(&self, key: &str) -> Result<Value, String> {
        match self.conn() {
            Conn::Local(e) => to_value(&e.prune_cache(key, "manual")),
            Conn::Remote(c) => self.remote(&c, "POST", "/v1/prune", Some(json!({ "key": key }))),
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Plan an action on one process (optionally its descendants) or on a
    /// consumer group, from facts read now. The plan's ready items become the
    /// only process requests [`Backend::execute`] accepts for that target.
    /// The app's own pid goes along, so a group holding the app is refused.
    pub fn process_plan(&self, mut req: ProcessPlanRequest) -> Result<Value, String> {
        req.caller_pid = Some(std::process::id());
        let plan = match self.conn() {
            Conn::Local(e) => to_value(&e.process_plan(&req))?,
            Conn::Remote(c) => {
                self.remote(&c, "POST", "/v1/process/plan", Some(to_value(&req)?))?
            }
            Conn::Down => return Err(self.down_error()),
        };
        let target = plan["target"].as_str().unwrap_or_default().to_string();
        guard(&self.plans).record_process(&target, ready_requests(&plan));
        Ok(plan)
    }

    /// Plan several requests (a bulk selection, or one action) for one
    /// review. Each is planned from facts read now; their ready items are
    /// kept as one entry, so a selection of any size is one stored plan.
    /// Returns `{target, plans}`.
    pub fn process_plans(&self, reqs: Vec<ProcessPlanRequest>) -> Result<Value, String> {
        if reqs.is_empty() || reqs.len() > MAX_BULK {
            return Err(format!("select between 1 and {MAX_BULK} processes"));
        }
        let mut plans = Vec::with_capacity(reqs.len());
        for mut req in reqs {
            req.caller_pid = Some(std::process::id());
            plans.push(match self.conn() {
                Conn::Local(e) => to_value(&e.process_plan(&req))?,
                Conn::Remote(c) => {
                    self.remote(&c, "POST", "/v1/process/plan", Some(to_value(&req)?))?
                }
                Conn::Down => return Err(self.down_error()),
            });
        }
        let mut targets: Vec<&str> = plans.iter().filter_map(|p| p["target"].as_str()).collect();
        targets.sort_unstable();
        targets.dedup();
        let target = targets.join(" + ");
        let ready = plans.iter().flat_map(ready_requests).collect();
        guard(&self.plans).record_process(&target, ready);
        Ok(json!({ "target": target, "plans": plans }))
    }

    /// Whether the plan book keeps a process plan for `target`.
    #[cfg(test)]
    pub fn keeps_process_plan(&self, target: &str) -> bool {
        guard(&self.plans).has_process(target)
    }

    /// Lower one process's priority to `nice`, only if it is still the
    /// process started at `start_time` (the gate checks; a mismatch comes
    /// back as a skipped result). Process plans have no renice for a single
    /// pid, so this goes to the gate directly, like the engine's own
    /// throttling.
    pub fn renice(&self, pid: u32, start_time: u64, nice: i32) -> Result<ActionResult, String> {
        let action = ProcAction::Renice {
            nice: nice.clamp(0, 20),
        };
        match self.conn() {
            Conn::Local(e) => Ok(e.process_action_expecting(pid, action, start_time, "manual")),
            Conn::Remote(c) => {
                let mut body = to_value(&action)?;
                body["pid"] = json!(pid);
                body["expected_start_time"] = json!(start_time);
                let v = self.remote(&c, "POST", "/v1/process", Some(body))?;
                from_value(v["result"].clone())
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Everything the engine knows about one process, read now
    /// (`ProcessDetail` as JSON). `Ok(None)` when it does not exist.
    pub fn process_detail(&self, pid: u32) -> Result<Option<Value>, String> {
        match self.conn() {
            Conn::Local(e) => e.process_detail(pid).as_ref().map(to_value).transpose(),
            Conn::Remote(c) => {
                let path = format!("/v1/process/{pid}");
                let r = c.request("GET", &path, None, api::timeout_for("GET", &path));
                match &r {
                    Err(e) if e.status != 404 => self.note_failure(e),
                    _ => self.note_ok(),
                }
                detail_reply(r)
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Hide finding `id` for `days`; returns when the dismissal ends.
    pub fn dismiss_finding(&self, id: &str, days: Option<u64>) -> Result<u64, String> {
        match self.conn() {
            Conn::Local(e) => e.dismiss_finding(id, days).map_err(|(_, m)| m),
            Conn::Remote(c) => {
                let v = self.remote(
                    &c,
                    "POST",
                    "/v1/findings/dismiss",
                    Some(json!({ "id": id, "days": days })),
                )?;
                v["until_unix"]
                    .as_u64()
                    .ok_or_else(|| "unexpected answer from the engine".into())
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Show finding `id` again; false when it was not dismissed.
    pub fn undismiss_finding(&self, id: &str) -> Result<bool, String> {
        match self.conn() {
            Conn::Local(e) => e.undismiss_finding(id).map_err(|(_, m)| m),
            Conn::Remote(c) => {
                let v = self.remote(
                    &c,
                    "POST",
                    "/v1/findings/undismiss",
                    Some(json!({ "id": id })),
                )?;
                Ok(v["was_dismissed"].as_bool().unwrap_or(false))
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Save and apply settings: in this engine, or through the running
    /// engine's `PUT /v1/config`, which saves its file the same way. The
    /// config stays JSON until this engine parses it, so a client passes on
    /// keys that a newer engine knows and this build does not.
    pub fn set_config(&self, cfg: Value) -> Result<(), String> {
        match self.conn() {
            Conn::Local(e) => {
                let cfg: Config = serde_json::from_value(cfg)
                    .map_err(|e| format!("the settings are not valid: {e}"))?;
                e.set_config(cfg).map_err(|e| e.to_string())
            }
            Conn::Remote(c) => self
                .remote(&c, "PUT", "/v1/config", Some(config_put_body(cfg)))
                .map(|_| ()),
            Conn::Down => Err(self.down_error()),
        }
    }

    /// The settings file of the engine in use. As a client, the running
    /// engine's (from `GET /v1/config`), which may be under another home.
    pub fn config_path(&self) -> PathBuf {
        if let Conn::Remote(c) = self.conn() {
            if let Some(p) = self
                .remote(&c, "GET", "/v1/config", None)
                .ok()
                .and_then(|v| v["path"].as_str().map(PathBuf::from))
            {
                return p;
            }
        }
        self.paths.config_file()
    }
}

/// The body of `PUT /v1/config`: the config exactly as given.
pub fn config_put_body(cfg: Value) -> Value {
    json!({ "config": cfg })
}

/// A remote execute failed after its items left the plan book. When the
/// engine could not be reached nothing was sent; otherwise (a timeout, a
/// broken connection, an error answer) the action may have run.
pub fn execute_error(e: &ApiError) -> String {
    if e.status == 0 && e.code == "unreachable" {
        format!(
            "Mulchie's engine could not be reached ({}), so nothing was sent. Review the plan again.",
            e.message
        )
    } else {
        format!(
            "The request failed after it was sent ({}). The action may have run: check Activity before trying again.",
            e.message
        )
    }
}

/// `GET /v1/process/{pid}` as `process_detail` returns it. Only the engine's
/// `unknown_pid` means the process is gone; another 404 is an engine without
/// the route.
pub fn detail_reply(r: Result<Value, ApiError>) -> Result<Option<Value>, String> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.status == 404 && e.code == "unknown_pid" => Ok(None),
        Err(e) if e.status == 404 => {
            Err("process details are not supported by the running engine; update Mulchie".into())
        }
        Err(e) => Err(e.message),
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}
