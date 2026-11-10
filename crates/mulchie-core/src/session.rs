//! Agent sessions: an agent's root process and everything it started, in the
//! workspace of the root's working directory.
//!
//! A session is identified by its root's pid and start time; its id is
//! `s-<start_time>-<pid>`. Roots are processes whose name, executable or
//! script matches the agent table below or `agents.extra`, and processes a
//! hook call registered. A process belongs to the nearest root among its
//! ancestors, so an agent started by another agent is its own session and no
//! process is counted twice. A process of the same tool directly under a root
//! of that tool (`node …/codex` starting the native `codex`) is the same
//! session, not a nested one.
//!
//! [`Tracker::observe`] is pure: it takes one list of processes per sample
//! and returns what changed. It runs on the sample loop, so it does no I/O.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// One agent tool Mulchie recognises as a session root.
pub struct AgentDef {
    /// The name sessions report.
    pub name: &'static str,
    /// Executable or script names that mean this tool.
    pub names: &'static [&'static str],
    /// npm packages whose path in `node_modules` means this tool when it runs
    /// under `node`, `bun` or `deno`.
    pub packages: &'static [&'static str],
}

/// Built-in agent tools.
///
/// Documented (the binary name is in the vendor's install docs): claude
/// (Claude Code), codex (OpenAI Codex CLI), gemini (Gemini CLI), copilot
/// (GitHub Copilot CLI), cursor-agent (Cursor CLI), aider, goose, opencode.
/// Inferred (from the tools' own package names or common installs, not
/// checked against vendor docs): droid, kimi, amp, qwen, crush, cline.
/// `agy` (Antigravity) is left out: on macOS it is the IDE's launcher, not
/// the agent. Add other tools with `agents.extra`.
pub const AGENTS: &[AgentDef] = &[
    AgentDef {
        name: "claude",
        names: &["claude"],
        packages: &["@anthropic-ai/claude-code"],
    },
    AgentDef {
        name: "codex",
        names: &["codex"],
        packages: &["@openai/codex"],
    },
    AgentDef {
        name: "gemini",
        names: &["gemini"],
        packages: &["@google/gemini-cli"],
    },
    AgentDef {
        name: "copilot",
        names: &["copilot"],
        packages: &["@github/copilot"],
    },
    AgentDef {
        name: "cursor-agent",
        names: &["cursor-agent"],
        packages: &[],
    },
    AgentDef {
        name: "aider",
        names: &["aider"],
        packages: &[],
    },
    AgentDef {
        name: "goose",
        names: &["goose"],
        packages: &[],
    },
    AgentDef {
        name: "opencode",
        names: &["opencode"],
        packages: &["opencode-ai"],
    },
    // Inferred entries.
    AgentDef {
        name: "droid",
        names: &["droid"],
        packages: &[],
    },
    AgentDef {
        name: "kimi",
        names: &["kimi"],
        packages: &[],
    },
    AgentDef {
        name: "amp",
        names: &["amp"],
        packages: &["@sourcegraph/amp"],
    },
    AgentDef {
        name: "qwen",
        names: &["qwen"],
        packages: &["@qwen-code/qwen-code"],
    },
    AgentDef {
        name: "crush",
        names: &["crush"],
        packages: &[],
    },
    AgentDef {
        name: "cline",
        names: &["cline"],
        packages: &[],
    },
];

const RUNTIMES: &[&str] = &["node", "bun", "deno", "python", "python3"];

fn base_name(s: &str) -> &str {
    let b = s.rsplit('/').next().unwrap_or(s);
    [".js", ".cjs", ".mjs", ".py"]
        .iter()
        .find_map(|e| b.strip_suffix(e))
        .unwrap_or(b)
}

/// The agent tool a process is, if it is one: by its name, executable,
/// `argv[0]`, and for a runtime (`node`, `python3`, …) the script it runs
/// or the npm package that script belongs to. `extra` are exact names from
/// `agents.extra`.
pub fn agent_of(
    name: &str,
    exe: Option<&Path>,
    args: &[String],
    extra: &[String],
) -> Option<String> {
    let mut names: Vec<&str> = vec![name];
    let exe_s = exe.map(|e| e.to_string_lossy());
    if let Some(e) = exe_s.as_deref() {
        names.push(base_name(e));
    }
    if let Some(a) = args.first() {
        names.push(base_name(a));
    }
    let runtime = names
        .iter()
        .any(|n| RUNTIMES.contains(n) || n.starts_with("python3"));
    if runtime {
        if let Some(script) = args.iter().skip(1).find(|a| !a.starts_with('-')) {
            names.push(base_name(script));
            for d in AGENTS {
                if d.packages
                    .iter()
                    .any(|p| script.contains(&format!("node_modules/{p}/")))
                {
                    return Some(d.name.to_string());
                }
            }
        }
    }
    for n in &names {
        if n.is_empty() {
            continue;
        }
        if let Some(d) = AGENTS.iter().find(|d| d.names.contains(n)) {
            return Some(d.name.to_string());
        }
        if let Some(x) = extra.iter().find(|x| x.as_str() == *n) {
            return Some(x.clone());
        }
    }
    None
}

/// What the sample loop knows about one process, for sessions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionProc {
    pub pid: u32,
    pub ppid: u32,
    pub start_time: u64,
    pub name: String,
    /// Owned by the current user. Only own processes are counted.
    pub own: bool,
    /// The agent tool, when the process is one ([`agent_of`]).
    pub agent: Option<String>,
    /// Percent of one core.
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    /// Bytes written since the process started; `None` when the platform
    /// does not say.
    pub written: Option<u64>,
    pub stopped: bool,
    /// The checkout its working directory is in.
    pub workspace: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    /// The command line, redacted and shortened.
    pub cmd: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Ended,
}

/// How the root was recognised.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootSource {
    /// The built-in agent table or `agents.extra`.
    Detected,
    /// A hook call named it. Limits only notify for such a session (a hook
    /// may come from any client holding the read-only token).
    Hook,
}

/// Resource use now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// Cores in use (100% of one core is 1.0).
    pub cpu_cores: f64,
    pub memory_mb: f64,
    pub processes: u32,
    /// Bytes written by the session's processes since it started, counting
    /// those that exited. `null`: this platform does not report it.
    pub bytes_written: Option<u64>,
    /// Write rate over the last minute, in MB per hour. `null` as above, or
    /// before a minute of samples.
    pub write_mb_per_hour: Option<f64>,
    pub runtime_secs: u64,
    /// Build slots held by the session's processes.
    pub build_slots: u32,
}

/// The highest value seen of each figure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Peak {
    pub cpu_cores: f64,
    pub memory_mb: f64,
    pub processes: u32,
    pub write_mb_per_hour: Option<f64>,
}

/// One session as State and the API show it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub agent: String,
    pub source: RootSource,
    pub status: SessionStatus,
    pub root_pid: u32,
    pub root_start_time: u64,
    /// The root's command line, redacted.
    pub command: String,
    pub workspace: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    /// The session that started this one's root, if any.
    pub parent: Option<String>,
    /// From the hook: the client (`claude`, `codex`, …) and its own id.
    pub client: Option<String>,
    pub client_session_id: Option<String>,
    pub started_unix: u64,
    pub ended_unix: Option<u64>,
    pub usage: Usage,
    pub peak: Peak,
    /// TCP ports the session's processes listen on (from the last watch run).
    pub listening: Vec<u16>,
}

impl Session {
    pub fn id_for(pid: u32, start_time: u64) -> String {
        format!("s-{start_time}-{pid}")
    }

    /// `s-<start>-<pid>` back to (pid, start time).
    pub fn parse_id(id: &str) -> Option<(u32, u64)> {
        let rest = id.strip_prefix("s-")?;
        let (st, pid) = rest.split_once('-')?;
        if st.is_empty()
            || pid.is_empty()
            || !st.bytes().chain(pid.bytes()).all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some((pid.parse().ok()?, st.parse().ok()?))
    }
}

/// What one sample changed.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Started {
        id: String,
    },
    Ended {
        id: String,
    },
    /// A process joined a running session after it started.
    ProcessStarted {
        id: String,
        pid: u32,
        name: String,
        cmd: String,
    },
}

/// A root a hook call named.
#[derive(Debug, Clone)]
struct Registration {
    client: Option<String>,
    client_session_id: Option<String>,
    cwd: Option<PathBuf>,
    agent: String,
}

/// A live member: start time and the bytes it had written at the last sample.
#[derive(Debug, Clone, Copy)]
struct Member {
    start_time: u64,
    written: Option<u64>,
}

#[derive(Debug, Clone)]
struct Live {
    members: HashMap<u32, Member>,
    /// Bytes written by members that exited.
    written_exited: u64,
    /// (time, bytes written in all) once per sample, for the write rate.
    written_at: VecDeque<(u64, u64)>,
}

/// Every session, live and recently ended.
#[derive(Debug, Default)]
pub struct Tracker {
    sessions: BTreeMap<String, Session>,
    live: HashMap<String, Live>,
    registered: HashMap<(u32, u64), Registration>,
    /// pid → (start time, session id) from the last sample.
    membership: HashMap<u32, (u64, String)>,
    /// The last sample's processes by pid: (ppid, start time, name).
    last_procs: HashMap<u32, (u32, u64, String)>,
}

/// Ended sessions kept in memory (their records stay on disk longer).
pub const ENDED_KEPT: usize = 200;
/// Write-rate window.
const RATE_WINDOW_SECS: u64 = 60;

impl Tracker {
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Live members of a session: (pid, start time).
    pub fn members(&self, id: &str) -> Vec<(u32, u64)> {
        let mut v: Vec<(u32, u64)> = self
            .live
            .get(id)
            .map(|l| l.members.iter().map(|(p, m)| (*p, m.start_time)).collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// The session `pid` belonged to at the last sample, if its start time
    /// matches.
    pub fn session_of(&self, pid: u32, start_time: u64) -> Option<&str> {
        self.membership
            .get(&pid)
            .filter(|(s, _)| *s == start_time)
            .map(|(_, id)| id.as_str())
    }

    /// The session of `pid` or its nearest ancestor from the last sample.
    pub fn session_of_ancestry(&self, pid: u32) -> Option<String> {
        let mut cur = pid;
        let mut seen = HashSet::new();
        while cur > 1 && seen.insert(cur) && seen.len() < 128 {
            if let Some((_, id)) = self.membership.get(&cur) {
                return Some(id.clone());
            }
            cur = self.last_procs.get(&cur).map(|p| p.0).unwrap_or(0);
        }
        None
    }

    /// Make `pid` (started at `start_time`) a session root, for a hook call
    /// that no session covers. Returns the session id. The session appears
    /// at once and is filled in by the next sample.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        now: u64,
        pid: u32,
        start_time: u64,
        agent: &str,
        command: String,
        client: Option<String>,
        client_session_id: Option<String>,
        cwd: Option<PathBuf>,
        workspace: Option<PathBuf>,
    ) -> String {
        let id = Session::id_for(pid, start_time);
        self.registered.insert(
            (pid, start_time),
            Registration {
                client: client.clone(),
                client_session_id: client_session_id.clone(),
                cwd: cwd.clone(),
                agent: agent.to_string(),
            },
        );
        self.sessions.entry(id.clone()).or_insert_with(|| Session {
            id: id.clone(),
            agent: agent.to_string(),
            source: RootSource::Hook,
            status: SessionStatus::Running,
            root_pid: pid,
            root_start_time: start_time,
            command,
            workspace,
            cwd,
            parent: None,
            client,
            client_session_id,
            started_unix: now,
            ended_unix: None,
            usage: Usage::default(),
            peak: Peak::default(),
            listening: Vec::new(),
        });
        self.live.entry(id.clone()).or_insert_with(|| Live {
            members: HashMap::from([(
                pid,
                Member {
                    start_time,
                    written: None,
                },
            )]),
            written_exited: 0,
            written_at: VecDeque::new(),
        });
        self.membership.insert(pid, (start_time, id.clone()));
        id
    }

    /// Bind a session to the first hook client that reports for it. A bound
    /// session is never rebound or overwritten. Returns whether the caller's
    /// `(client, client_session_id)` is the bound pair.
    pub fn set_client(
        &mut self,
        id: &str,
        client: Option<String>,
        client_session_id: Option<String>,
    ) -> bool {
        let Some(s) = self.sessions.get_mut(id) else {
            return false;
        };
        if s.client.is_none() && client.is_some() {
            s.client = client.clone();
            s.client_session_id = client_session_id.clone();
        }
        s.client.is_some() && s.client == client && s.client_session_id == client_session_id
    }

    /// Set the listening ports of each running session from (pid, port) pairs.
    pub fn set_listening(&mut self, listeners: &[(u32, u16)]) {
        let mut by: HashMap<&str, Vec<u16>> = HashMap::new();
        for (pid, port) in listeners {
            if let Some((_, id)) = self.membership.get(pid) {
                by.entry(id.as_str()).or_default().push(*port);
            }
        }
        let by: HashMap<String, Vec<u16>> = by
            .into_iter()
            .map(|(k, mut v)| {
                v.sort_unstable();
                v.dedup();
                (k.to_string(), v)
            })
            .collect();
        for s in self.sessions.values_mut() {
            if s.status == SessionStatus::Running {
                s.listening = by.get(&s.id).cloned().unwrap_or_default();
            }
        }
    }

    /// Update every session from one sample. `slots` are the pids holding
    /// build slots; `retain_secs` is how long ended sessions are listed.
    pub fn observe(
        &mut self,
        now: u64,
        procs: &[SessionProc],
        slots: &[u32],
        retain_secs: u64,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let by_pid: HashMap<u32, &SessionProc> = procs.iter().map(|p| (p.pid, p)).collect();
        self.last_procs = procs
            .iter()
            .map(|p| (p.pid, (p.ppid, p.start_time, p.name.clone())))
            .collect();
        self.registered
            .retain(|(pid, st), _| by_pid.get(pid).is_some_and(|p| p.start_time == *st));

        // Roots: own agents not directly under the same tool, and hook roots.
        let mut roots: HashMap<u32, String> = HashMap::new();
        for p in procs.iter().filter(|p| p.own) {
            if let Some(a) = &p.agent {
                let wrapper = by_pid
                    .get(&p.ppid)
                    .is_some_and(|q| q.own && q.agent.as_deref() == Some(a.as_str()));
                if !wrapper {
                    roots.insert(p.pid, a.clone());
                }
            }
        }
        let detected: HashSet<u32> = roots.keys().copied().collect();
        for ((pid, _), r) in &self.registered {
            roots.entry(*pid).or_insert_with(|| r.agent.clone());
        }

        // Each own process belongs to its nearest root (itself included).
        let mut owner: HashMap<u32, Option<u32>> = HashMap::with_capacity(procs.len());
        for p in procs.iter().filter(|p| p.own) {
            let mut chain = Vec::new();
            let mut cur = p.pid;
            let found = loop {
                if let Some(o) = owner.get(&cur) {
                    break *o;
                }
                if roots.contains_key(&cur) {
                    break Some(cur);
                }
                chain.push(cur);
                let next = by_pid.get(&cur).map(|q| q.ppid).unwrap_or(0);
                if next <= 1 || next == cur || chain.len() > 256 {
                    break None;
                }
                cur = next;
            };
            for c in chain {
                owner.insert(c, found);
            }
            if roots.contains_key(&p.pid) {
                owner.insert(p.pid, Some(p.pid));
            }
        }

        // Group members by root.
        let mut members: HashMap<u32, Vec<&SessionProc>> = HashMap::new();
        for p in procs.iter().filter(|p| p.own) {
            if let Some(Some(r)) = owner.get(&p.pid) {
                members.entry(*r).or_default().push(p);
            }
        }

        let mut membership = HashMap::with_capacity(procs.len());
        let mut running: HashSet<String> = HashSet::new();
        for (root_pid, agent) in &roots {
            let Some(root) = by_pid.get(root_pid) else {
                continue;
            };
            let id = Session::id_for(root.pid, root.start_time);
            running.insert(id.clone());
            let ms = members.get(root_pid).cloned().unwrap_or_default();
            for m in &ms {
                membership.insert(m.pid, (m.start_time, id.clone()));
            }
            // The session that started this root: the nearest root above it.
            let parent = {
                let mut cur = root.ppid;
                let mut hops = 0;
                loop {
                    if cur <= 1 || hops > 256 {
                        break None;
                    }
                    if let Some(q) = by_pid.get(&cur) {
                        if roots.contains_key(&cur) {
                            break Some(Session::id_for(q.pid, q.start_time));
                        }
                        cur = q.ppid;
                    } else {
                        break None;
                    }
                    hops += 1;
                }
            };
            let reg = self.registered.get(&(root.pid, root.start_time)).cloned();
            let is_new = !self.sessions.contains_key(&id);
            let s = self.sessions.entry(id.clone()).or_insert_with(|| Session {
                id: id.clone(),
                agent: agent.clone(),
                source: if detected.contains(root_pid) {
                    RootSource::Detected
                } else {
                    RootSource::Hook
                },
                status: SessionStatus::Running,
                root_pid: root.pid,
                root_start_time: root.start_time,
                command: root.cmd.clone(),
                workspace: root.workspace.clone(),
                cwd: root.cwd.clone(),
                parent: None,
                client: reg.as_ref().and_then(|r| r.client.clone()),
                client_session_id: reg.as_ref().and_then(|r| r.client_session_id.clone()),
                started_unix: now,
                ended_unix: None,
                usage: Usage::default(),
                peak: Peak::default(),
                listening: Vec::new(),
            });
            if is_new {
                events.push(SessionEvent::Started { id: id.clone() });
            }
            // A hook root that turns out to be a detected agent stays as it
            // was registered; the workspace follows the root's cwd.
            if detected.contains(root_pid) {
                s.source = RootSource::Detected;
                s.agent = agent.clone();
            }
            s.parent = parent;
            if s.workspace.is_none() {
                s.workspace = root.workspace.clone();
            }
            if s.cwd.is_none() {
                s.cwd = root
                    .cwd
                    .clone()
                    .or_else(|| reg.as_ref().and_then(|r| r.cwd.clone()));
            }
            if s.command.is_empty() {
                s.command = root.cmd.clone();
            }

            let live = self.live.entry(id.clone()).or_insert_with(|| Live {
                members: HashMap::new(),
                written_exited: 0,
                written_at: VecDeque::new(),
            });
            // Members that left: keep what they wrote.
            let now_pids: HashMap<u32, &SessionProc> = ms.iter().map(|m| (m.pid, *m)).collect();
            let mut exited_bytes = 0u64;
            live.members.retain(|pid, m| {
                let still = now_pids
                    .get(pid)
                    .is_some_and(|p| p.start_time == m.start_time);
                if !still {
                    exited_bytes += m.written.unwrap_or(0);
                }
                still
            });
            live.written_exited += exited_bytes;
            for m in &ms {
                let known = live.members.contains_key(&m.pid);
                if !known && !is_new && m.pid != root.pid {
                    events.push(SessionEvent::ProcessStarted {
                        id: id.clone(),
                        pid: m.pid,
                        name: m.name.clone(),
                        cmd: m.cmd.clone(),
                    });
                }
                live.members.insert(
                    m.pid,
                    Member {
                        start_time: m.start_time,
                        written: m.written,
                    },
                );
            }

            let cpu_cores = ms.iter().map(|m| m.cpu_pct as f64).sum::<f64>() / 100.0;
            let memory_mb = ms.iter().map(|m| m.mem_bytes).sum::<u64>() as f64 / 1_048_576.0;
            let processes = ms.len() as u32;
            let written = if ms.iter().any(|m| m.written.is_none()) {
                None
            } else {
                Some(live.written_exited + ms.iter().filter_map(|m| m.written).sum::<u64>())
            };
            let rate = written.and_then(|w| {
                live.written_at.push_back((now, w));
                while live
                    .written_at
                    .front()
                    .is_some_and(|(t, _)| now.saturating_sub(*t) > RATE_WINDOW_SECS * 2)
                {
                    live.written_at.pop_front();
                }
                let (t0, w0) = live
                    .written_at
                    .iter()
                    .find(|(t, _)| now.saturating_sub(*t) <= RATE_WINDOW_SECS)
                    .copied()?;
                let span = now.saturating_sub(t0);
                (span >= RATE_WINDOW_SECS / 2)
                    .then(|| w.saturating_sub(w0) as f64 / 1_048_576.0 * 3600.0 / span as f64)
            });
            if written.is_none() {
                live.written_at.clear();
            }
            let pids: HashSet<u32> = ms.iter().map(|m| m.pid).collect();
            s.usage = Usage {
                cpu_cores,
                memory_mb,
                processes,
                bytes_written: written,
                write_mb_per_hour: rate,
                runtime_secs: now.saturating_sub(root.start_time),
                build_slots: slots.iter().filter(|p| pids.contains(p)).count() as u32,
            };
            s.peak.cpu_cores = s.peak.cpu_cores.max(cpu_cores);
            s.peak.memory_mb = s.peak.memory_mb.max(memory_mb);
            s.peak.processes = s.peak.processes.max(processes);
            if let Some(r) = rate {
                s.peak.write_mb_per_hour = Some(s.peak.write_mb_per_hour.unwrap_or(0.0).max(r));
            }
            s.status = SessionStatus::Running;
            s.ended_unix = None;
        }
        self.membership = membership;

        // Sessions whose root is gone have ended.
        for s in self.sessions.values_mut() {
            if s.status == SessionStatus::Running && !running.contains(&s.id) {
                s.status = SessionStatus::Ended;
                s.ended_unix = Some(now);
                s.usage.cpu_cores = 0.0;
                s.usage.memory_mb = 0.0;
                s.usage.processes = 0;
                s.usage.build_slots = 0;
                s.listening.clear();
                self.live.remove(&s.id);
                events.push(SessionEvent::Ended { id: s.id.clone() });
            }
        }
        self.prune(now, retain_secs);
        events
    }

    /// Forget ended sessions past the retention, and all but the newest
    /// [`ENDED_KEPT`].
    fn prune(&mut self, now: u64, retain_secs: u64) {
        self.sessions.retain(|_, s| {
            s.ended_unix
                .is_none_or(|e| now.saturating_sub(e) <= retain_secs)
        });
        let mut ended: Vec<(u64, String)> = self
            .sessions
            .values()
            .filter_map(|s| s.ended_unix.map(|e| (e, s.id.clone())))
            .collect();
        if ended.len() > ENDED_KEPT {
            ended.sort();
            for (_, id) in ended.iter().take(ended.len() - ENDED_KEPT) {
                self.sessions.remove(id);
            }
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
