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

use mulchie_core::api::{self, Client, Server};
use mulchie_core::engine::{ProcAction, Sample};
use mulchie_core::lock::{self, EngineLock, LockError};
use mulchie_core::process::ProcessSource;
use mulchie_core::{
    ActionResult, Config, Engine, LoadedConfig, Paths, Plan, Request, WorkspaceAction,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
}

impl Batch {
    pub fn allows(self, r: &Request) -> bool {
        match self {
            Batch::Clean => matches!(r, Request::DeleteCache { .. }),
            Batch::Worktree | Batch::Sweep => matches!(
                r,
                Request::RemoveWorktree { .. } | Request::PruneWorktree { .. }
            ),
        }
    }

    fn route(self) -> &'static str {
        match self {
            Batch::Clean => "/v1/clean",
            Batch::Worktree => "/v1/worktree/remove",
            Batch::Sweep => "/v1/sweep",
        }
    }
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

/// The ready requests of the last plan issued for each batch kind. An execute
/// must name only these, so the UI can approve a subset of a plan but never
/// add to it.
#[derive(Debug, Default)]
pub struct PlanBook {
    ready: HashMap<Batch, Vec<Request>>,
}

impl PlanBook {
    pub fn record(&mut self, batch: Batch, plan: &Plan) {
        self.ready.insert(batch, plan.approved());
    }

    /// Check `items` against the last plan for `batch`. On success the items
    /// leave the book, so the same approval cannot be sent twice.
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
        let ready = self.ready.get(&batch).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(r) = items.iter().find(|r| !ready.contains(r)) {
            return Err(format!(
                "{} is not a ready item of the current plan; review the plan again",
                r.target()
            ));
        }
        if let Some(list) = self.ready.get_mut(&batch) {
            list.retain(|r| !items.contains(r));
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
        inner.lock = Some(lk);
        inner.server = server;
        inner.down_since = None;
    }

    /// Connect to the engine holding the lock, which announced `port`.
    fn connect(&self, inner: &mut Inner, port: Option<u16>) {
        let holder = lock::running_engine(&self.paths);
        let port = port.or(holder.as_ref().and_then(|h| h.port));
        let Some(port) = port else {
            let msg = match &holder {
                Some(h) => format!("the engine (pid {}) has not opened its API yet", h.pid),
                None => "no Mulchie engine is running".into(),
            };
            return self.set_down(inner, msg);
        };
        match Client::connect_in(&self.paths, port) {
            Ok(c) => {
                inner.info = ConnInfo {
                    mode: Mode::Client,
                    port: Some(c.port()),
                    engine_pid: holder.map(|h| h.pid),
                    detail: None,
                    since_unix: now_unix(),
                };
                inner.conn = Conn::Remote(c);
                inner.down_since = None;
            }
            Err(e) => self.set_down(inner, e),
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
    }

    /// A remote call could not reach the engine.
    fn mark_down(&self, why: &str) {
        let mut inner = guard(&self.inner);
        if matches!(inner.conn, Conn::Remote(_)) {
            self.set_down(&mut inner, format!("lost the engine: {why}"));
        }
    }

    /// One supervision step (every few seconds): check a remote engine, and
    /// when disconnected, reconnect or take over.
    pub fn tick(&self) {
        match self.conn() {
            Conn::Local(_) => {}
            Conn::Remote(c) => {
                if let Err(e) = c.request("GET", "/v1/health", None, api::HEALTH_TIMEOUT) {
                    self.mark_down(&e.message);
                }
            }
            Conn::Down => {
                let mut inner = guard(&self.inner);
                if !matches!(inner.conn, Conn::Down) {
                    return;
                }
                if let Some(h) = lock::running_engine(&self.paths) {
                    return self.connect(&mut inner, h.port);
                }
                let waited = inner
                    .down_since
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::MAX);
                if waited < self.opts.takeover_after {
                    return;
                }
                match EngineLock::acquire(&self.paths, now_unix()) {
                    Ok(lk) => self.become_engine(&mut inner, lk),
                    Err(LockError::Held(h)) => self.connect(&mut inner, h.and_then(|h| h.port)),
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
        c.request(method, path, body.as_ref(), api::timeout_for(method, path))
            .map_err(|e| {
                if e.code == "unreachable" {
                    self.mark_down(&e.message);
                }
                e.message
            })
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
    /// this kind. Each is re-checked by the safety gate now.
    pub fn execute(&self, batch: Batch, items: Vec<Request>) -> Result<Vec<ActionResult>, String> {
        guard(&self.plans).take(batch, &items)?;
        match self.conn() {
            Conn::Local(e) => Ok(e.execute(&items, "manual")),
            Conn::Remote(c) => from_value(self.remote(
                &c,
                "POST",
                batch.route(),
                Some(json!({ "items": items })),
            )?),
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

    pub fn workspace_action(&self, path: &Path, action: WorkspaceAction) -> Result<usize, String> {
        match self.conn() {
            Conn::Local(e) => e.workspace_action(path, action, "manual"),
            Conn::Remote(c) => {
                let v = self.remote(
                    &c,
                    "POST",
                    "/v1/workspace",
                    Some(json!({ "path": path, "action": action })),
                )?;
                Ok(v["processes"].as_u64().unwrap_or(0) as usize)
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    pub fn process_action(&self, pid: u32, action: ProcAction) -> Result<(), String> {
        match self.conn() {
            Conn::Local(e) => e.process_action(pid, action, "manual"),
            Conn::Remote(c) => {
                let mut body = to_value(&action)?;
                body["pid"] = json!(pid);
                self.remote(&c, "POST", "/v1/process", Some(body))
                    .map(|_| ())
            }
            Conn::Down => Err(self.down_error()),
        }
    }

    /// Save and apply settings. Only the engine can do this: the API has no
    /// route for it, so a client says where the settings live instead.
    pub fn set_config(&self, cfg: Config) -> Result<(), String> {
        match self.conn() {
            Conn::Local(e) => e.set_config(cfg).map_err(|e| e.to_string()),
            Conn::Remote(_) => Err(format!(
                "Settings belong to the engine that is running (pid {}). Edit {} and restart that engine, or stop it so this app runs the engine.",
                self.info().engine_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                self.paths.config_file().display()
            )),
            Conn::Down => Err(self.down_error()),
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}
