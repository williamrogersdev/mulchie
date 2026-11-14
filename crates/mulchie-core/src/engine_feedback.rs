//! Feedback through the engine: a report gets the machine's state attached
//! and a line in the action journal, then goes to the feedback file.

use super::*;
use crate::feedback::{Entry, Machine, Report, Store};

/// `GET /v1/feedback`.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackList {
    /// Newest first.
    pub feedback: Vec<Entry>,
    /// Entries stored in all, of which `feedback` holds the newest.
    pub total: usize,
    pub path: PathBuf,
}

impl Engine {
    /// The machine as of the last sample, for a feedback entry.
    fn feedback_machine(&self) -> Option<Machine> {
        let (sessions, _) = self.session_counts();
        let st = lock(&self.state);
        let s = &st.snapshot;
        if s.at_unix == 0 {
            return None;
        }
        Some(Machine {
            health: serde_json::to_value(st.health)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            cpu_pct: s.cpu_pct,
            load1: s.load[0],
            cores: s.cpu_count,
            mem_free_pct: s.mem_free_pct(),
            disk_free_gb: s.disk_free_gb(),
            sessions,
        })
    }

    /// Store a report. `source` is how it arrived (`api`, `mcp`, `cli`); a
    /// report sent over the API may name its own source in `client`.
    pub fn feedback_add(
        &self,
        report: Report,
        source: &str,
    ) -> Result<Entry, (u16, &'static str, String)> {
        let entry = report
            .into_entry(source, self.feedback_machine(), now_unix())
            .map_err(|e| (400, "invalid_body", e.0))?;
        Store::new(&self.paths).append(&entry).map_err(|e| {
            (
                500,
                "feedback_not_saved",
                format!("could not save the feedback: {e}"),
            )
        })?;
        self.log(
            "feedback",
            format!(
                "Feedback {} ({}): {}",
                entry.id,
                serde_json::to_value(entry.category)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
                entry.summary.chars().take(120).collect::<String>()
            ),
            0,
            entry.cwd.clone(),
            entry.client.as_deref().unwrap_or(source),
        );
        Ok(entry)
    }

    /// The newest `limit` reports, newest first.
    pub fn feedback_list(&self, limit: usize) -> FeedbackList {
        let store = Store::new(&self.paths);
        let (feedback, total) = store.recent(limit);
        FeedbackList {
            feedback,
            total,
            path: store.path().to_path_buf(),
        }
    }
}
