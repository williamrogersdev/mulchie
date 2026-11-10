//! One engine per user: an exclusive `flock` on `<config dir>/engine.lock`.
//!
//! The holder writes `{pid, port, started_unix}` into the file. A second engine
//! (the app started while `mulchie serve` runs, or the other way round) sees
//! [`LockError::Held`] and becomes a client instead of running its own loops.
//! Clients use [`running_engine`] to check that the listener they are about to
//! send the token to was announced by an engine that holds the lock.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::config::create_private_dir;
use crate::paths::Paths;

/// What the lock holder announces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// The API port, once the listener is bound. `None` while starting or when
    /// the API could not bind.
    pub port: Option<u16>,
    pub started_unix: u64,
}

#[derive(Debug)]
pub enum LockError {
    /// Another engine holds the lock. Its announcement, when readable.
    Held(Option<Holder>),
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held(Some(h)) => match h.port {
                Some(p) => write!(
                    f,
                    "another Mulchie engine is running (pid {}, port {p})",
                    h.pid
                ),
                None => write!(f, "another Mulchie engine is running (pid {})", h.pid),
            },
            LockError::Held(None) => write!(f, "another Mulchie engine is running"),
            LockError::Io(e) => write!(f, "engine lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// The held lock. Dropping it (or the process exiting) releases it.
#[derive(Debug)]
pub struct EngineLock {
    file: File,
    path: PathBuf,
    holder: Holder,
}

pub fn lock_path(paths: &Paths) -> PathBuf {
    paths.config_dir.join("engine.lock")
}

fn open_lock_file(path: &Path, create: bool) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    if create {
        o.create(true).mode(0o600);
    }
    let f = o.open(path)?;
    check_private(path, &f)?;
    Ok(f)
}

/// The file must belong to us and be readable and writable by us alone.
fn check_private(path: &Path, f: &File) -> std::io::Result<()> {
    let meta = f.metadata()?;
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} belongs to uid {}, not {uid}",
                path.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} has mode {:o}; it must be 600",
                path.display(),
                meta.mode() & 0o777
            ),
        ));
    }
    Ok(())
}

fn flock(f: &File, op: libc::c_int) -> std::io::Result<bool> {
    if unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Release this open file's lock. Closing the file is not enough while a
/// copy of the descriptor is open elsewhere: a child that another thread is
/// spawning shares every descriptor until its exec closes the close-on-exec
/// ones, and the lock lasts until the last copy closes. An explicit unlock
/// ends it for every copy at once.
fn unlock(f: &File) {
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
}

/// Whether nobody holds the lock on `f`'s file. Takes a shared lock to find
/// out and releases it explicitly (see [`unlock`]), so the probe never keeps
/// an engine from taking the lock.
fn unheld(f: &File) -> std::io::Result<bool> {
    let free = flock(f, libc::LOCK_SH)?;
    if free {
        unlock(f);
    }
    Ok(free)
}

fn read_holder(mut f: &File) -> Option<Holder> {
    let mut s = String::new();
    f.seek(SeekFrom::Start(0)).ok()?;
    f.take(4096).read_to_string(&mut s).ok()?;
    serde_json::from_str(s.trim()).ok()
}

impl EngineLock {
    /// Take the lock without waiting.
    pub fn acquire(paths: &Paths, now: u64) -> Result<Self, LockError> {
        create_private_dir(&paths.config_dir).map_err(LockError::Io)?;
        let path = lock_path(paths);
        let file = open_lock_file(&path, true).map_err(LockError::Io)?;
        match flock(&file, libc::LOCK_EX) {
            Ok(true) => {}
            Ok(false) => return Err(LockError::Held(read_holder(&file))),
            Err(e) => return Err(LockError::Io(e)),
        }
        let lock = Self {
            file,
            path,
            holder: Holder {
                pid: std::process::id(),
                port: None,
                started_unix: now,
            },
        };
        lock.write().map_err(LockError::Io)?;
        Ok(lock)
    }

    fn write(&self) -> std::io::Result<()> {
        let mut f = &self.file;
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(serde_json::to_string(&self.holder)?.as_bytes())?;
        f.sync_data()
    }

    /// Announce the API port once the listener is bound.
    pub fn set_port(&mut self, port: Option<u16>) -> std::io::Result<()> {
        self.holder.port = port;
        self.write()
    }

    pub fn holder(&self) -> &Holder {
        &self.holder
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EngineLock {
    /// Release the lock before the file closes (see [`unlock`]), so it is
    /// free the moment the holder is dropped.
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

/// The engine that holds the lock now, if one does. `None` when no file, the
/// file is not ours or not private, or nobody holds the lock (a stale file left
/// by a crashed engine).
pub fn running_engine(paths: &Paths) -> Option<Holder> {
    let path = lock_path(paths);
    let dir = std::fs::symlink_metadata(&paths.config_dir).ok()?;
    let uid = unsafe { libc::getuid() };
    if !dir.is_dir() || dir.uid() != uid || dir.mode() & 0o022 != 0 {
        return None;
    }
    let f = open_lock_file(&path, false).ok()?;
    match unheld(&f) {
        Ok(true) => None,
        Ok(false) => read_holder(&f),
        Err(_) => None,
    }
}

/// The uid owning the IPv4 loopback listener on `port`, from `/proc/net/tcp`.
/// Only Linux exposes this cheaply; elsewhere `None`.
#[cfg(target_os = "linux")]
pub fn listener_uid(port: u16) -> Option<u32> {
    let text = std::fs::read_to_string("/proc/net/tcp").ok()?;
    parse_listener_uid(&text, port)
}

#[cfg(not(target_os = "linux"))]
pub fn listener_uid(_port: u16) -> Option<u32> {
    None
}

/// Find `127.0.0.1:<port>` in LISTEN state (0A) in `/proc/net/tcp` text.
pub fn parse_listener_uid(text: &str, port: u16) -> Option<u32> {
    let want = format!("0100007F:{port:04X}");
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() > 7 && cols[1].eq_ignore_ascii_case(&want) && cols[3] == "0A" {
            return cols[7].parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_one_engine_holds_the_lock() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        assert_eq!(running_engine(&paths), None, "no file yet");
        let mut a = EngineLock::acquire(&paths, 5).unwrap();
        // A second open of the file conflicts, even in the same process.
        match EngineLock::acquire(&paths, 6) {
            Err(LockError::Held(Some(h))) => {
                assert_eq!(h.pid, std::process::id());
                assert_eq!(h.port, None);
            }
            other => panic!("expected Held, got {other:?}"),
        }
        a.set_port(Some(7777)).unwrap();
        let h = running_engine(&paths).unwrap();
        assert_eq!(h.port, Some(7777));
        assert_eq!(h.started_unix, 5);
        let mode = std::fs::metadata(lock_path(&paths))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(a);
        // A stale file from an engine that is gone does not count.
        assert_eq!(running_engine(&paths), None, "released once dropped");
        let b = EngineLock::acquire(&paths, 9).unwrap();
        assert_eq!(b.holder().started_unix, 9);
    }

    /// A copy of the descriptor (what a child being spawned by another thread
    /// holds until its exec) keeps neither the holder's lock nor a probe's.
    #[test]
    fn the_lock_is_free_once_dropped_while_a_copy_of_the_file_is_open() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let a = EngineLock::acquire(&paths, 1).unwrap();
        let copy = a.file.try_clone().unwrap();
        drop(a);
        assert_eq!(running_engine(&paths), None, "the copy does not hold it");
        let b = EngineLock::acquire(&paths, 2).unwrap();
        drop(copy);
        drop(b);

        let probe = open_lock_file(&lock_path(&paths), false).unwrap();
        let copy = probe.try_clone().unwrap();
        assert!(unheld(&probe).unwrap());
        drop(probe);
        let c = EngineLock::acquire(&paths, 3);
        assert!(c.is_ok(), "a probe's shared lock is gone: {c:?}");
        drop(copy);
    }

    #[test]
    fn a_loose_lock_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let a = EngineLock::acquire(&paths, 1).unwrap();
        std::fs::set_permissions(lock_path(&paths), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(
            running_engine(&paths),
            None,
            "not trusted when readable by others"
        );
        drop(a);
        assert!(matches!(
            EngineLock::acquire(&paths, 2),
            Err(LockError::Io(_))
        ));
    }

    #[test]
    fn parses_proc_net_tcp() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1E61 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1\n   1: 0100007F:1F90 00000000:0000 01 00000000:00000000 00:00000000 00000000  1001        0 12346 1\n";
        assert_eq!(parse_listener_uid(text, 7777), Some(1000));
        assert_eq!(parse_listener_uid(text, 8080), None, "not listening");
        assert_eq!(parse_listener_uid(text, 1), None);
    }
}
