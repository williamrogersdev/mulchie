//! The activity record of each session: `<config dir>/sessions/<id>.jsonl`,
//! one JSON event per line, append-only, files 0600 in a 0700 directory.
//!
//! Writes go through a bounded queue to one writer thread, so the sample
//! loop never waits on the disk; a full queue drops the event and counts it.
//! Every event carries `seq` (1, 2, … per session), `t` (unix seconds) and
//! `type`. Caps: per session `max_record_mb` and `max_events` (then one
//! `truncated` event and nothing more), process starts at most
//! `process_events_per_minute` per session (the rest summed up in a
//! `process_starts_summarised` event), and `max_total_mb` for the directory:
//! the oldest records of ended sessions go first on each sweep, and at write
//! time a session whose next event would take the directory past the cap gets
//! one `truncated` event and nothing more. Every string in an event is cut at
//! 4096 bytes. Records older than
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

/// Any string in a stored event is cut at this many bytes.
pub const MAX_FIELD_BYTES: usize = 4096;

/// Cut every string in `v` at [`MAX_FIELD_BYTES`].
fn cap_strings(v: &mut Value) {
    match v {
        Value::String(s) if s.len() > MAX_FIELD_BYTES => {
            let mut end = MAX_FIELD_BYTES;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            s.push('…');
        }
        Value::Array(a) => a.iter_mut().for_each(cap_strings),
        Value::Object(m) => m.values_mut().for_each(cap_strings),
        _ => {}
    }
}

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
    /// Messages queued and not yet handled by the writer.
    pending: Arc<AtomicU64>,
    index: Arc<EventIndex>,
}

impl Record {
    /// Start a writer for `dir` (created 0700 on first write).
    pub fn new(dir: PathBuf, limits: RecordLimits) -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        let error = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicU64::new(0));
        let w = Writer {
            dir: dir.clone(),
            limits,
            files: HashMap::new(),
            minutes: HashMap::new(),
            error: error.clone(),
            total: None,
            pending: pending.clone(),
        };
        let _ = std::thread::Builder::new()
            .name("mulchie-record".into())
            .spawn(move || w.run(rx));
        Self {
            dir,
            tx,
            error,
            dropped: Arc::new(AtomicU64::new(0)),
            pending,
            index: Arc::new(EventIndex::default()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn send(&self, m: Msg) {
        // Counted before the send, so the writer never sees it handled first.
        self.pending.fetch_add(1, Ordering::SeqCst);
        match self.tx.try_send(m) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::SeqCst);
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
        self.flush_within(Duration::from_secs(5));
    }

    /// For a reader: when events are still queued, wait for them at most
    /// `max`; with nothing queued, return at once.
    pub fn flush_pending(&self, max: Duration) {
        if self.pending.load(Ordering::SeqCst) > 0 {
            self.flush_within(max);
        }
    }

    fn flush_within(&self, max: Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(max);
        } else {
            self.pending.fetch_sub(1, Ordering::SeqCst);
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
        Ok(self.page(session, Query::Since { since, limit })?.events)
    }

    /// One page of `session`'s events, through this record's index.
    pub fn page(&self, session: &str, q: Query) -> std::io::Result<Page> {
        self.index.read(&self.dir, session, q)
    }
}

/// The file of a session id (ids are `s-<digits>-<digits>`; anything else
/// is refused by the caller first).
pub fn file_of(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Events of `session` with `seq` above `since`, at most `limit`, oldest
/// first. A missing file is no events. Without a [`Record`] (and its index),
/// this reads from the start of the file.
pub fn read_events(
    dir: &Path,
    session: &str,
    since: u64,
    limit: usize,
) -> std::io::Result<Vec<Value>> {
    Ok(EventIndex::default()
        .read(dir, session, Query::Since { since, limit })?
        .events)
}

/// Open a record for reading or appending: never through a symlink, never
/// blocking (on a FIFO), and only a regular file with one link. `None` when
/// there is no file.
fn open_plain(path: &Path, append: bool) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut o = std::fs::OpenOptions::new();
    if append {
        o.create(true).append(true).mode(0o600);
    } else {
        o.read(true);
    }
    let f = match o
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let m = f.metadata()?;
    if !m.file_type().is_file() || m.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a plain file", path.display()),
        ));
    }
    Ok(Some(f))
}

/// One mark every this many events: a read seeks to the mark at or before
/// the first event it wants, so it reads at most this many events it skips.
const STRIDE: u64 = 128;
/// Sessions whose index is kept; past this the index starts over.
const INDEXED: usize = 256;

/// Which events a read wants. Both answer oldest first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Query {
    /// `seq` above `since`, at most `limit`.
    Since { since: u64, limit: usize },
    /// The last `n` events with `seq` below `before` (of the whole record
    /// without it).
    Tail { n: usize, before: Option<u64> },
}

/// A page of events.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Page {
    pub events: Vec<Value>,
    /// The `before` for the page just older than this one; `None` when this
    /// page starts at the first event (or is empty).
    pub prev_before: Option<u64>,
}

/// Where each file's lines start, built once and extended as the file grows.
#[derive(Default)]
struct Index {
    ino: u64,
    /// Bytes indexed so far (whole lines only).
    scanned: u64,
    last_seq: u64,
    /// `(seq, byte offset of its line)`, one per [`STRIDE`] events.
    marks: Vec<(u64, u64)>,
}

impl Index {
    /// Index the lines written since the last call.
    fn extend(&mut self, f: &mut std::fs::File, len: u64) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        if self.scanned >= len {
            return Ok(());
        }
        f.seek(SeekFrom::Start(self.scanned))?;
        let mut r = std::io::BufReader::new(&mut *f);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf)?;
            if n == 0 || buf.last() != Some(&b'\n') {
                break; // the end, or a line still being written
            }
            if let Some(seq) = seq_of(&buf) {
                if self.marks.last().is_none_or(|m| seq >= m.0 + STRIDE) {
                    self.marks.push((seq, self.scanned));
                }
                self.last_seq = self.last_seq.max(seq);
            }
            self.scanned += n as u64;
        }
        Ok(())
    }

    /// The offset to start reading at for events from `seq` on.
    fn offset_for(&self, seq: u64) -> u64 {
        match self.marks.partition_point(|m| m.0 <= seq) {
            0 => 0,
            i => self.marks[i - 1].1,
        }
    }
}

fn seq_of(line: &[u8]) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Seq {
        seq: u64,
    }
    serde_json::from_slice::<Seq>(line).ok().map(|s| s.seq)
}

/// Byte offsets into the session records, so a page of events costs a seek
/// and at most [`STRIDE`] skipped lines instead of a read from the start.
#[derive(Default)]
pub struct EventIndex(Mutex<HashMap<String, Index>>);

impl EventIndex {
    pub fn read(&self, dir: &Path, session: &str, q: Query) -> std::io::Result<Page> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let Some(mut f) = open_plain(&file_of(dir, session), false)? else {
            return Ok(Page::default());
        };
        let m = f.metadata()?;
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= INDEXED && !map.contains_key(session) {
            map.clear();
        }
        let idx = map.entry(session.to_string()).or_default();
        if idx.ino != m.ino() || m.len() < idx.scanned {
            *idx = Index {
                ino: m.ino(),
                ..Index::default()
            };
        }
        idx.extend(&mut f, m.len())?;
        let (lo, hi, limit) = match q {
            Query::Since { since, limit } => (since.saturating_add(1), u64::MAX, limit),
            Query::Tail { n, before } => {
                let hi = before
                    .unwrap_or(u64::MAX)
                    .min(idx.last_seq.saturating_add(1));
                (hi.saturating_sub(n as u64).max(1), hi, n)
            }
        };
        let start = idx.offset_for(lo);
        drop(map);
        let mut events = Vec::new();
        if limit > 0 && lo < hi {
            f.seek(SeekFrom::Start(start))?;
            for line in std::io::BufReader::new(f).lines() {
                let Ok(v) = serde_json::from_str::<Value>(&line?) else {
                    continue;
                };
                let Some(seq) = v["seq"].as_u64() else {
                    continue;
                };
                if seq >= hi {
                    break;
                }
                if seq >= lo {
                    events.push(v);
                    if events.len() >= limit {
                        break;
                    }
                }
            }
        }
        let prev_before = events
            .first()
            .and_then(|e| e["seq"].as_u64())
            .filter(|s| *s > 1);
        Ok(Page {
            events,
            prev_before,
        })
    }
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
    /// Bytes of every record in the directory, live sessions included;
    /// read once, then kept up to date by writes (a sweep re-reads it).
    total: Option<u64>,
    pending: Arc<AtomicU64>,
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
                    // Off the count before the flusher wakes, so it sees
                    // nothing pending once its flush returns.
                    self.pending.fetch_sub(1, Ordering::SeqCst);
                    let _ = done.send(());
                    continue;
                }
            }
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn set_error(&self, e: Option<String>) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = e;
    }

    /// Size and last `seq` of an existing file, read once per session.
    fn state_of(&mut self, session: &str) -> &mut FileState {
        let path = file_of(&self.dir, session);
        self.files.entry(session.to_string()).or_insert_with(|| {
            let bytes = std::fs::symlink_metadata(&path)
                .map(|m| m.len())
                .unwrap_or(0);
            let mut events = 0;
            let mut truncated = false;
            if bytes > 0 {
                if let Ok(Some(f)) = open_plain(&path, false) {
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

    /// Bytes of the records in the directory.
    fn total_bytes(&mut self) -> u64 {
        if let Some(t) = self.total {
            return t;
        }
        let t = record_files(&self.dir).iter().map(|f| f.1).sum();
        self.total = Some(t);
        t
    }

    fn append(&mut self, session: &str, t: u64, ty: &str, mut fields: Value) {
        let limits = self.limits;
        if self.state_of(session).truncated {
            return;
        }
        let total = self.total_bytes();
        let st = self.state_of(session);
        cap_strings(&mut fields);
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
        let own_cap = st.events + 2 > limits.max_events
            || st.bytes + line.len() as u64 + marker_room > limits.max_bytes;
        let all_cap = total + line.len() as u64 + marker_room > limits.max_total_bytes;
        let over = own_cap || all_cap;
        let line = if over {
            st.truncated = true;
            let detail = if own_cap {
                "the record reached its size or event cap; later events are not kept"
            } else {
                "the session records together reached sessions.max_total_mb; later events are not kept"
            };
            let mut m = serde_json::to_vec(&json!({
                "seq": seq,
                "t": t,
                "type": "truncated",
                "detail": detail,
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
                self.total = Some(total + line.len() as u64);
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
        self.total = None;
        let live: HashSet<&str> = live.iter().map(String::as_str).collect();
        let mut files: Vec<(u64, u64, PathBuf, String)> = record_files(&self.dir)
            .into_iter()
            .map(|(m, len, p, stem)| (m.unwrap_or(now), len, p, stem))
            .collect();
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

/// The record files in `dir`: (mtime, bytes, path, session id). Only
/// regular files named after a session id.
fn record_files(dir: &Path) -> Vec<(Option<u64>, u64, PathBuf, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
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
            .map(|d| d.as_secs());
        files.push((mtime, m.len(), p, stem));
    }
    files
}

/// Append one line, through [`open_plain`]: a symlink, FIFO or hard link in
/// the file's place is refused, never followed and never waited on.
fn write_line(dir: &Path, path: &Path, line: &[u8]) -> std::io::Result<()> {
    create_private_dir(dir)?;
    let mut f = open_plain(path, true)?
        .ok_or_else(|| std::io::Error::other("the record could not be created"))?;
    f.write_all(line)
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
