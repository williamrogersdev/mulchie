//! Runaway chains: a command that starts itself again and again.
//!
//! A build script that calls itself (`npm run build` whose build step runs
//! `npm run build`) adds a level every second or so. Each level holds its
//! memory while it waits for the next, so the machine runs out of memory and
//! swap within minutes. The shape is unmistakable in the process table: one
//! command line, in one working directory, nested inside copies of itself.
//!
//! [`detect`] finds that shape in one sample and [`Runaway::evaluate`] decides
//! what to do about it; both are pure and run on the sample loop. The engine
//! stops a chain on the limits worker, through the safety gate (see
//! `engine_runaway.rs`). A chain Mulchie saw is remembered for a while as a
//! [`Memo`], so the guard rules can warn before the same command is run in
//! the same directory again.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::config::{MemoryAction, RunawayConfig};
use crate::limits::ActionState;
use crate::memguard::fmt_mb;
use crate::process::ProcFacts;
use crate::session::SessionProc;

/// Stoppings kept for state and the API.
pub const STOPPINGS_KEPT: usize = 50;
/// Commands remembered for the guard rule, at most.
pub const MEMOS_KEPT: usize = 32;
/// A chain the gate could not stop is tried again after this long.
pub const RETRY_SECS: u64 = 30;
/// Ancestors walked for one process, and levels counted, at most.
const MAX_WALK: usize = 4096;
/// Processes of one chain, at most.
const MAX_TREE: usize = 50_000;
/// Self-nesting commands remembered per chain, at most.
const MAX_PHRASES: usize = 4;
/// Sentences waiting for one session's next hook call, at most.
const MAX_NOTES: usize = 8;

/// Runtimes whose first argument may be the script that is the real command.
const RUNTIMES: &[&str] = &[
    "node", "bun", "deno", "tsx", "python", "python3", "ruby", "perl",
];

/// A command as the guard rules see one: the program's file name and its
/// arguments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Phrase {
    pub prog: String,
    pub args: Vec<String>,
}

impl Phrase {
    /// From a process's command line. `node /x/bin/npm run build` is
    /// `npm run build`: a runtime followed by a script path is dropped, and
    /// so is the script's `-cli.js` or `.js` ending.
    pub fn of(cmd: &str) -> Option<Phrase> {
        let mut t: Vec<&str> = cmd.split_whitespace().collect();
        let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
        let first = base(t.first()?);
        if RUNTIMES.contains(&first.as_str())
            && t.get(1)
                .is_some_and(|s| !s.starts_with('-') && s.contains('/'))
        {
            t.remove(0);
        }
        let mut prog = base(t[0]);
        for suffix in ["-cli.js", ".js", ".mjs", ".cjs"] {
            if let Some(p) = prog.strip_suffix(suffix) {
                prog = p.to_string();
                break;
            }
        }
        if prog.is_empty() || prog.starts_with('-') {
            return None;
        }
        Some(Phrase {
            prog,
            args: t[1..].iter().map(|s| s.to_string()).collect(),
        })
    }

    pub fn words(&self) -> String {
        std::iter::once(self.prog.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One command nested inside copies of itself, with everything under it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chain {
    /// `chain:<pid>:<start time>` of the outermost copy.
    pub key: String,
    /// The command line that repeats (redacted, shortened).
    pub command: String,
    pub cwd: Option<PathBuf>,
    /// How many copies are nested inside each other.
    pub depth: usize,
    /// The outermost copy.
    pub root_pid: u32,
    #[serde(skip)]
    pub root_start_time: u64,
    /// The outermost copy and everything it started.
    pub processes: usize,
    pub mem_mb: f64,
    /// The agent session it runs in, when it runs in one.
    pub session_id: Option<String>,
    pub workspace: Option<PathBuf>,
    pub first_seen_unix: u64,
    /// (pid, start time) of every process of the chain in the sample.
    #[serde(skip)]
    pub procs: Vec<(u32, u64)>,
    /// The self-nesting commands in it, for the guard rule.
    #[serde(skip)]
    pub phrases: Vec<Phrase>,
}

impl Chain {
    /// `` `npm run build` in /code/site ``.
    pub fn place(&self) -> String {
        match &self.cwd {
            Some(c) => format!("`{}` in {}", self.command, c.display()),
            None => format!("`{}`", self.command),
        }
    }

    /// One sentence for a person.
    pub fn sentence(&self) -> String {
        format!(
            "{} is running inside {} copies of itself ({} process(es), {})",
            self.place(),
            self.depth,
            self.processes,
            fmt_mb(self.mem_mb)
        )
    }
}

fn sig_of(p: &SessionProc) -> Option<u64> {
    // A login shell (`-zsh`) inside a login shell is a person, not a script.
    if !p.own || p.cmd.is_empty() || p.cmd.starts_with('-') {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.cmd.hash(&mut h);
    p.cwd.hash(&mut h);
    Some(h.finish())
}

/// Every chain in one sample: a command line that, in one working directory,
/// is nested inside at least `depth` copies of itself (itself included).
/// A chain is reported once, from its outermost copy.
pub fn detect(procs: &[SessionProc], depth: usize) -> Vec<Chain> {
    let n = procs.len();
    let depth = depth.max(2);
    let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let sig: Vec<Option<u64>> = procs.iter().map(sig_of).collect();
    // The parent in the sample; a "parent" that started later is a reused pid.
    let parent = |i: usize| -> Option<usize> {
        let j = *by_pid.get(&procs[i].ppid)?;
        (j != i && procs[j].start_time <= procs[i].start_time).then_some(j)
    };
    let same = |a: usize, b: usize| {
        sig[a] == sig[b] && procs[a].cmd == procs[b].cmd && procs[a].cwd == procs[b].cwd
    };
    let nearest_same = |i: usize| -> Option<usize> {
        let mut p = i;
        for _ in 0..MAX_WALK {
            p = parent(p)?;
            if same(p, i) {
                return Some(p);
            }
        }
        None
    };
    // rep: copies of the command from the outermost down to this one;
    // top: the outermost copy.
    let mut rep = vec![0usize; n];
    let mut top = vec![usize::MAX; n];
    for i in 0..n {
        if sig[i].is_none() || rep[i] != 0 {
            continue;
        }
        let mut stack = vec![i];
        let mut base = (0usize, usize::MAX);
        loop {
            match nearest_same(*stack.last().unwrap_or(&i)) {
                Some(j) if rep[j] != 0 => {
                    base = (rep[j], top[j]);
                    break;
                }
                Some(j) if stack.len() < MAX_WALK => stack.push(j),
                _ => break,
            }
        }
        let (mut r, mut t) = base;
        for &k in stack.iter().rev() {
            r += 1;
            if t == usize::MAX {
                t = k;
            }
            rep[k] = r;
            top[k] = t;
        }
    }
    let mut deep: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        if rep[i] >= depth {
            let d = deep.entry(top[i]).or_insert(0);
            *d = (*d).max(rep[i]);
        }
    }
    if deep.is_empty() {
        return Vec::new();
    }
    // A flagged copy under another flagged copy is part of that chain.
    let outermost = |t: usize| -> usize {
        let (mut out, mut p) = (t, t);
        for _ in 0..MAX_WALK {
            match parent(p) {
                Some(j) => {
                    if deep.contains_key(&j) {
                        out = j;
                    }
                    p = j;
                }
                None => break,
            }
        }
        out
    };
    let mut inner: HashMap<usize, Vec<usize>> = HashMap::new();
    for &t in deep.keys() {
        inner.entry(outermost(t)).or_default().push(t);
    }
    let mut kids: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        if parent(i).is_some() {
            kids.entry(p.ppid).or_default().push(i);
        }
    }
    let mut chains: Vec<Chain> = inner
        .into_iter()
        .map(|(o, mut tops)| {
            tops.sort_by_key(|t| (*t != o, procs[*t].pid));
            let mut phrases: Vec<Phrase> = Vec::new();
            for t in &tops {
                if let Some(ph) = Phrase::of(&procs[*t].cmd) {
                    if !phrases.contains(&ph) && phrases.len() < MAX_PHRASES {
                        phrases.push(ph);
                    }
                }
            }
            let mut seen: HashSet<usize> = HashSet::from([o]);
            let mut order = vec![o];
            let mut i = 0;
            while i < order.len() && order.len() < MAX_TREE {
                for &c in kids.get(&procs[order[i]].pid).into_iter().flatten() {
                    if seen.insert(c) {
                        order.push(c);
                    }
                }
                i += 1;
            }
            let root = &procs[o];
            Chain {
                key: format!("chain:{}:{}", root.pid, root.start_time),
                command: root.cmd.clone(),
                cwd: root.cwd.clone(),
                depth: deep[&o],
                root_pid: root.pid,
                root_start_time: root.start_time,
                processes: order.len(),
                mem_mb: order.iter().map(|k| procs[*k].mem_bytes).sum::<u64>() as f64 / 1_048_576.0,
                session_id: None,
                workspace: root.workspace.clone(),
                first_seen_unix: 0,
                procs: order
                    .iter()
                    .map(|k| (procs[*k].pid, procs[*k].start_time))
                    .collect(),
                phrases,
            }
        })
        .collect();
    chains.sort_by_key(|c| c.root_pid);
    chains
}

/// `root` and everything under it in a fresh process list, as (pid, start
/// time). Empty when `root` is not running with that start time.
pub fn tree(list: &[ProcFacts], root: (u32, u64)) -> Vec<(u32, u64)> {
    if !list
        .iter()
        .any(|p| p.pid == root.0 && p.start_time == root.1)
    {
        return Vec::new();
    }
    let mut kids: HashMap<u32, Vec<&ProcFacts>> = HashMap::new();
    for p in list {
        kids.entry(p.ppid).or_default().push(p);
    }
    let mut seen: HashSet<u32> = HashSet::from([root.0]);
    let mut order = vec![root];
    let mut i = 0;
    while i < order.len() && order.len() < MAX_TREE {
        let (pid, start) = order[i];
        for c in kids.get(&pid).into_iter().flatten() {
            // A child that started before its parent is a reused pid.
            if c.pid > 1 && c.start_time >= start && seen.insert(c.pid) {
                order.push((c.pid, c.start_time));
            }
        }
        i += 1;
    }
    order
}

/// One chain Mulchie stopped, or tried to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stopping {
    pub id: String,
    pub at_unix: u64,
    pub key: String,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub depth: usize,
    pub processes: usize,
    pub mem_mb: f64,
    pub session_id: Option<String>,
    pub workspace: Option<PathBuf>,
    pub state: ActionState,
    pub detail: Option<String>,
    #[serde(skip)]
    pub root: (u32, u64),
}

impl Stopping {
    fn place(&self) -> String {
        match &self.cwd {
            Some(c) => format!("`{}` in {}", self.command, c.display()),
            None => format!("`{}`", self.command),
        }
    }

    /// One sentence for a person or an agent.
    pub fn sentence(&self) -> String {
        format!(
            "Mulchie stopped {}: it was running inside {} copies of itself ({} process(es), {}). Something in that command starts the command again; find and fix that before running it again",
            self.place(),
            self.depth,
            self.processes,
            fmt_mb(self.mem_mb)
        )
    }
}

/// A command that ran away, remembered for the `runaway_repeat` guard rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memo {
    /// The command in words (`npm run build`).
    pub command: String,
    #[serde(skip)]
    pub phrase: Phrase,
    pub cwd: PathBuf,
    /// The deepest nesting seen.
    pub depth: usize,
    pub at_unix: u64,
    /// The guard rule matches until then.
    pub until_unix: u64,
}

/// The memo a command (program file name and arguments) run in `cwd` matches.
pub fn repeat_of<'a>(memos: &'a [Memo], prog: &str, args: &[&str], cwd: &Path) -> Option<&'a Memo> {
    memos.iter().find(|m| {
        m.phrase.prog == prog
            && m.cwd == cwd
            && m.phrase.args.len() == args.len()
            && m.phrase.args.iter().zip(args).all(|(a, b)| a == b)
    })
}

/// What one sample asks of the engine.
#[derive(Debug, Default, PartialEq)]
pub struct Decision {
    /// Ids of new stoppings to carry out.
    pub stop: Vec<String>,
    /// Chains seen for the first time.
    pub fresh: Vec<Chain>,
}

#[derive(Debug, Default)]
pub struct Runaway {
    chains: Vec<Chain>,
    /// Chain key → when it was first seen.
    seen: HashMap<String, u64>,
    /// Chain key → not stopped again before this time.
    retry: HashMap<String, u64>,
    stoppings: VecDeque<Stopping>,
    memos: Vec<Memo>,
    /// Session id → sentences for its next hook call.
    notes: HashMap<String, Vec<String>>,
    seq: u64,
}

impl Runaway {
    /// The chains in the last sample.
    pub fn chains(&self) -> &[Chain] {
        &self.chains
    }

    /// Newest first.
    pub fn stoppings(&self) -> impl Iterator<Item = &Stopping> {
        self.stoppings.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Stopping> {
        self.stoppings.iter().find(|s| s.id == id)
    }

    pub fn memos(&self) -> &[Memo] {
        &self.memos
    }

    /// What the gate did for a stopping.
    pub fn set_result(&mut self, id: &str, state: ActionState, detail: String) {
        let Some(s) = self.stoppings.iter_mut().find(|s| s.id == id) else {
            return;
        };
        s.state = state;
        s.detail = Some(detail);
        if matches!(state, ActionState::Done | ActionState::Partial) {
            if let Some(sid) = s.session_id.clone() {
                let text = s.sentence();
                let notes = self.notes.entry(sid).or_default();
                if notes.len() < MAX_NOTES {
                    notes.push(text);
                }
            }
        }
    }

    /// The sentences waiting for a session, each handed over once.
    pub fn take_notes(&mut self, session: &str) -> Vec<String> {
        self.notes.remove(session).unwrap_or_default()
    }

    fn remember(&mut self, c: &Chain, now: u64, secs: u64) {
        let Some(cwd) = &c.cwd else { return };
        for ph in &c.phrases {
            match self
                .memos
                .iter_mut()
                .find(|m| &m.phrase == ph && &m.cwd == cwd)
            {
                Some(m) => {
                    m.depth = m.depth.max(c.depth);
                    m.until_unix = now.saturating_add(secs);
                }
                None => self.memos.push(Memo {
                    command: ph.words(),
                    phrase: ph.clone(),
                    cwd: cwd.clone(),
                    depth: c.depth,
                    at_unix: now,
                    until_unix: now.saturating_add(secs),
                }),
            }
        }
        if self.memos.len() > MEMOS_KEPT {
            self.memos.sort_by_key(|m| std::cmp::Reverse(m.until_unix));
            self.memos.truncate(MEMOS_KEPT);
        }
    }

    /// Look at the chains of one sample. With `cfg.action` `stop` and
    /// `can_act`, every chain gets a stopping (again after [`RETRY_SECS`]
    /// while it is still there).
    pub fn evaluate(
        &mut self,
        now: u64,
        chains: Vec<Chain>,
        cfg: &RunawayConfig,
        can_act: bool,
    ) -> Decision {
        self.memos.retain(|m| m.until_unix > now);
        if cfg.action == MemoryAction::Off {
            self.chains.clear();
            self.seen.clear();
            self.retry.clear();
            return Decision::default();
        }
        let mut out = Decision::default();
        self.chains = chains;
        let keys: HashSet<&str> = self.chains.iter().map(|c| c.key.as_str()).collect();
        self.seen.retain(|k, _| keys.contains(k.as_str()));
        self.retry.retain(|k, _| keys.contains(k.as_str()));
        for i in 0..self.chains.len() {
            let key = self.chains[i].key.clone();
            match self.seen.get(&key) {
                Some(t) => self.chains[i].first_seen_unix = *t,
                None => {
                    self.seen.insert(key.clone(), now);
                    self.chains[i].first_seen_unix = now;
                    out.fresh.push(self.chains[i].clone());
                }
            }
            let c = self.chains[i].clone();
            if cfg.remember_secs > 0 {
                self.remember(&c, now, cfg.remember_secs);
            }
            if cfg.action != MemoryAction::Stop || !can_act {
                continue;
            }
            if self.retry.get(&key).is_some_and(|t| now < *t) {
                continue;
            }
            self.retry.insert(key, now.saturating_add(RETRY_SECS));
            self.seq += 1;
            let id = format!("r-{now}-{}", self.seq);
            self.stoppings.push_front(Stopping {
                id: id.clone(),
                at_unix: now,
                key: c.key,
                command: c.command,
                cwd: c.cwd,
                depth: c.depth,
                processes: c.processes,
                mem_mb: c.mem_mb,
                session_id: c.session_id,
                workspace: c.workspace,
                state: ActionState::Pending,
                detail: None,
                root: (c.root_pid, c.root_start_time),
            });
            self.stoppings.truncate(STOPPINGS_KEPT);
            out.stop.push(id);
        }
        out
    }
}

#[cfg(test)]
#[path = "runaway_tests.rs"]
mod tests;
