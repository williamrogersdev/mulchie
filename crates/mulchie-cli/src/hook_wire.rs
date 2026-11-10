//! The request and reply of `POST /v1/hook`, kept in one place so they are
//! easy to adjust to the core.
//!
//! Assumed shape (the route is being added to the core):
//! - request: `{client, client_session_id, event, pid, cwd, tool?, command?, detail?}`
//!   where `event` is one of [`EVENTS`] and `pid` is the hook process's parent
//!   (the agent, or the shell it ran the hook through);
//! - reply: `{session_id, decision: allow|warn|block, reason?, rule?,
//!   context: [string], breaches: [...]}`.
//!
//! The request goes with the read-only token: a hook only reports and asks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The client-neutral events.
pub const EVENTS: [&str; 7] = [
    "session_start",
    "prompt",
    "pre_tool",
    "post_tool",
    "stop",
    "session_end",
    "other",
];

pub const ROUTE: &str = "/v1/hook";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HookRequest {
    pub client: String,
    pub client_session_id: Option<String>,
    pub event: String,
    pub pid: u32,
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow,
    Warn,
    Block,
}

/// The reply; `session_id` is not needed by the hook and is ignored.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HookReply {
    /// `allow`, `warn` or `block`; anything else is read as `allow`.
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub breaches: Vec<Value>,
}

impl HookReply {
    pub fn decision(&self) -> Decision {
        match self.decision.as_deref() {
            Some("block") => Decision::Block,
            Some("warn") => Decision::Warn,
            _ => Decision::Allow,
        }
    }
}
