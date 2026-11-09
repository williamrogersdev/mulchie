use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::config_dir;
use crate::engine::{Engine, ProcAction, WorkspaceAction};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn token_path() -> PathBuf {
    config_dir().join("token")
}

pub fn load_or_create_token() -> std::io::Result<String> {
    let path = token_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            return Ok(t);
        }
    }
    std::fs::create_dir_all(config_dir())?;
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

pub fn serve(engine: Engine, port: u16) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let token = load_or_create_token()?;
    std::thread::Builder::new()
        .name("mulchie-api".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let engine = engine.clone();
                let token = token.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &engine, &token);
                });
            }
        })
}

struct Req {
    method: String,
    path: String,
    auth: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_req(stream: &TcpStream) -> std::io::Result<Req> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut len = 0usize;
    let mut auth = None;
    let mut origin = None;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let v = v.trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.parse().unwrap_or(0).min(1 << 20),
                "authorization" => auth = Some(v),
                "origin" => origin = Some(v),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Req {
        method,
        path,
        auth,
        origin,
        body,
    })
}

fn respond(mut s: &TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Not Found",
    };
    write!(
        s,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[derive(Deserialize)]
struct WorkspaceBody {
    path: PathBuf,
    action: WorkspaceAction,
}

#[derive(Deserialize)]
struct CleanBody {
    paths: Option<Vec<PathBuf>>,
    stale: Option<bool>,
}

#[derive(Deserialize)]
struct ProcessBody {
    pid: u32,
    #[serde(flatten)]
    action: ProcAction,
}

#[derive(Deserialize)]
struct WorktreeBody {
    paths: Vec<PathBuf>,
    #[serde(default)]
    backup: bool,
}

#[derive(Deserialize)]
struct PruneBody {
    key: String,
}

#[derive(Deserialize)]
struct LeaseBody {
    #[serde(default)]
    label: String,
    #[serde(default)]
    pid: u32,
    ticket: Option<u64>,
}

#[derive(Deserialize)]
struct ReleaseBody {
    ticket: u64,
}

const PROTECTED: &[&str] = &[
    "/v1/state",
    "/v1/history",
    "/v1/advice",
    "/v1/scan",
    "/v1/clean",
    "/v1/workspace",
    "/v1/process",
    "/v1/worktree/remove",
    "/v1/sweep",
    "/v1/prune",
    "/v1/lease",
    "/v1/release",
];

fn body<T: serde::de::DeserializeOwned>(req: &Req) -> Result<T, String> {
    serde_json::from_slice(&req.body).map_err(|e| format!("invalid body: {e}"))
}

fn ok<T: serde::Serialize>(v: &T) -> (u16, String) {
    (200, serde_json::to_string(v).unwrap_or_default())
}

fn err(code: u16, msg: impl std::fmt::Display) -> (u16, String) {
    (
        code,
        serde_json::json!({ "error": msg.to_string() }).to_string(),
    )
}

fn route(req: &Req, engine: &Engine) -> (u16, String) {
    let path = req.path.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("GET", "/v1/state") => ok(&engine.snapshot_state()),
        ("GET", "/v1/history") => ok(&engine.history()),
        ("GET", "/v1/advice") => ok(&engine.advice()),
        ("GET", "/v1/sweep") => ok(&engine.sweep_plan()),
        ("POST", "/v1/scan") => {
            engine.request_scan();
            ok(&serde_json::json!({ "ok": true }))
        }
        ("POST", "/v1/clean") => match body::<CleanBody>(req) {
            Ok(b) if b.stale.unwrap_or(false) => ok(&engine.reclaim_stale(0, "api")),
            Ok(b) => ok(&engine.clean_paths(&b.paths.unwrap_or_default(), "api")),
            Err(e) => err(400, e),
        },
        ("POST", "/v1/workspace") => match body::<WorkspaceBody>(req) {
            Ok(b) => match engine.workspace_action(&b.path, b.action, "api") {
                Ok(n) => ok(&serde_json::json!({ "ok": true, "processes": n })),
                Err(e) => err(400, e),
            },
            Err(e) => err(400, e),
        },
        ("POST", "/v1/process") => match body::<ProcessBody>(req) {
            Ok(b) => match engine.process_action(b.pid, b.action, "api") {
                Ok(()) => ok(&serde_json::json!({ "ok": true })),
                Err(e) => err(400, e),
            },
            Err(e) => err(400, e),
        },
        ("POST", "/v1/worktree/remove") => match body::<WorktreeBody>(req) {
            Ok(b) => ok(&engine.remove_worktrees(&b.paths, b.backup, "api")),
            Err(e) => err(400, e),
        },
        ("POST", "/v1/sweep") => ok(&engine.sweep_worktrees("api")),
        ("POST", "/v1/prune") => match body::<PruneBody>(req) {
            Ok(b) => ok(&engine.prune_cache(&b.key, "api")),
            Err(e) => err(400, e),
        },
        ("POST", "/v1/lease") => match body::<LeaseBody>(req) {
            Ok(b) => {
                let label = if b.label.is_empty() {
                    format!("pid {}", b.pid)
                } else {
                    b.label
                };
                ok(&engine.acquire(&label, b.pid, b.ticket))
            }
            Err(e) => err(400, e),
        },
        ("POST", "/v1/release") => match body::<ReleaseBody>(req) {
            Ok(b) => ok(&serde_json::json!({ "released": engine.release(b.ticket) })),
            Err(e) => err(400, e),
        },
        _ => err(404, "not found"),
    }
}

fn handle(stream: TcpStream, engine: &Engine, token: &str) -> std::io::Result<()> {
    let req = read_req(&stream)?;
    if req.origin.is_some() {
        return respond(
            &stream,
            403,
            r#"{"error":"browser requests are not allowed"}"#,
        );
    }
    let path = req.path.split('?').next().unwrap_or("");
    if req.method == "GET" && path == "/v1/health" {
        let st = engine.snapshot_state();
        let adv = engine.advice();
        return respond(
            &stream,
            200,
            &serde_json::json!({
                "name": "mulchie",
                "version": VERSION,
                "health": st.health,
                "disk_free": st.snapshot.disk_free,
                "disk_total": st.snapshot.disk_total,
                "cpu_pct": st.snapshot.cpu_pct,
                "load": st.snapshot.load,
                "mem_free_pct": st.snapshot.mem_free_pct(),
                "agents": st.snapshot.agent_count,
                "builds": st.snapshot.build_count,
                "free_slots": adv.free_slots,
                "ok_to_start_heavy_work": adv.ok_to_start_heavy_work,
                "reclaimable_stale": st.scan.as_ref().map(|s| s.stale_bytes).unwrap_or(0),
            })
            .to_string(),
        );
    }
    if PROTECTED.contains(&path) && req.auth.as_deref() != Some(&format!("Bearer {token}")) {
        return respond(
            &stream,
            401,
            r#"{"error":"missing or invalid bearer token"}"#,
        );
    }
    let (code, out) = route(&req, engine);
    respond(&stream, code, &out)
}

pub struct Client {
    port: u16,
    token: String,
}

impl Client {
    pub fn connect() -> Option<Self> {
        let port = crate::config::Config::load().api_port;
        let token = std::fs::read_to_string(token_path())
            .ok()?
            .trim()
            .to_string();
        let c = Self { port, token };
        c.call("GET", "/v1/health", None).ok().map(|_| c)
    }

    pub fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800))
            .map_err(|e| format!("mulchie is not running ({e})"))?;
        s.set_read_timeout(Some(Duration::from_secs(600))).ok();
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            self.token,
            payload.len()
        )
        .map_err(|e| e.to_string())?;
        let mut raw = String::new();
        s.read_to_string(&mut raw).map_err(|e| e.to_string())?;
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let code: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        if code == 200 {
            Ok(v)
        } else {
            Err(v
                .get("error")
                .and_then(|e| e.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("HTTP {code}")))
        }
    }
}
