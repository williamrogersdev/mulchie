//! The activity record of each session: `<config dir>/sessions/<id>.jsonl`,
//! one JSON event per line, append-only, files 0600 in a 0700 directory.
//!
//! Writes go through a bounded queue to one writer thread, so the sample
//! loop never waits on the disk; a full queue drops the event and counts it.
//! Every event carries `seq` (1, 2, … per session), `t` (unix seconds) and
//! `type`. Caps: per session `max_record_mb` and `max_events` (then one
//! `truncated` event and nothing more), process starts at most
//! `process_events_per_minute` per session (the rest summed up in a
//! `process_starts_summarised` event), and `max_total_mb` for the directory
//! (the oldest records of ended sessions go first). Records older than
//! `retain_days` are removed. Anything stored here has been redacted by the
//! caller.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{create_private_dir, SessionsConfig};

/// Events the queue holds before it drops.
pub const QUEUE: usize = 4096;
/// How often the engine asks for retention and the total cap.
pub const SWEEP_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordLimits {
    pub max_bytes: u64,
    pub max_events: u64,
    pub max_total_bytes: u64,
    pub retain_secs: u64,
    pub per_minute: u64,
}

impl RecordLimits {
    pub fn from_config(c: &SessionsConfig) -> Self {
        Self {
            max_bytes: c.max_record_mb * 1_048_576,
            max_events: c.max_events,
            max_total_bytes: c.max_total_mb * 1_048_576,
            retain_secs: c.retain_days * 86_400,
            per_minute: c.process_events_per_minute,
        }
    }
}

enum Msg {
    Event {
        session: String,
        t: u64,
        ty: String,
        fields: Value,
    },
    ProcessStart {
        session: String,
        t: u64,
        pid: u32,
        name: String,
        cmd: String,
    },
    /// Write any pending summary for a session (it ended).
    Close {
        session: String,
        t: u64,
    },
    Limits(RecordLimits),
    /// Retention and the total cap now; `live` sessions are kept.
    Sweep {
        now: u64,
        live: Vec<String>,
    },
    Flush(std::sync::mpsc::Sender<()>),
}

/// The writer's handle. Cloning shares the same writer.
#[derive(Clone)]
pub struct Record {
    dir: PathBuf,
    tx: SyncSender<Msg>,
    error: Arc<Mutex<Option<String>>>,
    dropped: Arc<AtomicU64>,
}

impl Record {
    /// Start a writer for `dir` (created 0700 on first write).
    pub fn new(dir: PathBuf, limits: RecordLimits) -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        let error = Arc::new(Mutex::new(None));
        let w = Writer {
            dir: dir.clone(),
            limits,
            files: HashMap::new(),
            minutes: HashMap::new(),
            error: error.clone(),
        };
        let _ = std::thread::Builder::new()
            .name("mulchie-record".into())
            .spawn(move || w.run(rx));
        Self {
            dir,
            tx,
            error,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn send(&self, m: Msg) {
        match self.tx.try_send(m) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Append one event (`fields` must be an object; it is redacted already).
    pub fn event(&self, session: &str, t: u64, ty: &str, fields: Value) {
        self.send(Msg::Event {
            session: session.to_string(),
            t,
            ty: ty.to_string(),
            fields,
        });
    }

    /// A process joined the session (coalesced per minute).
    pub fn process_start(&self, session: &str, t: u64, pid: u32, name: &str, cmd: &str) {
        self.send(Msg::ProcessStart {
            session: session.to_string(),
            t,
            pid,
            name: name.to_string(),
            cmd: cmd.to_string(),
        });
    }

    pub fn close(&self, session: &str, t: u64) {
        self.send(Msg::Close {
            session: session.to_string(),
            t,
        });
    }

    pub fn set_limits(&self, l: RecordLimits) {
        self.send(Msg::Limits(l));
    }

    pub fn sweep(&self, now: u64, live: Vec<String>) {
        self.send(Msg::Sweep { now, live });
    }

    /// Wait (up to 5 s) until everything queued so far is written.
    pub fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.tx.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    /// Why the record could not be written, while that is so, including
    /// events dropped because the queue was full.
    pub fn error(&self) -> Option<String> {
        let e = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let d = self.dropped.load(Ordering::Relaxed);
        match (e, d) {
            (None, 0) => None,
            (Some(e), 0) => Some(e),
            (None, d) => Some(format!(
                "{d} record event(s) dropped: the writer fell behind"
            )),
            (Some(e), d) => Some(format!("{e}; {d} record event(s) dropped")),
        }
    }

    /// Events of `session` with `seq` above `since`, at most `limit`.
    pub fn read(&self, session: &str, since: u64, limit: usize) -> std::io::Result<Vec<Value>> {
        read_events(&self.dir, session, since, limit)
    }
}

/// The file of a session id (ids are `s-<digits>-<digits>`; anything else
/// is refused by the caller first).
pub fn file_of(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Events of `session` with `seq` above `since`, at most `limit`, oldest
/// first. A missing file is no events.
pub fn read_events(
    dir: &Path,
    session: &str,
    since: u64,
    limit: usize,
) -> std::io::Result<Vec<Value>> {
    let f = match std::fs::File::open(file_of(dir, session)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in std::io::BufReader::new(f).lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["seq"].as_u64().is_some_and(|s| s > since) {
            out.push(v);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

struct FileState {
    bytes: u64,
    events: u64,
    truncated: bool,
}

/// Process starts in the current minute of one session.
struct Minute {
    minute: u64,
    written: u64,
    over: u64,
    names: BTreeMap<String, u64>,
}

struct Writer {
    dir: PathBuf,
    limits: RecordLimits,
    files: HashMap<String, FileState>,
    minutes: HashMap<String, Minute>,
    error: Arc<Mutex<Option<String>>>,
}

impl Writer {
    fn run(mut self, rx: Receiver<Msg>) {
        while let Ok(m) = rx.recv() {
            match m {
                Msg::Event {
                    session,
                    t,
                    ty,
                    fields,
                } => {
                    self.summarise_if_new_minute(&session, t);
                    self.append(&session, t, &ty, fields);
                }
                Msg::ProcessStart {
                    session,
                    t,
                    pid,
                    name,
                    cmd,
                } => self.process_start(&session, t, pid, name, cmd),
                Msg::Close { session, t } => {
                    self.summarise(&session, t);
                    self.minutes.remove(&session);
                    self.files.remove(&session);
                }
                Msg::Limits(l) => self.limits = l,
                Msg::Sweep { now, live } => self.sweep(now, &live),
                Msg::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    fn set_error(&self, e: Option<String>) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = e;
    }

    /// Size and last `seq` of an existing file, read once per session.
    fn state_of(&mut self, session: &str) -> &mut FileState {
        let path = file_of(&self.dir, session);
        self.files.entry(session.to_string()).or_insert_with(|| {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let mut events = 0;
            let mut truncated = false;
            if bytes > 0 {
                if let Ok(f) = std::fs::File::open(&path) {
                    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            events = v["seq"].as_u64().unwrap_or(events);
                            truncated |= v["type"] == "truncated";
                        }
                    }
                }
            }
            FileState {
                bytes,
                events,
                truncated,
            }
        })
    }

    fn append(&mut self, session: &str, t: u64, ty: &str, fields: Value) {
        let limits = self.limits;
        let st = self.state_of(session);
        if st.truncated {
            return;
        }
        let mut obj = match fields {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let seq = st.events + 1;
        obj.insert("seq".into(), json!(seq));
        obj.insert("t".into(), json!(t));
        obj.insert("type".into(), json!(ty));
        let mut line = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
        line.push(b'\n');
        // Room is kept for the marker; past either cap, write it instead.
        let marker_room = 160;
        let over = st.events + 2 > limits.max_events
            || st.bytes + line.len() as u64 + marker_room > limits.max_bytes;
        let line = if over {
            st.truncated = true;
            let mut m = serde_json::to_vec(&json!({
                "seq": seq,
                "t": t,
                "type": "truncated",
                "detail": "the record reached its size or event cap; later events are not kept",
            }))
            .unwrap_or_default();
            m.push(b'\n');
            m
        } else {
            line
        };
        let path = file_of(&self.dir, session);
        let res = write_line(&self.dir, &path, &line);
        let st = self.state_of(session);
        match res {
            Ok(()) => {
                st.bytes += line.len() as u64;
                st.events = seq;
                self.set_error(None);
            }
            Err(e) => {
                if over {
                    st.truncated = false;
                }
                self.set_error(Some(format!(
                    "could not write the session record {}: {e}",
                    path.display()
                )));
            }
        }
    }

    fn process_start(&mut self, session: &str, t: u64, pid: u32, name: String, cmd: String) {
        self.summarise_if_new_minute(session, t);
        let per = self.limits.per_minute;
        let m = self.minutes.entry(session.to_string()).or_insert(Minute {
            minute: t / 60,
            written: 0,
            over: 0,
            names: BTreeMap::new(),
        });
        if m.written < per {
            m.written += 1;
            self.append(
                session,
                t,
                "process_start",
                json!({ "pid": pid, "name": name, "cmd": cmd }),
            );
        } else {
            m.over += 1;
            if m.names.len() < 64 || m.names.contains_key(&name) {
                *m.names.entry(name).or_default() += 1;
            }
        }
    }

    fn summarise_if_new_minute(&mut self, session: &str, t: u64) {
        if self
            .minutes
            .get(session)
            .is_some_and(|m| m.minute != t / 60)
        {
            self.summarise(session, t);
            self.minutes.remove(session);
        }
    }

    /// Write the overflow of the current minute, if any.
    fn summarise(&mut self, session: &str, t: u64) {
        let Some(m) = self.minutes.get_mut(session) else {
            return;
        };
        if m.over == 0 {
            return;
        }
        let mut top: Vec<(String, u64)> = std::mem::take(&mut m.names).into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top.truncate(10);
        let fields = json!({
            "minute_unix": m.minute * 60,
            "count": m.over,
            "names": top.into_iter().collect::<BTreeMap<_, _>>(),
        });
        m.over = 0;
        self.append(session, t, "process_starts_summarised", fields);
    }

    /// Remove records past the retention, then the oldest records of
    /// sessions not in `live` until the directory fits its cap.
    fn sweep(&mut self, now: u64, live: &[String]) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let live: HashSet<&str> = live.iter().map(String::as_str).collect();
        let mut files: Vec<(u64, u64, PathBuf, String)> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let Some(stem) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
                .map(String::from)
            else {
                continue;
            };
            if crate::session::Session::parse_id(&stem).is_none() {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            if !m.is_file() {
                continue;
            }
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(now);
            files.push((mtime, m.len(), p, stem));
        }
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        for (mtime, len, path, stem) in &files {
            if live.contains(stem.as_str()) {
                continue;
            }
            let expired = now.saturating_sub(*mtime) > self.limits.retain_secs;
            let over = total > self.limits.max_total_bytes;
            if (expired || over) && std::fs::remove_file(path).is_ok() {
                total = total.saturating_sub(*len);
                self.files.remove(stem);
            }
        }
    }
}

fn write_line(dir: &Path, path: &Path, line: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    create_private_dir(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(line)
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
