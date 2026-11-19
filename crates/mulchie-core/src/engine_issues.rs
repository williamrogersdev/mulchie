//! Issues through the engine ([`crate::issues`]): the book is read from disk
//! on first use, fed with the stored findings whenever they change, and
//! saved by the engine that holds the lock.

use super::*;
use crate::issues::{Issue, IssueBook, IssueCounts, IssueStatus};
use crate::webhook::WebhookEvent;

/// `GET /v1/issues`.
#[derive(Debug, Clone, Serialize)]
pub struct IssueList {
    pub issues: Vec<Issue>,
    pub counts: IssueCounts,
}

impl Engine {
    /// This engine holds the engine lock now: only it saves the book. A
    /// one-off engine (the CLI's local fallback) and one that lost the race
    /// for the lock keep what they see in memory.
    fn keeps_issues(&self) -> bool {
        self.automation_enabled.load(Ordering::SeqCst)
            && lock::running_engine(&self.paths).is_some_and(|h| h.pid == std::process::id())
    }

    /// Run `f` on the book, reading it from disk the first time.
    fn with_issues<R>(&self, f: impl FnOnce(&mut IssueBook) -> R) -> R {
        let mut g = lock(&self.issues);
        let book = g.get_or_insert_with(|| IssueBook::load(&self.paths));
        f(book)
    }

    /// Fold the stored findings into the issue book, save it when this
    /// engine holds the lock, and journal and notify about issues that came
    /// back after the user resolved them.
    pub(crate) fn observe_issues(&self, now: u64) {
        let findings: Vec<Finding> = lock(&self.state).findings.clone();
        let holder = self.keeps_issues();
        let seen = self.with_issues(|book| {
            let r = book.observe_changes(&findings, now);
            if holder {
                // A book that cannot be saved costs only history.
                let _ = book.save_if_needed(&self.paths, now);
            }
            r
        });
        if !holder {
            return;
        }
        let regressed = seen.regressed;
        self.webhook_issues(WebhookEvent::NewIssue, &seen.new, &findings);
        self.webhook_issues(WebhookEvent::Regressed, &regressed, &findings);
        for i in &regressed {
            let text = format!(
                "Came back after it was resolved ({} time(s) in all): {}",
                i.occurrences, i.title
            );
            if self.notify.load(Ordering::SeqCst) {
                crate::notify::notify("Mulchie", &text);
            }
            self.log(
                "issue",
                text,
                0,
                i.workspace.as_ref().map(|w| w.display().to_string()),
                "watch",
            );
        }
    }

    /// Issues with `status` (unresolved when `None`; every one with `all`),
    /// of `kind`.
    pub fn issues(
        &self,
        status: Option<IssueStatus>,
        all: bool,
        kind: Option<watch::FindingKind>,
    ) -> IssueList {
        self.with_issues(|b| IssueList {
            issues: b.list(status, all, kind),
            counts: b.counts(),
        })
    }

    /// Counts, and up to five unresolved issues that came back most often
    /// (regressed first), for the state and `diagnose`.
    pub(crate) fn issue_summary(&self) -> (IssueCounts, Vec<Issue>) {
        self.with_issues(|b| {
            let mut top = b.list(None, false, None);
            top.sort_by(|a, b| {
                (b.status == IssueStatus::Regressed)
                    .cmp(&(a.status == IssueStatus::Regressed))
                    .then(b.occurrences.cmp(&a.occurrences))
                    .then(b.last_seen_unix.cmp(&a.last_seen_unix))
            });
            top.truncate(5);
            (b.counts(), top)
        })
    }

    pub fn issue(&self, id: &str) -> Option<Issue> {
        self.with_issues(|b| b.get(id).cloned())
    }

    /// Mark issue `id` resolved, ignored or open again. Only the engine that
    /// holds the lock keeps the book, so another one refuses.
    pub fn set_issue_status(
        &self,
        id: &str,
        status: IssueStatus,
    ) -> Result<Issue, (&'static str, String)> {
        if status == IssueStatus::Regressed {
            return Err((
                "invalid_body",
                "an issue regresses by itself; set resolved, ignored or open".into(),
            ));
        }
        if !self.keeps_issues() {
            return Err((
                "not_lock_holder",
                "another Mulchie engine holds the engine lock and keeps the issues; send the change to it".into(),
            ));
        }
        let now = now_unix();
        let (r, before) = self.with_issues(|b| {
            let before = b.get(id).map(|i| i.status);
            let r = b.set_status(id, status, now);
            if r.is_some() {
                let _ = b.save_if_needed(&self.paths, now);
            }
            (r, before)
        });
        let issue = r.ok_or(("not_found", format!("no issue {id:?}")))?;
        if status == IssueStatus::Resolved && before != Some(IssueStatus::Resolved) {
            let findings: Vec<Finding> = lock(&self.state).findings.clone();
            self.webhook_issues(
                WebhookEvent::ResolvedByUser,
                std::slice::from_ref(&issue),
                &findings,
            );
        }
        let word = serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        self.log(
            "issue",
            format!("Marked {word}: {}", issue.title),
            0,
            issue.workspace.as_ref().map(|w| w.display().to_string()),
            "api",
        );
        Ok(issue)
    }
}
