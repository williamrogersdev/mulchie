//! What Mulchie remembers about actions that did not succeed.
//!
//! Every cache deletion, worktree removal and cache prune that goes through
//! the engine is already in the action journal as `done` or `skipped` with a
//! reason. This module folds those entries into one record per target: how
//! many times in a row it was skipped, why, and since when. Nothing new is
//! stored: the memory is rebuilt from the journal when the engine starts and
//! kept up to date as actions run.
//!
//! It is used three ways:
//!
//! - **Counted and shown.** `state.failures` lists every target skipped at
//!   least [`SHOWN_AFTER`] times in a row, with the last reason, a sentence
//!   on what to do about it, and when automation will try again.
//! - **Avoided by automation.** After [`BACKOFF_AFTER`] skips in a row the
//!   engine's own passes (automatic sweep and reclaim, and `{"auto": true}`)
//!   leave the target alone for [`backoff_secs`]: 6 hours, doubling with each
//!   further failure, at most 7 days. The engine only does less here, never
//!   more: it does not change the action, widen it or try another way.
//! - **Told to whoever plans.** A plan item for such a target carries
//!   `past_failures`, so a person or an agent sees it failed before and why.
//!   An explicit plan can still be executed: a person may have fixed the
//!   cause, and a success clears the record.
//!
//! Process actions are left out: a pid names a different process each time.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::journal::Action;
use crate::safety::SkipReason;

/// Skips in a row before a target is listed in `state.failures`.
pub const SHOWN_AFTER: u32 = 2;
/// Skips in a row before automation stops retrying for a while.
pub const BACKOFF_AFTER: u32 = 3;
/// The first wait; it doubles with each further failure.
pub const BACKOFF_BASE_SECS: u64 = 6 * 3600;
/// The longest wait.
pub const BACKOFF_MAX_SECS: u64 = 7 * 86_400;
/// A record with no new failure for this long is forgotten.
pub const FORGET_AFTER_SECS: u64 = 30 * 86_400;
/// Records kept; the least recently failed go first.
pub const MAX_RECORDS: usize = 500;

/// Journal kinds whose targets are paths or cache keys (stable names).
const KINDS: [&str; 2] = ["reclaim", "worktree"];

/// How long automation leaves a target alone after `count` skips in a row
/// (0 below [`BACKOFF_AFTER`]).
pub fn backoff_secs(count: u32) -> u64 {
    if count < BACKOFF_AFTER {
        return 0;
    }
    let doublings = (count - BACKOFF_AFTER).min(16);
    BACKOFF_BASE_SECS
        .saturating_mul(1u64 << doublings)
        .min(BACKOFF_MAX_SECS)
}

/// One target that keeps being skipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Failure {
    /// `reclaim` (a build cache or a package cache key) or `worktree`.
    pub kind: String,
    /// The path, or the cache key of a prune.
    pub target: String,
    /// Skips in a row; a success resets it.
    pub count: u32,
    /// Why the last one was skipped.
    pub reason: Option<SkipReason>,
    /// The last skip, in the gate's words.
    pub detail: String,
    pub first_unix: u64,
    pub last_unix: u64,
    /// Mulchie's automatic passes leave the target alone until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_unix: Option<u64>,
    /// What a person can do about it.
    pub advice: String,
}

/// What a plan item says about earlier tries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PastFailures {
    pub count: u32,
    pub reason: Option<SkipReason>,
    pub detail: String,
    pub last_unix: u64,
    pub advice: String,
}

impl From<&Failure> for PastFailures {
    fn from(f: &Failure) -> Self {
        Self {
            count: f.count,
            reason: f.reason,
            detail: f.detail.clone(),
            last_unix: f.last_unix,
            advice: f.advice.clone(),
        }
    }
}

/// One sentence on what to do about a skip that keeps happening.
pub fn advice(reason: Option<SkipReason>) -> &'static str {
    use SkipReason::*;
    match reason {
        Some(BackupFailed) => "The backup could not be written: check that Mulchie's backups directory is writable and the disk has room, or commit the work and remove the worktree yourself.",
        Some(RemoveFailed) => "The removal itself failed: run `git worktree remove <path>` in the repository to see git's own error (for a cache, check the directory's permissions).",
        Some(ChangedDuringBackup) | Some(HeadMoved) | Some(Busy) | Some(NotIdle) | Some(NotStale) => "Something is still using it: a process works there or files keep changing. Find it with inspect_process or list_sessions, or wait until it is idle.",
        Some(IgnoredTooLarge) => "Its gitignored files are larger than backup_ignored_max_mb: move them out, or raise the limit in the config.",
        Some(HasSubmodules) | Some(NestedRepo) | Some(HiddenChanges) => "It holds work Mulchie cannot back up (a submodule, another repository, or files git hides): look at it and remove it by hand if nothing in it is needed.",
        Some(GitFailed) | Some(TimedOut) | Some(CommandFailed) => "A command failed or ran out of time: run `git status` there (or the package manager's own prune) to see the error.",
        Some(Locked) => "git has it locked: `git worktree unlock <path>` if the lock is stale.",
        Some(Dirty) => "It has uncommitted changes: commit them, or plan the removal with allow_dirty so a patch is saved first.",
        Some(PruneDeferred) | Some(NotPrunable) => "git still keeps the entry: `git worktree prune` in the repository removes it once git agrees it is stale.",
        Some(ToolNotFound) | Some(NoPruneCommand) => "The tool that owns this cache is not installed or has no prune command; clear it with that tool yourself.",
        Some(OutsideScanRoots) | Some(NotAbsolute) | Some(NotCanonical) | Some(Symlink) | Some(NotFound) | Some(NotRecognisedCache) | Some(MissingCachedirTag) | Some(NotAWorktree) | Some(NotAgent) | Some(UnknownTarget) | Some(MissingExpectedHead) | Some(NodeModulesDisabled) | Some(XcodeDisabled) => "Mulchie's rules do not allow this target; asking again will not change that.",
        _ => "See the detail of the last try.",
    }
}

/// Skips in a row per target, from the journal.
#[derive(Debug, Default)]
pub struct Memory {
    map: HashMap<(String, String), Failure>,
}

impl Memory {
    /// Fold journal entries, oldest first.
    pub fn from_actions<'a>(actions: impl IntoIterator<Item = &'a Action>) -> Self {
        let mut m = Self::default();
        for a in actions {
            m.note(a);
        }
        m
    }

    /// Take one journal entry into account: a skip counts, a success clears.
    /// Entries of other kinds, or without a target and a status, are ignored.
    /// Returns the record when this entry was a skip.
    pub fn note(&mut self, a: &Action) -> Option<Failure> {
        if !KINDS.contains(&a.kind.as_str()) {
            return None;
        }
        let (Some(target), Some(status)) = (a.target.as_deref(), a.status.as_deref()) else {
            return None;
        };
        let key = (a.kind.clone(), target.to_string());
        match status {
            "done" => {
                self.map.remove(&key);
                None
            }
            "skipped" => {
                // A gap longer than the memory starts a new run.
                if self
                    .map
                    .get(&key)
                    .is_some_and(|f| a.at_unix.saturating_sub(f.last_unix) > FORGET_AFTER_SECS)
                {
                    self.map.remove(&key);
                }
                let f = self.map.entry(key).or_insert_with(|| Failure {
                    kind: a.kind.clone(),
                    target: target.to_string(),
                    count: 0,
                    reason: None,
                    detail: String::new(),
                    first_unix: a.at_unix,
                    last_unix: a.at_unix,
                    retry_after_unix: None,
                    advice: String::new(),
                });
                f.count = f.count.saturating_add(1);
                f.reason = a.reason;
                f.detail = a.detail.clone();
                f.last_unix = f.last_unix.max(a.at_unix);
                f.advice = advice(a.reason).to_string();
                let wait = backoff_secs(f.count);
                f.retry_after_unix = (wait > 0).then(|| f.last_unix.saturating_add(wait));
                let out = f.clone();
                self.trim();
                Some(out)
            }
            _ => None,
        }
    }

    fn trim(&mut self) {
        while self.map.len() > MAX_RECORDS {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, f)| f.last_unix)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            self.map.remove(&oldest);
        }
    }

    /// The record for a target, unless it is older than the memory.
    pub fn get(&self, kind: &str, target: &str, now: u64) -> Option<&Failure> {
        self.map
            .get(&(kind.to_string(), target.to_string()))
            .filter(|f| now.saturating_sub(f.last_unix) <= FORGET_AFTER_SECS)
    }

    /// The record that makes automation leave `target` alone at `now`.
    pub fn backing_off(&self, kind: &str, target: &str, now: u64) -> Option<&Failure> {
        self.get(kind, target, now)
            .filter(|f| f.retry_after_unix.is_some_and(|t| now < t))
    }

    /// What a plan item for `target` should say about earlier tries.
    pub fn past(&self, kind: &str, target: &str, now: u64) -> Option<PastFailures> {
        self.get(kind, target, now)
            .filter(|f| f.count >= SHOWN_AFTER)
            .map(PastFailures::from)
    }

    /// Targets skipped at least [`SHOWN_AFTER`] times in a row, the most
    /// often failed first. A wait that has passed is no longer reported.
    pub fn list(&self, now: u64) -> Vec<Failure> {
        let mut v: Vec<Failure> = self
            .map
            .values()
            .filter(|f| {
                f.count >= SHOWN_AFTER && now.saturating_sub(f.last_unix) <= FORGET_AFTER_SECS
            })
            .cloned()
            .map(|mut f| {
                if f.retry_after_unix.is_some_and(|t| t <= now) {
                    f.retry_after_unix = None;
                }
                f
            })
            .collect();
        v.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(b.last_unix.cmp(&a.last_unix))
                .then(a.target.cmp(&b.target))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(at: u64, kind: &str, target: &str, status: &str, reason: Option<SkipReason>) -> Action {
        Action {
            target: Some(target.into()),
            status: Some(status.into()),
            reason,
            ..Action::note(at, kind, format!("{status} {target}"), 0, None, "optimizer")
        }
    }

    #[test]
    fn skips_in_a_row_are_counted_and_a_success_clears_them() {
        let mut m = Memory::default();
        let bf = Some(SkipReason::BackupFailed);
        assert_eq!(
            m.note(&act(100, "worktree", "/w/a", "skipped", bf))
                .unwrap()
                .count,
            1
        );
        assert!(m.list(100).is_empty(), "one skip is not a pattern");
        let f = m
            .note(&act(
                200,
                "worktree",
                "/w/a",
                "skipped",
                Some(SkipReason::Busy),
            ))
            .unwrap();
        assert_eq!(
            (f.count, f.reason, f.first_unix, f.last_unix),
            (2, Some(SkipReason::Busy), 100, 200)
        );
        assert_eq!(f.retry_after_unix, None, "no wait before the third");
        let l = m.list(200);
        assert_eq!(l.len(), 1);
        assert!(l[0].advice.contains("still using it"), "{}", l[0].advice);
        // Another target and another kind are counted apart.
        m.note(&act(210, "reclaim", "/w/a", "skipped", None));
        assert_eq!(m.get("worktree", "/w/a", 210).unwrap().count, 2);
        assert_eq!(m.get("reclaim", "/w/a", 210).unwrap().count, 1);
        // A success forgets the run.
        assert!(m
            .note(&act(300, "worktree", "/w/a", "done", None))
            .is_none());
        assert!(m.get("worktree", "/w/a", 300).is_none());
        assert!(m.list(300).is_empty());
    }

    #[test]
    fn automation_backs_off_after_three_and_longer_each_time() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(2), 0);
        assert_eq!(backoff_secs(3), 6 * 3600);
        assert_eq!(backoff_secs(4), 12 * 3600);
        assert_eq!(backoff_secs(5), 24 * 3600);
        assert_eq!(backoff_secs(8), 7 * 86_400);
        assert_eq!(backoff_secs(u32::MAX), 7 * 86_400);

        let mut m = Memory::default();
        for t in [10, 20] {
            m.note(&act(
                t,
                "worktree",
                "/w/a",
                "skipped",
                Some(SkipReason::RemoveFailed),
            ));
            assert!(m.backing_off("worktree", "/w/a", t).is_none());
        }
        let f = m
            .note(&act(
                30,
                "worktree",
                "/w/a",
                "skipped",
                Some(SkipReason::RemoveFailed),
            ))
            .unwrap();
        assert_eq!(f.retry_after_unix, Some(30 + 6 * 3600));
        assert!(m.backing_off("worktree", "/w/a", 31).is_some());
        assert!(
            m.backing_off("worktree", "/w/a", 30 + 6 * 3600).is_none(),
            "the wait ends"
        );
        assert!(m.backing_off("reclaim", "/w/a", 31).is_none());
        // Once the wait has passed the list no longer names a retry time.
        assert_eq!(m.list(30 + 6 * 3600)[0].retry_after_unix, None);
        assert_eq!(m.list(31)[0].retry_after_unix, Some(30 + 6 * 3600));
        // A plan is told about it.
        let p = m.past("worktree", "/w/a", 31).unwrap();
        assert_eq!((p.count, p.reason), (3, Some(SkipReason::RemoveFailed)));
    }

    #[test]
    fn other_entries_are_ignored_and_old_records_forgotten() {
        let mut m = Memory::default();
        // Process actions, notes without a status, and findings do not count.
        assert!(m
            .note(&act(
                1,
                "stop",
                "pid 42",
                "skipped",
                Some(SkipReason::ProcessGone)
            ))
            .is_none());
        assert!(m
            .note(&Action::note(1, "reclaim", "x".into(), 0, None, "api"))
            .is_none());
        assert!(m.note(&act(1, "finding", "x", "skipped", None)).is_none());
        assert!(m.list(1).is_empty());

        m.note(&act(
            1_000,
            "reclaim",
            "npm",
            "skipped",
            Some(SkipReason::Busy),
        ));
        m.note(&act(
            2_000,
            "reclaim",
            "npm",
            "skipped",
            Some(SkipReason::Busy),
        ));
        assert_eq!(m.list(2_000).len(), 1);
        let later = 2_000 + FORGET_AFTER_SECS + 1;
        assert!(m.list(later).is_empty());
        assert!(m.get("reclaim", "npm", later).is_none());
        // A failure after the gap starts at one again.
        let f = m
            .note(&act(
                later,
                "reclaim",
                "npm",
                "skipped",
                Some(SkipReason::Busy),
            ))
            .unwrap();
        assert_eq!((f.count, f.first_unix), (1, later));
    }

    #[test]
    fn the_memory_is_rebuilt_from_the_journal_and_bounded() {
        let entries: Vec<Action> = (0..4)
            .map(|i| {
                act(
                    100 + i,
                    "worktree",
                    "/w/a",
                    "skipped",
                    Some(SkipReason::BackupFailed),
                )
            })
            .chain([
                act(200, "worktree", "/w/b", "skipped", None),
                act(201, "worktree", "/w/b", "done", None),
            ])
            .collect();
        let m = Memory::from_actions(&entries);
        assert_eq!(m.get("worktree", "/w/a", 300).unwrap().count, 4);
        assert!(m.get("worktree", "/w/b", 300).is_none());

        let mut m = Memory::default();
        for i in 0..(MAX_RECORDS as u64 + 50) {
            m.note(&act(i, "reclaim", &format!("/c/{i}"), "skipped", None));
        }
        assert_eq!(m.map.len(), MAX_RECORDS);
        assert!(
            m.get("reclaim", "/c/0", 600).is_none(),
            "the oldest went first"
        );
        assert!(m
            .get("reclaim", &format!("/c/{}", MAX_RECORDS + 49), 600)
            .is_some());
    }

    #[test]
    fn every_reason_has_advice() {
        for r in [
            SkipReason::BackupFailed,
            SkipReason::Busy,
            SkipReason::OutsideScanRoots,
            SkipReason::ReopenNotQuit,
        ] {
            assert!(!advice(Some(r)).is_empty());
        }
        assert!(!advice(None).is_empty());
    }
}
