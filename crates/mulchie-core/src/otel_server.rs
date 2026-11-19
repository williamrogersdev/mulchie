//! A minimal OTLP/HTTP trace receiver. The engine that holds the lock runs
//! it while `otel.enabled` is on (see `engine_otel.rs`).
//!
//! It binds loopback addresses only ([`serve`] refuses any other), by default
//! `127.0.0.1:4318`, and takes `POST /v1/traces` with `Content-Type:
//! application/json` or `application/x-protobuf`. A compressed body
//! (`Content-Encoding: gzip`) is refused with 415, since decompressing would
//! need a new dependency. `Host` must name `localhost`, `127.0.0.1`, `[::1]`
//! or `host.docker.internal`, so a web page using DNS rebinding cannot post
//! spans. The last is the name a container uses for the host (Docker
//! Desktop forwards it to the host's loopback); the bind stays loopback.
//! A batch posted under that name has its `process.pid` removed, since a
//! pid from inside a container names some other process on the host.
//!
//! Each connection carries one request and is closed after the answer: 200
//! with an empty `ExportTraceServiceResponse` (`{}` in JSON, an empty body in
//! protobuf), or an error with a `google.rpc.Status`-shaped body in the
//! request's encoding (400 malformed, 404, 405, 408, 411, 413, 415, 431, 503).
//! Limits ([`Limits`]) cap line and header sizes, the body, the time to read a
//! request and the connections served at once. Decoded batches go to a
//! [`Sink`]; `Mutex<Aggregator>` is one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::otel::{self, Aggregator, DecodeLimits, TraceBatch};

/// The standard OTLP/HTTP port.
pub const DEFAULT_PORT: u16 = 4318;
pub const TRACES_PATH: &str = "/v1/traces";

/// `127.0.0.1:4318`.
pub fn default_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))
}

/// Where decoded batches go.
pub trait Sink: Send + Sync {
    fn accept(&self, batch: TraceBatch);
}

impl Sink for Mutex<Aggregator> {
    fn accept(&self, batch: TraceBatch) {
        // A panic elsewhere must not stop every later batch.
        let mut agg = self.lock().unwrap_or_else(|e| e.into_inner());
        agg.ingest(&batch, otel::now_ms());
    }
}

/// A closure as a [`Sink`].
pub struct FnSink<F>(pub F);

impl<F: Fn(TraceBatch) + Send + Sync> Sink for FnSink<F> {
    fn accept(&self, batch: TraceBatch) {
        (self.0)(batch)
    }
}

/// Request size and time limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest request line or header line.
    pub line: usize,
    /// All header lines together.
    pub headers: usize,
    pub body: usize,
    /// From accepting the connection to having read the whole request.
    pub deadline: Duration,
    pub max_connections: usize,
    pub decode: DecodeLimits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            line: 8 * 1024,
            headers: 32 * 1024,
            body: 4 * 1024 * 1024,
            deadline: Duration::from_secs(10),
            max_connections: 16,
            decode: DecodeLimits::default(),
        }
    }
}

/// The running receiver. [`Server::stop`] (or dropping it) closes the
/// listener and waits for the accept thread.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop accepting connections and wait for the accept thread. Requests
    /// already being read finish on their own threads.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // Wake the blocking accept on the address actually bound.
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `addr` (port 0 picks a free one; see [`Server::port`]) and serve on a
/// background thread, one thread per connection. Any address that is not
/// loopback is refused with `InvalidInput`.
pub fn serve(addr: SocketAddr, sink: Arc<dyn Sink>, limits: Limits) -> std::io::Result<Server> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("the OTLP receiver binds loopback addresses only, not {addr}"),
        ));
    }
    let listener = TcpListener::bind(addr)?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let stop2 = stop.clone();
    let handle = std::thread::Builder::new()
        .name("mulchie-otlp".into())
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
                let sink = sink.clone();
                let slot = Slot(active.clone());
                let _ = std::thread::Builder::new()
                    .name("mulchie-otlp-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle_conn(&stream, &*sink, &limits);
                    });
            }
        })?;
    Ok(Server {
        addr,
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

/// The most time spent turning one client away, or draining after an answer.
const DRAIN_BUDGET: Duration = Duration::from_millis(100);

fn reject_busy(stream: &TcpStream) {
    let _ = stream.set_write_timeout(Some(DRAIN_BUDGET / 2));
    let resp = Resp::error(
        Encoding::Json,
        503,
        14,
        "too many connections; try again shortly",
    );
    let _ = write_response(stream, &resp);
    drain(stream);
}

/// Close our side, then read (and discard) what the client still sends for a
/// short while, so an early answer is not lost to a reset.
fn drain(stream: &TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let deadline = Instant::now() + DRAIN_BUDGET;
    let mut buf = [0u8; 4096];
    let mut left: usize = 256 * 1024;
    while left > 0 {
        let rest = deadline.saturating_duration_since(Instant::now());
        if rest.is_zero() || stream.set_read_timeout(Some(rest)).is_err() {
            break;
        }
        match (&*stream).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

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

fn handle_conn(stream: &TcpStream, sink: &dyn Sink, limits: &Limits) {
    let _ = stream.set_write_timeout(Some(limits.deadline));
    let deadline = Instant::now() + limits.deadline;
    let mut reader = BufReader::new(DeadlineStream { stream, deadline });
    let resp = match read_request(&mut reader, stream, limits, deadline) {
        Ok(req) => {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle(&req, sink, limits)
            }));
            r.unwrap_or_else(|_| Resp::error(req.encoding, 500, 13, "the request failed"))
        }
        Err(ReadError::Closed) => return,
        Err(ReadError::Resp(r)) => r,
    };
    let _ = write_response(stream, &resp);
    drain(stream);
}

/// Decode the body and pass it to the sink.
fn handle(req: &Request, sink: &dyn Sink, limits: &Limits) -> Resp {
    let decoded = match req.encoding {
        Encoding::Json => otel::decode_json(&req.body, &limits.decode),
        Encoding::Protobuf => otel::decode_protobuf(&req.body, &limits.decode),
    };
    match decoded {
        Ok(mut batch) => {
            if req.container {
                for rs in &mut batch.resource_spans {
                    rs.resource.retain(|a| a.key != "process.pid");
                }
            }
            sink.accept(batch);
            Resp::ok(req.encoding)
        }
        Err(e) => Resp::error(req.encoding, 400, 3, &format!("malformed request: {e}")),
    }
}

// ---------------------------------------------------------------- requests

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Json,
    Protobuf,
}

/// A request that passed every check except decoding the body.
#[derive(Debug)]
struct Request {
    encoding: Encoding,
    body: Vec<u8>,
    /// Posted under `Host: host.docker.internal`.
    container: bool,
}

enum ReadError {
    /// The client closed (or never sent) the request; nothing to answer.
    Closed,
    Resp(Resp),
}

fn io_err(e: std::io::Error, enc: Encoding) -> ReadError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => timed_out(enc),
        _ => ReadError::Closed,
    }
}

fn timed_out(enc: Encoding) -> ReadError {
    ReadError::Resp(Resp::error(
        enc,
        408,
        4,
        "the request was not received in time",
    ))
}

fn bad(enc: Encoding, status: u16, code: u32, msg: &str) -> ReadError {
    ReadError::Resp(Resp::error(enc, status, code, msg))
}

/// One line without its CRLF, at most `max` bytes. `Ok(None)` when the stream
/// ends before any byte.
fn read_line<R: BufRead>(
    r: &mut R,
    max: usize,
    deadline: Instant,
    too_long: &dyn Fn() -> ReadError,
) -> Result<Option<Vec<u8>>, ReadError> {
    let mut out = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(timed_out(Encoding::Json));
        }
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(e, Encoding::Json)),
        };
        if buf.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Err(bad(Encoding::Json, 400, 3, "connection closed mid-line"))
            };
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        out.extend_from_slice(&buf[..take]);
        r.consume(take);
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

/// The media type of a `Content-Type` value, lower case, without parameters.
fn media_type(v: &str) -> String {
    v.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The name containers use for the host.
const CONTAINER_HOST: &str = "host.docker.internal";

/// The host name of a `Host` value, port and brackets dropped, lowercase.
fn host_name(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    h.split_once(':').map(|(n, _)| n.to_string()).unwrap_or(h)
}

/// Whether a `Host` value names this machine's loopback, or the host as a
/// container sees it ([`CONTAINER_HOST`]).
fn host_ok(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => {
                return inner == "::1";
            }
            _ => return false,
        }
    } else {
        h.split_once(':').map(|(n, _)| n).unwrap_or(&h)
    };
    name == "localhost" || name == "127.0.0.1" || name == CONTAINER_HOST
}

fn read_request<R: BufRead>(
    r: &mut R,
    stream: &TcpStream,
    limits: &Limits,
    deadline: Instant,
) -> Result<Request, ReadError> {
    let line_too_long = || bad(Encoding::Json, 414, 3, "the request line is too long");
    let Some(line) = read_line(r, limits.line, deadline, &line_too_long)? else {
        return Err(ReadError::Closed);
    };
    let line = String::from_utf8(line)
        .map_err(|_| bad(Encoding::Json, 400, 3, "the request line is not UTF-8"))?;
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(bad(Encoding::Json, 400, 3, "malformed request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(bad(Encoding::Json, 400, 3, "only HTTP/1.0 and HTTP/1.1"));
    }
    let path = target.split('?').next().unwrap_or("");

    let headers_too_large = || bad(Encoding::Json, 431, 3, "the request headers are too large");
    let mut total = 0usize;
    let mut content_type: Option<String> = None;
    let mut content_encoding: Option<String> = None;
    let mut content_length: Option<usize> = None;
    let mut host: Option<String> = None;
    let mut chunked = false;
    let mut expect_continue = false;
    loop {
        let Some(h) = read_line(r, limits.line, deadline, &headers_too_large)? else {
            return Err(bad(
                Encoding::Json,
                400,
                3,
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
            .map_err(|_| bad(Encoding::Json, 400, 3, "a header is not UTF-8"))?;
        let Some((name, value)) = h.split_once(':') else {
            return Err(bad(Encoding::Json, 400, 3, "a header line has no colon"));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(bad(Encoding::Json, 400, 3, "malformed Content-Length"));
                }
                let n: usize = value.parse().unwrap_or(usize::MAX);
                if content_length.is_some_and(|c| c != n) {
                    return Err(bad(Encoding::Json, 400, 3, "conflicting Content-Length"));
                }
                content_length = Some(n);
            }
            "content-type" => content_type = Some(media_type(value)),
            "content-encoding" => content_encoding = Some(value.to_ascii_lowercase()),
            "transfer-encoding" => chunked = true,
            "host" if host.is_some() => {
                return Err(bad(Encoding::Json, 400, 3, "duplicate Host header"));
            }
            "host" => host = Some(value.to_string()),
            "expect" => expect_continue = value.eq_ignore_ascii_case("100-continue"),
            _ => {}
        }
    }

    if path != TRACES_PATH {
        return Err(bad(
            Encoding::Json,
            404,
            5,
            &format!("not found; send traces to POST {TRACES_PATH}"),
        ));
    }
    if method != "POST" {
        return Err(bad(Encoding::Json, 405, 12, "only POST is allowed"));
    }
    let encoding = match content_type.as_deref() {
        Some("application/json") => Encoding::Json,
        Some("application/x-protobuf") => Encoding::Protobuf,
        _ => {
            return Err(bad(
                Encoding::Json,
                415,
                3,
                "Content-Type must be application/json or application/x-protobuf",
            ))
        }
    };
    if !host.as_deref().is_some_and(host_ok) {
        return Err(bad(
            encoding,
            403,
            7,
            "Host must be localhost, 127.0.0.1, [::1] or host.docker.internal",
        ));
    }
    match content_encoding.as_deref() {
        None | Some("identity") | Some("") => {}
        Some(other) => {
            return Err(bad(
                encoding,
                415,
                3,
                &format!(
                    "Content-Encoding {other} is not supported; send uncompressed \
                     (OTEL_EXPORTER_OTLP_COMPRESSION=none, or compression: none in a Collector exporter)"
                ),
            ))
        }
    }
    if chunked {
        return Err(bad(
            encoding,
            411,
            3,
            "Transfer-Encoding is not supported; send Content-Length",
        ));
    }
    let len = content_length.unwrap_or(0);
    if len > limits.body {
        return Err(bad(
            encoding,
            413,
            8,
            &format!("the body is limited to {} bytes", limits.body),
        ));
    }
    if expect_continue {
        let mut s = stream;
        let _ = s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; len];
    let mut got = 0;
    while got < len {
        if Instant::now() >= deadline {
            return Err(timed_out(encoding));
        }
        match r.read(&mut body[got..]) {
            Ok(0) => {
                return Err(bad(
                    encoding,
                    400,
                    3,
                    "the body is shorter than Content-Length",
                ))
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e, encoding)),
        }
    }
    let container = host
        .as_deref()
        .is_some_and(|h| host_name(h) == CONTAINER_HOST);
    Ok(Request {
        encoding,
        body,
        container,
    })
}

// --------------------------------------------------------------- responses

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resp {
    status: u16,
    encoding: Encoding,
    body: Vec<u8>,
}

impl Resp {
    /// An empty `ExportTraceServiceResponse`.
    fn ok(encoding: Encoding) -> Self {
        let body = match encoding {
            Encoding::Json => b"{}".to_vec(),
            Encoding::Protobuf => Vec::new(),
        };
        Self {
            status: 200,
            encoding,
            body,
        }
    }

    /// A `google.rpc.Status` with a gRPC `code` and a message.
    fn error(encoding: Encoding, status: u16, code: u32, message: &str) -> Self {
        let body = match encoding {
            Encoding::Json => serde_json::json!({ "code": code, "message": message })
                .to_string()
                .into_bytes(),
            Encoding::Protobuf => {
                let mut b = Vec::with_capacity(message.len() + 8);
                b.push(0x08); // field 1, varint
                put_varint(&mut b, u64::from(code));
                b.push(0x12); // field 2, length-delimited
                put_varint(&mut b, message.len() as u64);
                b.extend_from_slice(message.as_bytes());
                b
            }
        };
        Self {
            status,
            encoding,
            body,
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_response(mut s: &TcpStream, r: &Resp) -> std::io::Result<()> {
    let ctype = match r.encoding {
        Encoding::Json => "application/json",
        Encoding::Protobuf => "application/x-protobuf",
    };
    let allow = if r.status == 405 {
        "Allow: POST\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{allow}Connection: close\r\n\r\n",
        r.status,
        reason_phrase(r.status),
        r.body.len(),
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(&r.body);
    s.write_all(&out)?;
    s.flush()
}

#[cfg(test)]
#[path = "otel_server_tests.rs"]
mod tests;
