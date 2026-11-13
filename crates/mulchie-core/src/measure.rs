//! Measuring directory trees cheaply: size and newest change in one walk.
//!
//! The disk scan looks at every file of every worktree and cache. On macOS a
//! directory's entries come back with their type, size and modification time
//! from one `getattrlistbulk` call, so no file is `lstat`ed on its own.
//! Elsewhere the entries are read with `read_dir` and each one's metadata
//! (on Linux a `fstatat` against the open directory). Symbolic links are
//! never followed.

use crate::safety::KNOWN_CACHE_DIRS;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What one walk of a tree found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measure {
    /// Allocated bytes of everything that is not a directory.
    pub bytes: u64,
    /// Newest modification time (unix seconds) of a file outside `.git` and
    /// the [`KNOWN_CACHE_DIRS`]; 0 when there is none.
    pub newest: u64,
    /// False when more than the budget of such entries were seen: `newest`
    /// is then only what the walk happened to find.
    pub complete: bool,
}

/// Allocated bytes under `path`.
pub fn dir_size(path: &Path) -> u64 {
    measure_tree(path, usize::MAX).bytes
}

/// Size and newest change of `root` in one pass. `budget` is the number of
/// entries outside caches and `.git` after which `complete` is false; the
/// size is exact either way.
pub fn measure_tree(root: &Path, budget: usize) -> Measure {
    let mut m = Measure {
        complete: true,
        ..Measure::default()
    };
    let mut seen = 0usize;
    // (directory, whether it is inside a cache directory or `.git`)
    let mut stack: Vec<(PathBuf, bool)> = vec![(root.to_path_buf(), false)];
    let mut first = true;
    while let Some((dir, inside)) = stack.pop() {
        // Only the root may be a link (a store kept on another volume).
        let follow = std::mem::take(&mut first);
        let _ = list(&dir, follow, &mut |e: Entry<'_>| {
            let skip = inside || e.name == ".git" || KNOWN_CACHE_DIRS.iter().any(|k| e.name == *k);
            if !skip {
                seen += 1;
                if seen > budget {
                    m.complete = false;
                }
            }
            if e.is_dir {
                stack.push((dir.join(e.name), skip));
            } else {
                m.bytes += e.alloc;
                if !skip {
                    m.newest = m.newest.max(e.mtime);
                }
            }
        });
    }
    m
}

/// One directory entry.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub name: &'a OsStr,
    /// A real directory (never a link to one).
    pub is_dir: bool,
    /// Allocated bytes; 0 for a directory.
    pub alloc: u64,
    /// Modification time, unix seconds.
    pub mtime: u64,
}

/// Call `f` for each entry of `dir` (not `.` or `..`). With `follow` false
/// a `dir` that is a symbolic link is refused where the platform can tell
/// (macOS); the walk passes false for every directory below its root, so a
/// directory swapped for a link mid-walk is not entered.
pub fn list(dir: &Path, follow: bool, f: &mut dyn FnMut(Entry<'_>)) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        bulk::list(dir, follow, f)
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !follow && std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        list_portable(dir, f)
    }
}

/// `read_dir` and one metadata call per entry.
pub fn list_portable(dir: &Path, f: &mut dyn FnMut(Entry<'_>)) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(dir)?.flatten() {
        // `DirEntry::metadata` does not follow links.
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name();
        let is_dir = meta.is_dir();
        f(Entry {
            name: &name,
            is_dir,
            alloc: if is_dir { 0 } else { meta.blocks() * 512 },
            mtime: u64::try_from(meta.mtime()).unwrap_or(0),
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod bulk {
    use super::Entry;
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const VDIR: u32 = 2;
    /// Entries per call are as many as fit; 64 KiB holds several hundred.
    const BUF: usize = 64 * 1024;

    struct Fd(libc::c_int);
    impl Drop for Fd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    fn u32_at(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }
    fn i32_at(b: &[u8], at: usize) -> Option<i32> {
        Some(i32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }
    fn i64_at(b: &[u8], at: usize) -> Option<i64> {
        Some(i64::from_ne_bytes(b.get(at..at + 8)?.try_into().ok()?))
    }

    /// One packed entry: its length, the attributes returned, then those
    /// attributes in the order of their bits. Every read is bounds-checked;
    /// `None` means the entry was not as documented and is skipped.
    fn parse(e: &[u8]) -> Option<Entry<'_>> {
        // u32 length, then attribute_set_t: common, vol, dir, file, fork.
        let common = u32_at(e, 4)?;
        let file = u32_at(e, 16)?;
        let mut at = 24;
        let mut name: &[u8] = b"";
        if common & libc::ATTR_CMN_NAME != 0 {
            // attrreference_t: the offset is from the reference itself.
            let off = i32_at(e, at)?;
            let len = u32_at(e, at + 4)? as usize;
            let start = usize::try_from(at as i64 + i64::from(off)).ok()?;
            let raw = e.get(start..start.checked_add(len)?)?;
            name = raw.split(|c| *c == 0).next().unwrap_or(b"");
            at += 8;
        }
        let mut is_dir = false;
        if common & libc::ATTR_CMN_OBJTYPE != 0 {
            is_dir = u32_at(e, at)? == VDIR;
            at += 4;
        }
        let mut mtime = 0u64;
        if common & libc::ATTR_CMN_MODTIME != 0 {
            // timespec: 64-bit seconds, 64-bit nanoseconds.
            mtime = u64::try_from(i64_at(e, at)?).unwrap_or(0);
            at += 16;
        }
        let mut alloc = 0u64;
        if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
            alloc = u64::try_from(i64_at(e, at)?).unwrap_or(0);
        }
        if name.is_empty() || name == b"." || name == b".." {
            return None;
        }
        Some(Entry {
            name: OsStr::from_bytes(name),
            is_dir,
            alloc: if is_dir { 0 } else { alloc },
            mtime,
        })
    }

    pub fn list(dir: &Path, follow: bool, f: &mut dyn FnMut(Entry<'_>)) -> std::io::Result<()> {
        let c = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_CLOEXEC
                    | if follow { 0 } else { libc::O_NOFOLLOW },
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = Fd(fd);
        let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
        al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME;
        al.fileattr = libc::ATTR_FILE_ALLOCSIZE;
        // u64 storage keeps the buffer aligned for the kernel's packing.
        let mut store = vec![0u64; BUF / 8];
        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.0,
                    (&raw mut al).cast(),
                    store.as_mut_ptr().cast(),
                    BUF,
                    0,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Ok(());
            }
            let buf: &[u8] = unsafe { std::slice::from_raw_parts(store.as_ptr().cast(), BUF) };
            let mut at = 0usize;
            for _ in 0..n {
                let Some(len) = u32_at(buf, at).map(|l| l as usize) else {
                    break;
                };
                let Some(entry) = at.checked_add(len).and_then(|end| buf.get(at..end)) else {
                    break;
                };
                if len < 24 {
                    break;
                }
                if let Some(e) = parse(entry) {
                    f(e);
                }
                at += len;
            }
        }
    }
}

/// Run the current thread at background priority until the guard drops:
/// the periodic disk scan must give way to everything else. On macOS this is
/// the thread's Darwin background state (lower CPU priority, throttled disk
/// I/O, efficiency cores); on Linux the thread's nice value.
pub struct Background {
    #[cfg(target_os = "linux")]
    was: libc::c_int,
}

impl Background {
    pub fn enter() -> Self {
        #[cfg(target_os = "macos")]
        unsafe {
            libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
        }
        Self {
            #[cfg(target_os = "linux")]
            was: lower_thread_nice(),
        }
    }
}

/// Sets this thread's nice value to 19 and returns what it was.
#[cfg(target_os = "linux")]
fn lower_thread_nice() -> libc::c_int {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;
    let was = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
    unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, 19) };
    was
}

impl Drop for Background {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, 0);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            // Raising it back needs CAP_SYS_NICE on some systems; a scan
            // thread that stays low is harmless.
            libc::setpriority(libc::PRIO_PROCESS, tid, self.was);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;

    /// The walk the scan used before: `lstat` on every path.
    fn slow(root: &Path) -> (u64, u64) {
        let (mut bytes, mut newest) = (0u64, 0u64);
        let mut stack = vec![(root.to_path_buf(), false)];
        while let Some((dir, inside)) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let n = e.file_name();
                let skip = inside || n == ".git" || KNOWN_CACHE_DIRS.iter().any(|k| n == *k);
                let meta = std::fs::symlink_metadata(e.path()).unwrap();
                if meta.is_dir() {
                    stack.push((e.path(), skip));
                } else {
                    bytes += meta.blocks() * 512;
                    if !skip {
                        newest = newest.max(meta.mtime() as u64);
                    }
                }
            }
        }
        (bytes, newest)
    }

    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        std::fs::create_dir_all(r.join("src/deep/er")).unwrap();
        std::fs::create_dir_all(r.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(r.join(".git/objects")).unwrap();
        std::fs::create_dir_all(r.join("empty")).unwrap();
        std::fs::write(r.join("a.txt"), vec![1u8; 10_000]).unwrap();
        std::fs::write(r.join("src/deep/er/b.rs"), vec![2u8; 70_000]).unwrap();
        std::fs::write(r.join("src/zero"), b"").unwrap();
        std::fs::write(r.join("node_modules/pkg/index.js"), vec![3u8; 200_000]).unwrap();
        std::fs::write(r.join(".git/objects/pack"), vec![4u8; 30_000]).unwrap();
        // Long and non-ASCII names, and enough entries for several calls.
        std::fs::write(r.join(format!("{}.txt", "x".repeat(200))), b"long").unwrap();
        std::fs::write(r.join("naïve-ünïcode.txt"), b"u").unwrap();
        std::fs::create_dir(r.join("many")).unwrap();
        for i in 0..3000 {
            std::fs::write(
                r.join(format!("many/file-with-a-fairly-long-name-{i:05}")),
                b"x",
            )
            .unwrap();
        }
        // Links are counted as links, never followed.
        std::os::unix::fs::symlink(r.join("node_modules"), r.join("link-to-dir")).unwrap();
        std::os::unix::fs::symlink(r.join("a.txt"), r.join("link-to-file")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/nowhere", r.join("dangling")).unwrap();
        tmp
    }

    #[test]
    fn one_pass_matches_an_lstat_walk() {
        let tmp = tree();
        // Files in a cache and in .git are newer than the rest: they count
        // for size, not for activity.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        for p in ["a.txt", "src/deep/er/b.rs", "src/zero", "link-to-file"] {
            let f = std::fs::File::options()
                .write(true)
                .open(tmp.path().join(p))
                .unwrap();
            f.set_modified(old).unwrap();
        }
        let (bytes, newest) = slow(tmp.path());
        let m = measure_tree(tmp.path(), usize::MAX);
        assert_eq!(m.bytes, bytes);
        assert_eq!(m.newest, newest);
        assert!(m.complete && m.bytes >= 310_000 && m.newest > 1_600_000_000);
        assert_eq!(dir_size(tmp.path()), bytes);
        // A subtree with only old files reports their time.
        assert_eq!(
            measure_tree(&tmp.path().join("src"), usize::MAX).newest,
            1_600_000_000
        );
    }

    #[test]
    fn lists_every_entry_like_read_dir() {
        let tmp = tree();
        for dir in ["", "many", "src", "empty"] {
            let dir = tmp.path().join(dir);
            let mut fast = BTreeMap::new();
            list(&dir, true, &mut |e| {
                assert!(fast
                    .insert(e.name.to_os_string(), (e.is_dir, e.alloc, e.mtime))
                    .is_none());
            })
            .unwrap();
            let mut slow = BTreeMap::new();
            list_portable(&dir, &mut |e| {
                slow.insert(e.name.to_os_string(), (e.is_dir, e.alloc, e.mtime));
            })
            .unwrap();
            assert_eq!(fast, slow, "{}", dir.display());
        }
        assert_eq!(
            std::fs::read_dir(tmp.path().join("many")).unwrap().count(),
            3000
        );
    }

    #[test]
    fn links_below_the_root_are_not_entered() {
        let tmp = tree();
        // The root may be a link (as before); a directory below it may not.
        let through = measure_tree(&tmp.path().join("link-to-dir"), usize::MAX).bytes;
        assert_eq!(through, dir_size(&tmp.path().join("node_modules")));
        assert!(through >= 200_000);
        assert!(list(&tmp.path().join("link-to-dir"), false, &mut |_| {}).is_err());
        assert!(list(&tmp.path().join("nope"), true, &mut |_| {}).is_err());
        assert_eq!(dir_size(&tmp.path().join("nope")), 0);
    }

    #[test]
    fn the_budget_marks_activity_unknown_but_keeps_the_size() {
        let tmp = tree();
        let all = measure_tree(tmp.path(), usize::MAX);
        let few = measure_tree(tmp.path(), 10);
        assert!(!few.complete);
        assert_eq!(few.bytes, all.bytes);
        // Entries inside caches and .git do not use the budget.
        let only = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(only.path().join("node_modules/a/b")).unwrap();
        std::fs::write(only.path().join("node_modules/a/b/c"), b"x").unwrap();
        assert!(measure_tree(only.path(), 0).complete);
    }

    /// `MULCHIE_BENCH_DIR=<a big folder> cargo test -p mulchie-core --release
    /// measure::tests::bench -- --ignored --nocapture`: the old two walks
    /// against the one pass. Read-only.
    #[test]
    #[ignore]
    fn bench() {
        let Some(dir) = std::env::var_os("MULCHIE_BENCH_DIR").map(PathBuf::from) else {
            return;
        };
        let cpu = || {
            let mut r: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
            let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            s(r.ru_utime) + s(r.ru_stime)
        };
        let _ = measure_tree(&dir, usize::MAX);
        for round in 0..2 {
            let (c, t) = (cpu(), std::time::Instant::now());
            let m = measure_tree(&dir, usize::MAX);
            let (c1, t1) = (cpu() - c, t.elapsed());
            let (c, t) = (cpu(), std::time::Instant::now());
            // What a scan did per worktree: a size walk, then an activity walk.
            let old = (slow(&dir), crate::diskmap::last_activity(&dir));
            let (c2, t2) = (cpu() - c, t.elapsed());
            println!(
                "round {round}: one pass {:.2}s cpu {:.2?} wall ({} bytes) | before {:.2}s cpu {:.2?} wall ({} bytes)",
                c1, t1, m.bytes, c2, t2, old.0 .0
            );
        }
    }

    #[test]
    fn background_priority_enters_and_leaves() {
        let g = Background::enter();
        drop(g);
    }
}
