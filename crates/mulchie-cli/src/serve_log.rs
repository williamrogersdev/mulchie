//! Keep `~/Library/Logs/mulchie-serve.log` small.
//!
//! launchd opens the file named in the service's plist for `mulchie serve`'s
//! stdout and stderr and never rotates it. When the file `serve` writes to is
//! that log and has grown past [`MAX_BYTES`], `serve` copies it to
//! `mulchie-serve.log.1` (replacing the previous copy) and empties it. The
//! file was opened for appending, so later writes land at its new end. Only a
//! regular file named [`LOG_NAME`] and open for appending is touched: output
//! redirected anywhere else is left alone.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The log the service writes to (see `service/dev.mulchie.serve.plist`).
pub const LOG_NAME: &str = "mulchie-serve.log";
/// Past this size the log is copied aside and emptied.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// How often `serve` looks.
pub const EVERY: Duration = Duration::from_secs(600);

/// The path of the file open on `fd`, if the system can say.
pub fn fd_path(fd: RawFd) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes into buf.
        let r = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
        if r == -1 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0)?;
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
    }
}

/// Copy the log open on `fd` aside and empty it when it is [`LOG_NAME`],
/// open for appending and larger than `max`. Returns whether it did.
pub fn keep_small(fd: RawFd, max: u64) -> std::io::Result<bool> {
    // SAFETY: fstat writes into a zeroed stat struct; F_GETFL reads flags.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Ok(false);
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG || (st.st_size as u64) <= max {
        return Ok(false);
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || flags & libc::O_APPEND == 0 {
        return Ok(false);
    }
    let Some(path) = fd_path(fd) else {
        return Ok(false);
    };
    if path.file_name().and_then(|n| n.to_str()) != Some(LOG_NAME) {
        return Ok(false);
    }
    std::fs::copy(&path, aside(&path))?;
    // SAFETY: ftruncate on a descriptor this process owns.
    if unsafe { libc::ftruncate(fd, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// `<log>.1`.
pub fn aside(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Check stdout and stderr now and every [`EVERY`] on a background thread.
pub fn start() {
    let check = || {
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            match keep_small(fd, MAX_BYTES) {
                Ok(true) => eprintln!(
                    "mulchie: the log passed {} MiB; the earlier part is in {}.1",
                    MAX_BYTES >> 20,
                    LOG_NAME
                ),
                Ok(false) => {}
                Err(e) => eprintln!("mulchie: could not shorten the log: {e}"),
            }
        }
    };
    check();
    let _ = std::thread::Builder::new()
        .name("serve-log".into())
        .spawn(move || loop {
            std::thread::sleep(EVERY);
            check();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    fn open(path: &Path, append: bool) -> std::fs::File {
        let mut o = std::fs::OpenOptions::new();
        o.create(true).mode(0o644);
        if append {
            o.append(true);
        } else {
            o.write(true);
        }
        o.open(path).unwrap()
    }

    #[test]
    fn a_large_serve_log_is_copied_aside_and_emptied() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().canonicalize().unwrap().join(LOG_NAME);
        let mut f = open(&path, true);
        f.write_all(&[b'a'; 200]).unwrap();
        // Small enough: left alone.
        assert!(!keep_small(f.as_raw_fd(), 500).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 200);
        assert!(keep_small(f.as_raw_fd(), 100).unwrap());
        assert_eq!(std::fs::read(aside(&path)).unwrap(), vec![b'a'; 200]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // Appending goes on at the new end, with no hole.
        f.write_all(b"next\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"next\n");
    }

    #[test]
    fn other_files_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().canonicalize().unwrap();
        // Another name.
        let other = dir.join("my-output.txt");
        let mut f = open(&other, true);
        f.write_all(&[b'a'; 200]).unwrap();
        assert!(!keep_small(f.as_raw_fd(), 100).unwrap());
        assert_eq!(std::fs::metadata(&other).unwrap().len(), 200);
        // The right name, not open for appending.
        let log = dir.join(LOG_NAME);
        let mut f = open(&log, false);
        f.write_all(&[b'a'; 200]).unwrap();
        assert!(!keep_small(f.as_raw_fd(), 100).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 200);
        assert!(!aside(&log).exists());
        // Not a regular file.
        let (r, _w) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!keep_small(r.as_raw_fd(), 0).unwrap());
    }
}
