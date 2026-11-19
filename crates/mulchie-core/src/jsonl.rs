//! An append-only file of JSON lines that Mulchie keeps for itself
//! (`commits.jsonl`, `timings.jsonl`, `devservers.jsonl`): mode 0600 in the private config
//! directory, never opened through a symbolic link, renamed to `<file>.1`
//! (replacing the one before) when the next line would pass its size, as the
//! action journal is. Nothing is deleted.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::create_private_dir;

/// Lines longer than this are skipped when read.
const MAX_LINE: usize = 64 * 1024;

pub struct JsonlFile {
    path: PathBuf,
    max_bytes: u64,
    lock: Mutex<()>,
}

impl JsonlFile {
    pub fn at(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn rotated(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_os_string();
        s.push(".1");
        PathBuf::from(s)
    }

    /// Append one line, rotating first when the file would pass its size.
    /// Refuses (`O_NOFOLLOW`) when the file is a symbolic link.
    pub fn append<T: Serialize>(&self, v: &T) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(v).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let meta = std::fs::symlink_metadata(&self.path).ok();
        if meta.as_ref().is_some_and(|m| !m.file_type().is_file()) {
            return Err(std::io::Error::other(format!(
                "{} is not a plain file",
                self.path.display()
            )));
        }
        let size = meta.map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 > self.max_bytes {
            std::fs::rename(&self.path, self.rotated())?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)?;
        f.write_all(&line)
    }

    /// Every entry, oldest first, from the rotated file and the current one.
    /// A file that is not a plain file is not read; lines that do not parse
    /// are skipped.
    pub fn read_all<T: DeserializeOwned>(&self) -> Vec<T> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for p in [self.rotated(), self.path.clone()] {
            let plain = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_file());
            if !plain {
                continue;
            }
            let Ok(f) = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&p)
            else {
                continue;
            };
            let mut r = std::io::BufReader::new(f);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if buf.len() > MAX_LINE {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice::<T>(&buf) {
                    out.push(v);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct E {
        n: u64,
    }

    #[test]
    fn appends_rotates_and_reads_both_halves() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let f = JsonlFile::at(t.path().join("cfg/x.jsonl"), 30);
        for n in 0..10 {
            f.append(&E { n }).unwrap();
        }
        let all: Vec<E> = f.read_all();
        assert!(all.len() >= 3 && all.len() < 10, "{all:?}");
        assert_eq!(all.last().unwrap().n, 9);
        assert!(f.rotated().exists());
        assert!(!t.path().join("cfg/x.jsonl.2").exists());
        let mode = std::fs::metadata(f.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_symlink_is_neither_written_through_nor_read() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("elsewhere");
        std::fs::write(&target, "{\"n\":1}\n").unwrap();
        let link = t.path().join("x.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let f = JsonlFile::at(link, 1 << 20);
        assert!(f.append(&E { n: 2 }).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"n\":1}\n");
        assert!(f.read_all::<E>().is_empty());
    }

    #[test]
    fn bad_lines_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let f = JsonlFile::at(t.path().join("x.jsonl"), 1 << 20);
        f.append(&E { n: 1 }).unwrap();
        let mut h = std::fs::OpenOptions::new()
            .append(true)
            .open(f.path())
            .unwrap();
        h.write_all(b"not json\n").unwrap();
        f.append(&E { n: 2 }).unwrap();
        assert_eq!(f.read_all::<E>(), vec![E { n: 1 }, E { n: 2 }]);
    }
}
