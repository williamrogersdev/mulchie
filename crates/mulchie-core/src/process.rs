//! The process table as a trait, so busy detection, attribution and the signal
//! checks can run against a fake table in tests.

use std::path::PathBuf;
use std::sync::Mutex;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// What the safety gate and attribution need to know about one process,
/// read fresh from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcFacts {
    pub pid: u32,
    pub ppid: u32,
    pub uid: Option<u32>,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Seconds since the epoch. Together with `pid` it identifies a process.
    pub start_time: u64,
}

pub trait ProcessSource: Send + Sync {
    /// Every process, with working directories read now.
    fn list(&self) -> Vec<ProcFacts>;
    /// One process, read now.
    fn get(&self, pid: u32) -> Option<ProcFacts> {
        self.list().into_iter().find(|p| p.pid == pid)
    }
    fn current_uid(&self) -> u32;
    fn self_pid(&self) -> u32;
}

/// The real process table via `sysinfo`. Every call refreshes.
pub struct SystemProcesses {
    sys: Mutex<Option<System>>,
}

impl SystemProcesses {
    pub fn new() -> Self {
        Self {
            sys: Mutex::new(None),
        }
    }

    fn refresh(&self, which: ProcessesToUpdate<'_>) -> Vec<ProcFacts> {
        let mut guard = self.sys.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = System::new().ok();
        }
        let Some(sys) = guard.as_mut() else {
            return Vec::new();
        };
        sys.refresh_processes_specifics(
            which,
            true,
            ProcessRefreshKind::nothing()
                .with_cwd(UpdateKind::Always)
                .with_user(UpdateKind::Always)
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always),
        );
        sys.processes()
            .iter()
            .filter(|(_, p)| p.thread_kind().is_none())
            .map(|(pid, p)| ProcFacts {
                pid: pid.as_u32(),
                ppid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                uid: p.user_id().map(|u| **u),
                name: p.name().to_string_lossy().to_string(),
                exe: p.exe().map(|e| e.to_path_buf()),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect(),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                start_time: p.start_time(),
            })
            .collect()
    }
}

impl Default for SystemProcesses {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSource for SystemProcesses {
    fn list(&self) -> Vec<ProcFacts> {
        self.refresh(ProcessesToUpdate::All)
    }

    fn get(&self, pid: u32) -> Option<ProcFacts> {
        let p = Pid::from_u32(pid);
        self.refresh(ProcessesToUpdate::Some(&[p]))
            .into_iter()
            .find(|f| f.pid == pid)
    }

    fn current_uid(&self) -> u32 {
        unsafe { libc::getuid() }
    }

    fn self_pid(&self) -> u32 {
        std::process::id()
    }
}

/// A fixed process table for tests and dry runs.
#[derive(Debug, Clone, Default)]
pub struct FakeProcesses {
    pub procs: Vec<ProcFacts>,
    pub uid: u32,
    pub pid: u32,
}

impl FakeProcesses {
    /// An empty table owned by the current uid, with an impossible self pid.
    pub fn empty() -> Self {
        Self {
            procs: Vec::new(),
            uid: unsafe { libc::getuid() },
            pid: u32::MAX,
        }
    }

    /// Add a process owned by the table's uid with the given working directory.
    pub fn with(mut self, pid: u32, name: &str, cwd: Option<PathBuf>, start_time: u64) -> Self {
        self.procs.push(ProcFacts {
            pid,
            ppid: 1,
            uid: Some(self.uid),
            name: name.into(),
            exe: None,
            cmd: vec![name.into()],
            cwd,
            start_time,
        });
        self
    }
}

impl ProcessSource for FakeProcesses {
    fn list(&self) -> Vec<ProcFacts> {
        self.procs.clone()
    }
    fn current_uid(&self) -> u32 {
        self.uid
    }
    fn self_pid(&self) -> u32 {
        self.pid
    }
}

/// Working directories of the user's own processes (except this one), canonicalized
/// where possible.
pub fn own_cwds(src: &dyn ProcessSource) -> Vec<PathBuf> {
    let uid = src.current_uid();
    let me = src.self_pid();
    let mut out: Vec<PathBuf> = src
        .list()
        .into_iter()
        .filter(|p| p.uid == Some(uid) && p.pid != me)
        .filter_map(|p| p.cwd)
        .map(|c| c.canonicalize().unwrap_or(c))
        .collect();
    out.sort();
    out.dedup();
    out
}
