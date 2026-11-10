//! The local HTTP API on `127.0.0.1:<port>`.
//!
//! Every route except `GET /v1/health` needs `Authorization: Bearer <token>`,
//! the token in `<config dir>/token` (0600, in a 0700 directory). A second,
//! read-only token in `<config dir>/token.read` (same rules) is accepted on
//! the `GET` routes and the plan routes ([`read_only_allowed`]); anything else
//! sent with it is 403 `read_only_token`. Requests must
//! name `Host: 127.0.0.1:<port>` or `localhost:<port>` and carry no `Origin`, so
//! a web page (even through DNS rebinding) cannot drive it. Request lines are
//! limited to 8 KB, headers to 64 KB, bodies to 1 MB, a whole request to 10 s,
//! and the server to 64 connections at once.
//!
//! Errors are JSON: `{"error": "<message>", "code": "<stable code>"}`.
//!
//! Destructive routes execute exactly the requests they are given: a caller
//! fetches a plan (`GET /v1/sweep`, `/v1/clean/plan`, `POST /v1/worktree/plan`),
//! shows it, and posts back the approved items (`{"items": [...]}`).

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{create_private_dir, Config};
use crate::engine::{Engine, LeaseRequest, ProcAction, WorkspaceAction};
use crate::lock;
use crate::paths::{random_hex, Paths};
use crate::safety::Request;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Request size and time limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Longest request line or header line.
    pub line: usize,
    /// All header lines together.
    pub headers: usize,
    pub body: usize,
    /// From accepting the connection to having read the whole request.
    pub deadline: Duration,
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            line: 8 * 1024,
            headers: 64 * 1024,
            body: 1024 * 1024,
            deadline: Duration::from_secs(10),
            max_connections: 64,
        }
    }
}

/// Every route, for dispatch and for the auth tests. `/v1/health` is the only
/// one that needs no token.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/health"),
    ("GET", "/v1/state"),
    ("GET", "/v1/history"),
    ("GET", "/v1/advice"),
    ("POST", "/v1/scan"),
    ("GET", "/v1/clean/plan"),
    ("POST", "/v1/clean/plan"),
    ("POST", "/v1/clean"),
    ("GET", "/v1/sweep"),
    ("POST", "/v1/sweep"),
    ("POST", "/v1/worktree/plan"),
    ("POST", "/v1/worktree/remove"),
    ("POST", "/v1/prune"),
    ("POST", "/v1/workspace"),
    ("POST", "/v1/process"),
    ("POST", "/v1/process/plan"),
    ("POST", "/v1/process/execute"),
    ("POST", "/v1/lease"),
    ("POST", "/v1/release"),
    ("GET", "/v1/findings"),
    ("POST", "/v1/findings/dismiss"),
    ("POST", "/v1/findings/undismiss"),
    // `{pid}` is a whole number; see `route`.
    ("GET", "/v1/process/{pid}"),
    ("GET", "/v1/config"),
    ("PUT", "/v1/config"),
];

// ------------------------------------------------------------------ token

pub fn token_path_in(paths: &Paths) -> PathBuf {
    paths.config_dir.join("token")
}

/// The read-only token: reads and plans, never changes.
pub fn read_only_token_path_in(paths: &Paths) -> PathBuf {
    paths.config_dir.join("token.read")
}

/// Routes the read-only token may call: every `GET`, and the `POST`s that
/// only plan.
pub const READ_ONLY_POSTS: &[&str] = &["/v1/clean/plan", "/v1/worktree/plan", "/v1/process/plan"];

/// Build-slot routes the read-only token may also call, within limits: it may
/// take a lease only with a `ttl_secs` and no `pid`, and release only with the
/// lease's release secret. A read-only agent that cannot queue its builds
/// would make the machine worse, not safer.
pub const READ_ONLY_LEASE_POSTS: &[&str] = &["/v1/lease", "/v1/release"];

/// Whether the read-only token may call `method path`.
pub fn read_only_allowed(method: &str, path: &str) -> bool {
    method == "GET"
        || (method == "POST"
            && (READ_ONLY_POSTS.contains(&path) || READ_ONLY_LEASE_POSTS.contains(&path)))
}

/// The token file for this user's environment.
pub fn token_path() -> PathBuf {
    token_path_in(&Paths::from_env())
}

pub fn load_or_create_token() -> std::io::Result<String> {
    load_or_create_token_in(&Paths::from_env())
}

fn valid_token(t: &str) -> bool {
    t.len() >= 32 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn perm_error(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg)
}

/// A token file must be a regular file owned by us with no group or other
/// permissions; its directory must be ours and not writable by others.
fn check_token_file(path: &Path) -> std::io::Result<()> {
    let uid = unsafe { libc::getuid() };
    if let Some(dir) = path.parent() {
        let d = std::fs::symlink_metadata(dir)?;
        if !d.is_dir() || d.uid() != uid || d.mode() & 0o022 != 0 {
            return Err(perm_error(format!(
                "{} must be a directory owned by you and writable only by you (mode 700)",
                dir.display()
            )));
        }
    }
    let m = std::fs::symlink_metadata(path)?;
    if !m.file_type().is_file() {
        return Err(perm_error(format!(
            "{} is not a regular file; delete it and Mulchie will create a new token",
            path.display()
        )));
    }
    if m.uid() != uid {
        return Err(perm_error(format!(
            "{} belongs to uid {}, not you (uid {uid}); delete it and Mulchie will create a new token",
            path.display(),
            m.uid()
        )));
    }
    if m.mode() & 0o077 != 0 {
        return Err(perm_error(format!(
            "{} has mode {:o}, so others can read the API token; run `chmod 600 {}` or delete it and Mulchie will create a new token",
            path.display(),
            m.mode() & 0o777,
            path.display()
        )));
    }
    Ok(())
}

fn read_token_file(path: &Path) -> std::io::Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut s = String::new();
    (&mut f).take(4096).read_to_string(&mut s)?;
    Ok(s.trim().to_string())
}

/// The existing token, checked for ownership and permissions.
pub fn read_token(paths: &Paths) -> std::io::Result<String> {
    read_token_at(&token_path_in(paths))
}

/// The existing read-only token, checked like [`read_token`].
pub fn read_read_only_token(paths: &Paths) -> std::io::Result<String> {
    read_token_at(&read_only_token_path_in(paths))
}

fn read_token_at(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    check_token_file(&path)?;
    let t = read_token_file(&path)?;
    if !valid_token(&t) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} does not hold a valid token", path.display()),
        ));
    }
    Ok(t)
}

/// The token, created (32 random bytes, hex) with `create_new` and mode 0600
/// when there is none. An existing file with looser permissions or another
/// owner is refused, never silently used or replaced.
pub fn load_or_create_token_in(paths: &Paths) -> std::io::Result<String> {
    load_or_create_token_at(paths, &token_path_in(paths))
}

/// The read-only token, created like [`load_or_create_token_in`]. It is a
/// different random value from the full token.
pub fn load_or_create_read_only_token_in(paths: &Paths) -> std::io::Result<String> {
    load_or_create_token_at(paths, &read_only_token_path_in(paths))
}

fn load_or_create_token_at(paths: &Paths, path: &Path) -> std::io::Result<String> {
    create_private_dir(&paths.config_dir)?;
    let path = path.to_path_buf();
    for _ in 0..3 {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                check_token_file(&path)?;
                let t = read_token_file(&path)?;
                if valid_token(&t) {
                    return Ok(t);
                }
                // Ours, private, but empty or truncated (a crash mid-write).
                std::fs::remove_file(&path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)
                {
                    Ok(mut f) => {
                        let t = random_hex(32)?;
                        f.write_all(t.as_bytes())?;
                        f.sync_all()?;
                        return Ok(t);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "{} keeps changing while it is read",
        path.display()
    )))
}

/// Compare without an early exit, so timing says nothing about the token.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = 0u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0 && a.len() == b.len()
}

// ----------------------------------------------------------------- server

/// The running API. [`Server::stop`] closes the listener and waits for it.
pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting connections and wait for the accept loop to end.
    /// Requests already being handled finish on their own threads.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_millis(500),
        );
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Block until the server stops.
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn serve(engine: Engine, port: u16) -> std::io::Result<Server> {
    serve_with(engine, port, Limits::default())
}

/// Bind `127.0.0.1:<port>` (0 picks a free port; see [`Server::port`]) and
/// serve on a background thread, one thread per connection.
pub fn serve_with(engine: Engine, port: u16, limits: Limits) -> std::io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    engine.set_api_port(port);
    let token: Arc<str> = load_or_create_token_in(engine.paths())?.into();
    let read_token: Arc<str> = load_or_create_read_only_token_in(engine.paths())?.into();
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let stop2 = stop.clone();
    let handle = std::thread::Builder::new()
        .name("mulchie-api".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                if active.fetch_add(1, Ordering::SeqCst) >= limits.max_connections {
                    active.fetch_sub(1, Ordering::SeqCst);
                    reject_busy(&stream);
                    continue;
                }
                let engine = engine.clone();
                let token = token.clone();
                let read_token = read_token.clone();
                // Released when the connection thread ends, or when it could
                // not be started (the closure is dropped unrun).
                let slot = Slot(active.clone());
                let _ = std::thread::Builder::new()
                    .name("mulchie-api-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle_conn(stream, &engine, &token, &read_token, port, &limits);
                    });
            }
        })?;
    Ok(Server {
        port,
        stop,
        handle: Some(handle),
    })
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Too many connections: answer 503 on the accept thread, spending at most
/// [`REJECT_BUDGET`] on this client in all (write and drain together). The
/// request bytes are drained before closing so the answer is not lost to a
/// reset; a client trickling bytes cannot hold the accept thread past the
/// budget.
fn reject_busy(stream: &TcpStream) {
    let deadline = Instant::now() + REJECT_BUDGET;
    let _ = stream.set_write_timeout(Some(REJECT_BUDGET / 2));
    let resp = ApiError::new(
        503,
        "too_many_connections",
        "too many connections; try again shortly",
    )
    .into_response();
    let _ = write_response(stream, &resp);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut sink = [0u8; 4096];
    let mut left: usize = 64 * 1024;
    while left > 0 {
        let rest = deadline.saturating_duration_since(Instant::now());
        if rest.is_zero() || stream.set_read_timeout(Some(rest)).is_err() {
            break;
        }
        match (&*stream).read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

/// The most time the accept thread spends turning away one client.
const REJECT_BUDGET: Duration = Duration::from_millis(100);

/// A reader that never blocks past the request deadline.
struct DeadlineStream<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        match (&*self.stream).read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(std::io::ErrorKind::TimedOut.into())
            }
            r => r,
        }
    }
}

fn handle_conn(
    stream: TcpStream,
    engine: &Engine,
    token: &str,
    read_token: &str,
    port: u16,
    limits: &Limits,
) {
    let _ = stream.set_write_timeout(Some(limits.deadline));
    let deadline = Instant::now() + limits.deadline;
    let mut reader = BufReader::new(DeadlineStream {
        stream: &stream,
        deadline,
    });
    let resp = match read_req(&mut reader, limits, deadline) {
        Ok(req) => {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch(&req, engine, token, read_token, port)
            }));
            r.unwrap_or_else(|_| {
                ApiError::new(500, "internal_error", "the request failed inside Mulchie")
                    .into_response()
            })
        }
        Err(ReqError::Closed) => return,
        Err(ReqError::Http(e)) => e.into_response(),
    };
    let _ = write_response(&stream, &resp);
}

// ----------------------------------------------------------------- parsing

/// A parsed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub host: Option<String>,
    pub auth: Option<String>,
    pub origin: Option<String>,
    pub fetch_site: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum ReqError {
    /// The client closed (or never sent) the request; nothing to answer.
    Closed,
    Http(ApiError),
}

fn http_err(status: u16, code: &str, msg: impl Into<String>) -> ReqError {
    ReqError::Http(ApiError::new(status, code, msg))
}

fn timed_out() -> ReqError {
    http_err(
        408,
        "request_timeout",
        "the request was not received in time",
    )
}

fn io_err(e: std::io::Error) -> ReqError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => timed_out(),
        _ => ReqError::Closed,
    }
}

/// One line without its CRLF, at most `max` bytes. `Ok(None)` when the stream
/// ends before any byte.
fn read_line<R: BufRead>(
    r: &mut R,
    max: usize,
    deadline: Instant,
    too_long: &dyn Fn() -> ReqError,
) -> Result<Option<Vec<u8>>, ReqError> {
    let mut out = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(e)),
        };
        if buf.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Err(http_err(400, "bad_request", "connection closed mid-line"))
            };
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        out.extend_from_slice(&buf[..take]);
        r.consume(take);
        // Allow for the CRLF on top of `max`.
        if out.len() > max + 2 {
            return Err(too_long());
        }
        if done {
            out.pop();
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            if out.len() > max {
                return Err(too_long());
            }
            return Ok(Some(out));
        }
    }
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Read one HTTP/1.x request within the limits and the deadline.
pub fn read_req<R: BufRead>(
    r: &mut R,
    limits: &Limits,
    deadline: Instant,
) -> Result<Req, ReqError> {
    let line_too_long = || {
        http_err(
            414,
            "request_line_too_long",
            format!("the request line exceeds {} bytes", limits.line),
        )
    };
    let Some(line) = read_line(r, limits.line, deadline, &line_too_long)? else {
        return Err(ReqError::Closed);
    };
    let line = String::from_utf8(line)
        .map_err(|_| http_err(400, "bad_request", "the request line is not UTF-8"))?;
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(http_err(400, "bad_request", "malformed request line"));
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(http_err(400, "bad_request", "malformed method"));
    }
    if !target.starts_with('/') {
        return Err(http_err(400, "bad_request", "the target must be a path"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(http_err(400, "bad_request", "only HTTP/1.0 and HTTP/1.1"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let headers_too_large = || {
        http_err(
            431,
            "headers_too_large",
            format!(
                "header lines are limited to {} bytes each and {} in all",
                limits.line, limits.headers
            ),
        )
    };
    let mut total = 0usize;
    let mut req = Req {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        host: None,
        auth: None,
        origin: None,
        fetch_site: None,
        body: Vec::new(),
    };
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let Some(h) = read_line(r, limits.line, deadline, &headers_too_large)? else {
            return Err(http_err(
                400,
                "bad_request",
                "headers ended without a blank line",
            ));
        };
        if h.is_empty() {
            break;
        }
        total += h.len() + 2;
        if total > limits.headers {
            return Err(headers_too_large());
        }
        let h = String::from_utf8(h)
            .map_err(|_| http_err(400, "bad_request", "a header is not UTF-8"))?;
        let Some((name, value)) = h.split_once(':') else {
            return Err(http_err(400, "bad_request", "a header line has no colon"));
        };
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(http_err(400, "bad_request", "malformed header name"));
        }
        let value = value.trim();
        let dup = || http_err(400, "bad_request", format!("duplicate {name} header"));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(http_err(400, "bad_request", "malformed Content-Length"));
                }
                let n: usize = value.parse().unwrap_or(usize::MAX);
                if content_length.is_some_and(|c| c != n) {
                    return Err(dup());
                }
                content_length = Some(n);
            }
            "transfer-encoding" => chunked = true,
            "host" if req.host.is_some() => return Err(dup()),
            "host" => req.host = Some(value.into()),
            "authorization" if req.auth.is_some() => return Err(dup()),
            "authorization" => req.auth = Some(value.into()),
            "origin" => req.origin = Some(value.into()),
            "sec-fetch-site" => req.fetch_site = Some(value.into()),
            _ => {}
        }
    }
    if chunked {
        return Err(http_err(
            501,
            "unsupported_transfer_encoding",
            "Transfer-Encoding is not supported; send Content-Length",
        ));
    }
    let len = content_length.unwrap_or(0);
    if len > limits.body {
        return Err(http_err(
            413,
            "body_too_large",
            format!("the body is limited to {} bytes", limits.body),
        ));
    }
    let mut body = vec![0u8; len];
    let mut got = 0;
    while got < len {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        match r.read(&mut body[got..]) {
            Ok(0) => {
                return Err(http_err(
                    400,
                    "bad_request",
                    "the body is shorter than Content-Length",
                ))
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    req.body = body;
    Ok(req)
}

// ---------------------------------------------------------------- responses

/// An error answer: HTTP status, stable machine code, human message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: u16,
    pub code: String,
    #[serde(rename = "error")]
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    fn into_response(self) -> Resp {
        Resp {
            status: self.status,
            body: serde_json::json!({ "error": self.message, "code": self.code }).to_string(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

struct Resp {
    status: u16,
    body: String,
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_response(mut s: &TcpStream, r: &Resp) -> std::io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        r.status,
        reason_phrase(r.status),
        r.body.len(),
        r.body
    )?;
    s.flush()
}

fn ok<T: Serialize>(v: &T) -> Resp {
    match serde_json::to_string(v) {
        Ok(body) => Resp { status: 200, body },
        Err(e) => ApiError::new(500, "internal_error", e.to_string()).into_response(),
    }
}

fn fail(status: u16, code: &str, msg: impl Into<String>) -> Resp {
    ApiError::new(status, code, msg).into_response()
}

// ----------------------------------------------------------------- routing

fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else { return false };
    let h = h.to_ascii_lowercase();
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
}

fn bearer_ok(auth: Option<&str>, token: &str) -> bool {
    let Some(a) = auth else { return false };
    let Some(t) = a.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(t.trim().as_bytes(), token.as_bytes())
}

fn dispatch(req: &Req, engine: &Engine, token: &str, read_token: &str, port: u16) -> Resp {
    if !host_ok(req.host.as_deref(), port) {
        return fail(
            403,
            "bad_host",
            format!("the Host header must be 127.0.0.1:{port} or localhost:{port}"),
        );
    }
    if req.origin.is_some() || req.fetch_site.as_deref().is_some_and(|s| s != "none") {
        return fail(403, "origin_refused", "browser requests are not allowed");
    }
    if req.method == "GET" && req.path == "/v1/health" {
        return ok(&engine.health_summary());
    }
    let mut read_only = false;
    if !bearer_ok(req.auth.as_deref(), token) {
        if !bearer_ok(req.auth.as_deref(), read_token) {
            return fail(401, "unauthorized", "missing or invalid bearer token");
        }
        read_only = true;
        if !read_only_allowed(&req.method, &req.path) {
            return fail(
                403,
                "read_only_token",
                format!(
                    "the read-only token can read state and make plans, not {} {}",
                    req.method, req.path
                ),
            );
        }
    }
    route(req, engine, read_only)
}

fn body<T: serde::de::DeserializeOwned>(req: &Req) -> Result<T, Resp> {
    let raw: &[u8] = if req.body.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        &req.body
    };
    serde_json::from_slice(raw).map_err(|e| fail(400, "invalid_body", format!("invalid body: {e}")))
}

fn query_u64(req: &Req, key: &str) -> Result<Option<u64>, Resp> {
    for pair in req.query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return v.parse().map(Some).map_err(|_| {
                fail(
                    400,
                    "invalid_query",
                    format!("{key} must be a whole number"),
                )
            });
        }
    }
    Ok(None)
}

#[derive(Deserialize, Default)]
struct ItemsBody {
    items: Option<Vec<Request>>,
    paths: Option<Vec<PathBuf>>,
    #[serde(default)]
    stale: bool,
    #[serde(default)]
    auto: bool,
    want_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct WorktreePlanBody {
    paths: Vec<PathBuf>,
    #[serde(default, alias = "backup")]
    allow_dirty: bool,
}

#[derive(Deserialize)]
struct WorkspaceBody {
    path: PathBuf,
    action: WorkspaceAction,
    /// The previewed processes; when given, only these are acted on.
    #[serde(default)]
    pids: Option<Vec<crate::engine::PidRef>>,
}

#[derive(Deserialize)]
struct ProcessBody {
    pid: u32,
    /// The start time the caller saw; a mismatch is skipped (200), not an error.
    #[serde(default)]
    expected_start_time: Option<u64>,
    #[serde(flatten)]
    action: ProcAction,
}

#[derive(Deserialize)]
struct DismissBody {
    id: String,
    /// Default 30, 1 to 3650.
    #[serde(default)]
    days: Option<u64>,
}

#[derive(Deserialize)]
struct ConfigBody {
    /// The whole config, as `GET /v1/config` returns it. Fields left out take
    /// their defaults.
    config: Option<crate::config::Config>,
}

/// The raw value of `key` in the query string. Not percent-decoded: the
/// values read this way are snake_case codes.
fn query_str<'a>(req: &'a Req, key: &str) -> Option<&'a str> {
    req.query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn query_enum<T: serde::de::DeserializeOwned>(req: &Req, key: &str) -> Result<Option<T>, Resp> {
    let Some(v) = query_str(req, key).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    serde_json::from_value(serde_json::Value::String(v.to_string()))
        .map(Some)
        .map_err(|_| fail(400, "invalid_query", format!("unknown {key} {v:?}")))
}

/// `GET /v1/process/<pid>`.
fn process_detail(engine: &Engine, rest: &str) -> Resp {
    let Ok(pid) = rest.parse::<u32>() else {
        return fail(
            400,
            "invalid_pid",
            "expected /v1/process/<pid> with a whole-number pid",
        );
    };
    match engine.process_detail(pid) {
        Some(d) => ok(&d),
        None => fail(404, "unknown_pid", format!("no process {pid}")),
    }
}

#[derive(Deserialize)]
struct PruneBody {
    key: String,
}

#[derive(Deserialize)]
struct ReleaseBody {
    ticket: u64,
    /// The `release_secret` from the reply that created the ticket.
    #[serde(default)]
    secret: Option<String>,
}

fn is_cache(r: &Request) -> bool {
    matches!(r, Request::DeleteCache { .. })
}

fn is_process(r: &Request) -> bool {
    matches!(
        r,
        Request::Signal { .. } | Request::Renice { .. } | Request::ReopenApp { .. }
    )
}

fn is_worktree(r: &Request) -> bool {
    matches!(
        r,
        Request::RemoveWorktree { .. } | Request::PruneWorktree { .. }
    )
}

/// Execute exactly `items` when every one is the right kind.
fn run_items(
    engine: &Engine,
    items: &[Request],
    allowed: fn(&Request) -> bool,
    what: &str,
) -> Resp {
    if let Some(i) = items.iter().position(|r| !allowed(r)) {
        return fail(
            400,
            "invalid_item",
            format!("item {i} is not a {what} request"),
        );
    }
    ok(&engine.execute(items, "api"))
}

fn route(req: &Req, engine: &Engine, read_only: bool) -> Resp {
    if req.method == "GET" {
        if let Some(rest) = req.path.strip_prefix("/v1/process/") {
            if !rest.contains('/') && rest != "plan" && rest != "execute" {
                return process_detail(engine, rest);
            }
        }
    }
    let r = (|| -> Result<Resp, Resp> {
        Ok(match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/state") => ok(&engine.snapshot_state()),
            ("GET", "/v1/history") => ok(&engine.history()),
            ("GET", "/v1/advice") => ok(&engine.advice()),
            ("POST", "/v1/scan") => {
                ok(&serde_json::json!({ "ok": true, "queued": engine.request_scan() }))
            }

            ("GET", "/v1/clean/plan") => ok(&engine.reclaim_plan(query_u64(req, "want_bytes")?)),
            ("POST", "/v1/clean/plan") => {
                let b: ItemsBody = body(req)?;
                match (b.paths, b.stale) {
                    (Some(p), _) => ok(&engine.clean_plan(&p)),
                    (None, true) => ok(&engine.reclaim_plan(b.want_bytes)),
                    (None, false) => fail(
                        400,
                        "invalid_body",
                        "expected {\"paths\": [...]} or {\"stale\": true, \"want_bytes\": N}",
                    ),
                }
            }
            ("POST", "/v1/clean") => {
                let b: ItemsBody = body(req)?;
                if let Some(items) = &b.items {
                    run_items(engine, items, is_cache, "delete_cache")
                } else if b.auto {
                    ok(&engine.reclaim_now(b.want_bytes, "api"))
                } else if let Some(p) = b.paths.filter(|p| !p.is_empty()) {
                    // An explicit list of paths is its own approval.
                    ok(&engine.clean_paths(&p, "api"))
                } else if b.stale {
                    fail(
                        400,
                        "plan_required",
                        "get a plan first (GET /v1/clean/plan), then POST /v1/clean {\"items\": [...]} with the approved items, or send {\"auto\": true}",
                    )
                } else {
                    fail(
                        400,
                        "items_required",
                        "expected {\"items\": [...]} from GET /v1/clean/plan",
                    )
                }
            }

            ("GET", "/v1/sweep") => ok(&engine.sweep_plan()),
            ("POST", "/v1/sweep") => {
                let b: ItemsBody = body(req)?;
                if let Some(items) = &b.items {
                    run_items(engine, items, is_worktree, "worktree")
                } else if b.auto {
                    ok(&engine.sweep_now("api"))
                } else {
                    fail(
                        400,
                        "items_required",
                        "get the plan with GET /v1/sweep, then POST /v1/sweep {\"items\": [...]} with the approved items, or send {\"auto\": true}",
                    )
                }
            }

            ("POST", "/v1/worktree/plan") => {
                let b: WorktreePlanBody = body(req)?;
                ok(&engine.worktree_plan(&b.paths, b.allow_dirty))
            }
            ("POST", "/v1/worktree/remove") => {
                let b: ItemsBody = body(req)?;
                match (&b.items, &b.paths) {
                    (Some(items), _) => run_items(engine, items, is_worktree, "worktree"),
                    (None, Some(_)) => fail(
                        400,
                        "expected_head_required",
                        "removals must name the HEAD the caller saw: POST /v1/worktree/plan {\"paths\": [...]}, then POST /v1/worktree/remove {\"items\": [...]}",
                    ),
                    (None, None) => fail(400, "items_required", "expected {\"items\": [...]} from POST /v1/worktree/plan"),
                }
            }

            ("POST", "/v1/prune") => {
                let b: PruneBody = body(req)?;
                ok(&engine.prune_cache(&b.key, "api"))
            }
            ("POST", "/v1/workspace") => {
                let b: WorkspaceBody = body(req)?;
                if let Some(pids) = &b.pids {
                    return Ok(
                        match engine.workspace_action_on(&b.path, b.action, pids, "api") {
                            Ok(results) => ok(&serde_json::json!({
                                "ok": true,
                                "processes": results.iter().filter(|r| r.is_done()).count(),
                                "results": results,
                            })),
                            Err(e) => fail(400, "action_refused", e),
                        },
                    );
                }
                match engine.workspace_action(&b.path, b.action, "api") {
                    Ok(n) => ok(&serde_json::json!({ "ok": true, "processes": n })),
                    Err(e) => fail(400, "action_refused", e),
                }
            }
            ("POST", "/v1/process") => {
                let b: ProcessBody = body(req)?;
                if let Some(st) = b.expected_start_time {
                    let r = engine.process_action_expecting(b.pid, b.action, st, "api");
                    return Ok(ok(&serde_json::json!({ "ok": r.is_done(), "result": r })));
                }
                match engine.process_action(b.pid, b.action, "api") {
                    Ok(()) => ok(&serde_json::json!({ "ok": true })),
                    Err(e) => fail(400, "action_refused", e),
                }
            }
            ("POST", "/v1/process/plan") => {
                let b: crate::procplan::ProcessPlanRequest = body(req)?;
                ok(&engine.process_plan(&b))
            }
            ("POST", "/v1/process/execute") => {
                let b: ItemsBody = body(req)?;
                match &b.items {
                    Some(items) => run_items(engine, items, is_process, "signal, renice or reopen_app"),
                    None => fail(
                        400,
                        "items_required",
                        "expected {\"items\": [...]} with the requests of a plan from POST /v1/process/plan",
                    ),
                }
            }
            ("GET", "/v1/findings") => {
                let sev = query_enum(req, "severity")?;
                let kind = query_enum(req, "kind")?;
                let all = matches!(query_str(req, "dismissed"), Some("1" | "true"));
                let (counts, at) = engine.finding_summary();
                ok(&serde_json::json!({
                    "findings": engine.findings(sev, kind, all),
                    "counts": counts,
                    "updated_unix": at,
                    "enabled": engine.config().watch.enabled,
                }))
            }
            ("POST", "/v1/findings/dismiss") => {
                let b: DismissBody = body(req)?;
                match engine.dismiss_finding(&b.id, b.days) {
                    Ok(until) => {
                        ok(&serde_json::json!({ "ok": true, "id": b.id, "until_unix": until }))
                    }
                    Err((code, m)) => fail(400, code, m),
                }
            }
            ("POST", "/v1/findings/undismiss") => {
                let b: DismissBody = body(req)?;
                match engine.undismiss_finding(&b.id) {
                    Ok(was) => {
                        ok(&serde_json::json!({ "ok": true, "id": b.id, "was_dismissed": was }))
                    }
                    Err((code, m)) => fail(400, code, m),
                }
            }
            ("GET", "/v1/config") => ok(&serde_json::json!({
                "config": engine.config(),
                "config_error": engine.config_error(),
                "path": engine.paths().config_file(),
            })),
            ("PUT", "/v1/config") => {
                let b: ConfigBody = body(req)?;
                let Some(cfg) = b.config else {
                    return Ok(fail(
                        400,
                        "config_required",
                        "expected {\"config\": {...}}: the whole config from GET /v1/config with your changes",
                    ));
                };
                match engine.set_config(cfg) {
                    Ok(()) => ok(&serde_json::json!({ "ok": true, "config": engine.config() })),
                    Err(e) => fail(400, "invalid_config", e.to_string()),
                }
            }
            ("POST", "/v1/lease") => {
                let b: LeaseRequest = body(req)?;
                if read_only && b.pid.is_some() {
                    return Ok(fail(
                        403,
                        "read_only_token",
                        "the read-only token may take a build slot only with ttl_secs and no pid",
                    ));
                }
                match engine.acquire(&b) {
                    Ok(r) => ok(&r),
                    Err(
                        e @ (crate::engine::LeaseError::SecretRequired
                        | crate::engine::LeaseError::WrongSecret),
                    ) => fail(403, e.code(), e.to_string()),
                    Err(e) => fail(400, e.code(), e.to_string()),
                }
            }
            ("POST", "/v1/release") => {
                let b: ReleaseBody = body(req)?;
                match engine.release_with(b.ticket, b.secret.as_deref()) {
                    Ok(released) => ok(&serde_json::json!({ "released": released })),
                    Err(e) => fail(
                        403,
                        e.code(),
                        "this lease has a release secret; send the release_secret from the reply that created the ticket",
                    ),
                }
            }
            (m, p) => {
                if ROUTES.iter().any(|(_, rp)| *rp == p) {
                    fail(
                        405,
                        "method_not_allowed",
                        format!("{m} is not allowed on {p}"),
                    )
                } else {
                    fail(404, "not_found", format!("no route {p}"))
                }
            }
        })
    })();
    r.unwrap_or_else(|e| e)
}

// ----------------------------------------------------------------- client

pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Plans and executions run git per worktree and can take minutes.
pub const LONG_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_RESPONSE: u64 = 64 * 1024 * 1024;

/// The per-call timeout for a route.
pub fn timeout_for(method: &str, path: &str) -> Duration {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("GET", "/v1/health") => HEALTH_TIMEOUT,
        ("GET", "/v1/sweep" | "/v1/clean/plan")
        | (
            "POST",
            "/v1/clean"
            | "/v1/clean/plan"
            | "/v1/sweep"
            | "/v1/worktree/plan"
            | "/v1/worktree/remove"
            | "/v1/prune",
        ) => LONG_TIMEOUT,
        _ => DEFAULT_TIMEOUT,
    }
}

/// A client of this user's running engine.
#[derive(Debug, Clone)]
pub struct Client {
    port: u16,
    token: String,
}

impl Client {
    /// Connect to the engine for this environment, or `None` when none runs
    /// (callers then work locally).
    pub fn connect() -> Option<Self> {
        let paths = Paths::from_env();
        let port = crate::config::effective_port(&Config::load_from(&paths).config);
        Self::connect_in(&paths, port).ok()
    }

    /// Connect after checking that the listener on `port` belongs to the engine
    /// holding this user's engine lock (and, on Linux, that the socket is owned
    /// by this uid). The token is sent only after that check.
    pub fn connect_in(paths: &Paths, port: u16) -> Result<Self, String> {
        verify_listener(paths, port)?;
        let (code, v) =
            http(port, "GET", "/v1/health", None, None, HEALTH_TIMEOUT).map_err(|e| e.message)?;
        if code != 200 || v["name"] != "mulchie" {
            return Err(format!("port {port} does not answer as Mulchie"));
        }
        let token = read_token(paths).map_err(|e| e.to_string())?;
        Ok(Self { port, token })
    }

    /// Like [`Client::connect_in`], with the read-only token: reads and
    /// plans work, everything else is refused by the engine.
    pub fn connect_read_only_in(paths: &Paths, port: u16) -> Result<Self, String> {
        verify_listener(paths, port)?;
        let (code, v) =
            http(port, "GET", "/v1/health", None, None, HEALTH_TIMEOUT).map_err(|e| e.message)?;
        if code != 200 || v["name"] != "mulchie" {
            return Err(format!("port {port} does not answer as Mulchie"));
        }
        let token = read_read_only_token(paths).map_err(|e| e.to_string())?;
        Ok(Self { port, token })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Call a route; errors are the server's message.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.request(method, path, body, timeout_for(method, path))
            .map_err(|e| e.message)
    }

    /// Call a route with an explicit timeout; errors carry status and code.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiError> {
        let (code, v) = http(self.port, method, path, Some(&self.token), body, timeout)?;
        if code == 200 {
            return Ok(v);
        }
        Err(ApiError {
            status: code,
            code: v["code"].as_str().unwrap_or("http_error").to_string(),
            message: v["error"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("HTTP {code}")),
        })
    }
}

fn verify_listener(paths: &Paths, port: u16) -> Result<(), String> {
    let holder =
        lock::running_engine(paths).ok_or_else(|| "no Mulchie engine is running".to_string())?;
    if holder.port != Some(port) {
        return Err(match holder.port {
            Some(p) => format!("the running engine listens on port {p}, not {port}"),
            None => "the running engine has no API listener".into(),
        });
    }
    if cfg!(target_os = "linux") {
        let uid = unsafe { libc::getuid() };
        match lock::listener_uid(port) {
            Some(u) if u == uid => {}
            Some(u) => return Err(format!("port {port} is held by uid {u}, not you")),
            None => return Err(format!("nothing listens on 127.0.0.1:{port}")),
        }
    }
    Ok(())
}

/// One request; returns the status and the parsed JSON body.
fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
    timeout: Duration,
) -> Result<(u16, serde_json::Value), ApiError> {
    let down = |e: std::io::Error| {
        ApiError::new(0, "unreachable", format!("mulchie is not running ({e})"))
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(down)?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .map_err(|e| ApiError::new(0, "io_error", e.to_string()))?;
    let mut raw = Vec::new();
    (&mut s)
        .take(MAX_RESPONSE)
        .read_to_end(&mut raw)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ApiError::new(
                0,
                "timeout",
                format!("no answer from mulchie within {}s", timeout.as_secs()),
            ),
            _ => ApiError::new(0, "io_error", e.to_string()),
        })?;
    let raw = String::from_utf8_lossy(&raw);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let v = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    Ok((code, v))
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
