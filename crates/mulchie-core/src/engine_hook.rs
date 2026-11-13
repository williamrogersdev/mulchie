//! `POST /v1/hook` and `POST /v1/guard/check`: what an agent's hook asks.
//!
//! A hook call names a pid (the hook process's parent: the agent, or the
//! shell it ran the hook through). It is mapped to a session by walking that
//! pid's ancestors; when no session covers it, the nearest agent ancestor
//! (else the nearest that is not a shell) is registered as a session root.
//! Such a session is only reported on, never acted on (a hook may come from
//! any client holding the read-only token). `pre_tool` with a command runs
//! the guard rules; every reply hands over the session's undelivered limit
//! breaches exactly once. The work is a few process-table lookups and
//! in-memory checks: no scan, no subprocess, no network.

use super::*;
use crate::guard::{self, Decision as GuardDecision, Verdict};
use crate::limits::Breach;
use crate::redact::redact_command_line;
use crate::session::{agent_of, RootSource, SessionStatus};

/// Ancestors walked to map a hook pid to a session.
const HOOK_ANCESTRY: usize = 16;
/// Command and detail text kept in the activity record, at most.
const RECORD_TEXT: usize = 2_000;
/// Live sessions registered by hook calls, at most. Past it a hook call
/// still gets a guard decision, with no session.
pub const MAX_HOOK_SESSIONS: usize = 64;
/// A hook's `cwd` is kept only up to this many bytes.
const MAX_CWD_BYTES: usize = 4096;

/// What the hook reports.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    Prompt,
    PreTool,
    PostTool,
    Stop,
    SessionEnd,
    #[serde(other)]
    Other,
}

impl HookEvent {
    fn code(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::Prompt => "prompt",
            HookEvent::PreTool => "pre_tool",
            HookEvent::PostTool => "post_tool",
            HookEvent::Stop => "stop",
            HookEvent::SessionEnd => "session_end",
            HookEvent::Other => "other",
        }
    }
}

/// `POST /v1/hook` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookRequest {
    /// The agent client (`claude`, `codex`, `cursor`, …), 1 to 64 characters.
    pub client: String,
    #[serde(default)]
    pub client_session_id: Option<String>,
    pub event: HookEvent,
    /// The hook process's parent.
    pub pid: u32,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// `POST /v1/hook` reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookReply {
    /// `null` when sessions are off or the pid is not running.
    pub session_id: Option<String>,
    pub decision: GuardDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The guard rule that decided, or `heavy_build` for build advice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Sentences for the agent: machine state and house rules on
    /// `session_start`, build advice under pressure, breaches.
    pub context: Vec<String>,
    /// The session's limit breaches not handed over before; each appears in
    /// exactly one reply.
    pub breaches: Vec<Breach>,
}

/// `POST /v1/guard/check` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuardCheckRequest {
    pub command: String,
    pub cwd: PathBuf,
}

/// `POST /v1/guard/check` reply: the verdict and, under pressure, build
/// advice. Nothing is recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuardCheckReply {
    #[serde(flatten)]
    pub verdict: Verdict,
    /// `mulchie run -- <cmd>` advice for a heavy build while the machine is
    /// busy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advice: Option<String>,
}

/// The git checkout holding `dir` (the nearest ancestor with `.git`), and
/// the branch checked out there. A few `stat`s and one small read.
pub(crate) fn checkout_of(dir: &Path) -> (Option<PathBuf>, Option<String>) {
    for d in dir.ancestors().take(64) {
        let git = d.join(".git");
        let Ok(m) = std::fs::symlink_metadata(&git) else {
            continue;
        };
        let head = if m.is_dir() {
            git.join("HEAD")
        } else if m.is_file() {
            // A worktree: `gitdir: <path>`.
            match std::fs::read_to_string(&git) {
                Ok(s) => match s.trim().strip_prefix("gitdir:") {
                    Some(p) => {
                        let p = PathBuf::from(p.trim());
                        let p = if p.is_absolute() { p } else { d.join(p) };
                        p.join("HEAD")
                    }
                    None => return (Some(d.to_path_buf()), None),
                },
                Err(_) => return (Some(d.to_path_buf()), None),
            }
        } else {
            continue;
        };
        let branch = std::fs::read_to_string(head).ok().and_then(|h| {
            h.trim()
                .strip_prefix("ref: refs/heads/")
                .map(|b| b.to_string())
        });
        return (Some(d.to_path_buf()), branch);
    }
    (None, None)
}

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "tcsh", "csh", "env", "nu", "pwsh",
];

fn capped(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

impl Engine {
    /// The verdict of the guard rules on one command, for a hook or a
    /// check. Pure apart from reading the checkout's branch.
    fn guard_verdict(&self, cfg: &Config, command: &str, cwd: &Path, ws: Option<&Path>) -> Verdict {
        let (found, branch) = checkout_of(cwd);
        let ws = ws.map(Path::to_path_buf).or(found);
        let pids = [self.procs.self_pid()];
        guard::check(
            &guard::GuardInput {
                command,
                cwd,
                workspace: ws.as_deref(),
                home: &self.paths.home,
                config_dir: &self.paths.config_dir,
                current_branch: branch.as_deref(),
                mulchie_pids: &pids,
            },
            &cfg.guard,
        )
    }

    /// `mulchie run` advice for a heavy build while heavy work is not
    /// advised now.
    fn build_advice(&self, cfg: &Config, command: &str) -> Option<String> {
        if !cfg.guard.build_advice || self.advice().ok_to_start_heavy_work {
            return None;
        }
        let cmd = guard::heavy_build(command, &self.paths.home)?;
        Some(format!(
            "The machine is busy: run this build as `mulchie run -- {}` so it waits for a free build slot.",
            capped(&cmd, 400)
        ))
    }

    /// `POST /v1/guard/check`: the guard's verdict, recorded nowhere.
    pub fn guard_check(&self, req: &GuardCheckRequest) -> GuardCheckReply {
        let cfg = self.config();
        GuardCheckReply {
            verdict: self.guard_verdict(&cfg, &req.command, &req.cwd, None),
            advice: self.build_advice(&cfg, &req.command),
        }
    }

    /// The session of `pid`: one the last sample attributed it (or an
    /// ancestor) to, else a new hook-registered root. `None` when sessions
    /// are off or the pid is not the user's running process. The flag says
    /// whether the caller is the client the session is bound to.
    fn hook_session(&self, req: &HookRequest, now: u64, cfg: &Config) -> Option<(String, bool)> {
        if !cfg.sessions.enabled {
            return None;
        }
        let uid = self.procs.current_uid();
        // Live ancestry: the hook's parent may have started after the last
        // sample.
        let mut chain: Vec<crate::process::ProcFacts> = Vec::new();
        let mut cur = req.pid;
        while cur > 1 && chain.len() < HOOK_ANCESTRY && !chain.iter().any(|p| p.pid == cur) {
            let Some(p) = self.procs.get(cur) else { break };
            if p.uid != Some(uid) {
                break;
            }
            cur = p.ppid;
            chain.push(p);
        }
        {
            let mut sup = lock(&self.sup);
            let found = chain
                .iter()
                .find_map(|p| {
                    sup.tracker
                        .session_of(p.pid, p.start_time)
                        .map(String::from)
                })
                .or_else(|| {
                    chain
                        .is_empty()
                        .then(|| sup.tracker.session_of_ancestry(req.pid))
                        .flatten()
                });
            if let Some(id) = found {
                let running = sup
                    .tracker
                    .get(&id)
                    .is_some_and(|s| s.status == SessionStatus::Running);
                if running {
                    sup.tracker.touch(&id, now);
                    let bound = sup.tracker.set_client(
                        &id,
                        Some(req.client.clone()),
                        req.client_session_id.clone(),
                    );
                    return Some((id, bound));
                }
            }
        }
        // Register: the nearest agent, else the nearest non-shell.
        let extra = &cfg.agents.extra;
        let root = chain
            .iter()
            .find(|p| agent_of(&p.name, p.exe.as_deref(), &p.cmd, extra).is_some())
            .or_else(|| {
                chain.iter().find(|p| {
                    let n = p.name.rsplit('/').next().unwrap_or(&p.name);
                    !SHELLS.contains(&n.trim_start_matches('-'))
                })
            })?;
        let cwd = req
            .cwd
            .clone()
            .or_else(|| root.cwd.clone())
            .filter(|c| c.as_os_str().len() <= MAX_CWD_BYTES);
        let ws = cwd.as_deref().and_then(|c| checkout_of(c).0);
        let command = shorten_command(&root.cmd, &self.paths.home);
        let mut sup = lock(&self.sup);
        let hook_live = sup
            .tracker
            .sessions()
            .filter(|s| s.source == RootSource::Hook && s.status == SessionStatus::Running)
            .count();
        if hook_live >= MAX_HOOK_SESSIONS {
            return None;
        }
        let id = sup.tracker.register(
            now,
            root.pid,
            root.start_time,
            &capped(&req.client, 256),
            command.clone(),
            Some(req.client.clone()),
            req.client_session_id.clone(),
            cwd.clone(),
            ws.clone(),
        );
        drop(sup);
        if self.records_on(cfg) {
            self.record_writer().event(
                &id,
                now,
                "session_start",
                json!({
                    "agent": capped(&req.client, 256), "source": "hook",
                    "root_pid": root.pid, "root_start_time": root.start_time,
                    "command": command, "workspace": ws, "cwd": cwd,
                }),
            );
        }
        Some((id, true))
    }

    /// `POST /v1/hook`.
    pub fn hook(&self, req: &HookRequest) -> Result<HookReply, (&'static str, String)> {
        if req.client.is_empty() || req.client.chars().count() > 64 {
            return Err(("invalid_body", "client must be 1 to 64 characters".into()));
        }
        if req
            .client_session_id
            .as_ref()
            .is_some_and(|s| s.chars().count() > 512)
        {
            return Err((
                "invalid_body",
                "client_session_id is at most 512 characters".into(),
            ));
        }
        let now = now_unix();
        let cfg = self.config();
        let (sid, bound) = match self.hook_session(req, now, &cfg) {
            Some((id, b)) => (Some(id), b),
            None => (None, false),
        };
        let mut reply = HookReply {
            session_id: sid.clone(),
            decision: GuardDecision::Allow,
            reason: None,
            rule: None,
            context: Vec::new(),
            breaches: Vec::new(),
        };
        let cwd = req.cwd.clone().unwrap_or_else(|| self.paths.home.clone());
        let ws = sid.as_deref().and_then(|id| {
            lock(&self.sup)
                .tracker
                .get(id)
                .and_then(|s| s.workspace.clone())
        });
        if req.event == HookEvent::PreTool {
            if let Some(cmd) = req.command.as_deref().filter(|c| !c.trim().is_empty()) {
                let v = self.guard_verdict(&cfg, cmd, &cwd, ws.as_deref());
                reply.decision = v.decision;
                reply.reason = v.reason.clone();
                reply.rule = v.rule.map(|r| r.code().to_string());
                if let Some(a) = self.build_advice(&cfg, cmd) {
                    if reply.rule.is_none() {
                        reply.rule = Some("heavy_build".into());
                    }
                    reply.context.push(a);
                }
            }
        }
        if req.event == HookEvent::SessionStart {
            reply
                .context
                .extend(self.session_start_context(&cfg, ws.as_deref()));
        }
        if let Some(id) = &sid {
            // Breaches go only to the client the session is bound to.
            if bound {
                reply.breaches = lock(&self.sup).limits.take_undelivered(id);
            }
            reply.context.extend(
                reply
                    .breaches
                    .iter()
                    .filter(|b| b.active())
                    .map(|b| b.sentence()),
            );
            if self.records_on(&cfg) {
                let text = |s: &Option<String>| {
                    s.as_deref()
                        .map(|c| capped(&redact_command_line(c), RECORD_TEXT))
                };
                let mut ev = json!({
                    "client": capped(&req.client, 256),
                    "tool": req.tool.as_deref().map(|t| capped(&redact_command_line(t), 128)),
                    "command": text(&req.command),
                    "detail": text(&req.detail),
                    "decision": reply.decision,
                    "rule": reply.rule,
                });
                if !bound {
                    // Another caller: its own client fields, flagged.
                    ev["unbound"] = json!(true);
                    ev["client_session_id"] =
                        json!(req.client_session_id.as_deref().map(|c| capped(c, 2048)));
                }
                self.record_writer()
                    .event(id, now, &format!("hook_{}", req.event.code()), ev);
            }
        }
        Ok(reply)
    }

    /// Two or three sentences on the machine, then the house rules.
    fn session_start_context(&self, cfg: &Config, ws: Option<&Path>) -> Vec<String> {
        let adv = self.advice();
        let (health, cpu, mem_free, disk_free) = {
            let st = lock(&self.state);
            let s = &st.snapshot;
            (
                st.health_at(now_unix()),
                s.cpu_pct,
                s.mem_free_pct(),
                s.disk_free,
            )
        };
        let health = serde_json::to_value(health)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let mut out = vec![
            format!(
                "Mulchie: this machine is {health} (CPU {cpu:.0}% busy, {mem_free:.0}% of memory free, {} free on disk).",
                crate::human_bytes(disk_free)
            ),
            format!(
                "{} build slot(s) free; {}",
                adv.free_slots,
                if adv.ok_to_start_heavy_work {
                    "heavy work can start now."
                } else {
                    "hold heavy work until a slot frees up, or queue it with `mulchie run`."
                }
            ),
        ];
        if let Some(b) = cfg.limits.budget_for(ws) {
            let caps: Vec<String> = b
                .caps()
                .into_iter()
                .map(|(n, c)| {
                    let act = serde_json::to_value(c.action)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default();
                    format!("{n} {} ({act} after {}s)", c.max, c.sustain_secs)
                })
                .collect();
            if !caps.is_empty() {
                out.push(format!("This session's budget: {}.", caps.join(", ")));
            }
        }
        out.push(
            "House rules: run heavy builds and tests as `mulchie run -- <command>` so they wait for a build slot. Do not delete build caches or worktrees, or kill processes, to free resources yourself: Mulchie does that through plans the user approves."
                .into(),
        );
        out
    }
}

/// The root's command line for the session, redacted and shortened.
fn shorten_command(args: &[String], home: &Path) -> String {
    let red = crate::redact::redact_args(args).join(" ");
    let home_s = home.display().to_string();
    let s = if !home_s.is_empty() {
        red.replace(&home_s, "~")
    } else {
        red
    };
    capped(&s, 512)
}

#[cfg(test)]
#[path = "engine_hook_tests.rs"]
mod tests;
