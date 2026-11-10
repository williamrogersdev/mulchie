//! Finding the running engine, and saying plainly when there is none.

use mulchie_core::api::{ApiError, Client};
use mulchie_core::{lock, Paths};
use serde_json::Value;

pub const START_HINT: &str =
    "start one with `mulchie serve`, `mulchie service install`, or the Mulchie app";

/// The result of looking for this user's engine.
pub enum Conn {
    Engine(Client),
    /// Nobody holds the engine lock.
    NotRunning,
    /// An engine holds the lock but could not be used.
    Unreachable {
        pid: u32,
        reason: String,
    },
}

/// Connect to the engine that holds this user's lock, on the port it
/// announced. The core checks the listener and the token file before the
/// token is sent.
pub fn connect(paths: &Paths) -> Conn {
    connect_as(paths, false)
}

/// As [`connect`], with the read-only token when `read_only` is set: the
/// engine then refuses everything but reads, plans and TTL build slots.
pub fn connect_as(paths: &Paths, read_only: bool) -> Conn {
    let Some(holder) = lock::running_engine(paths) else {
        return Conn::NotRunning;
    };
    let Some(port) = holder.port else {
        return Conn::Unreachable {
            pid: holder.pid,
            reason: "it has no API listener yet (still starting, or its port was taken)".into(),
        };
    };
    let c = if read_only {
        let token = mulchie_core::api::read_only_token_path_in(paths);
        if !token.exists() {
            return Conn::Unreachable {
                pid: holder.pid,
                reason: format!(
                    "the engine has no read-only token yet ({}); restart the engine to create it",
                    token.display()
                ),
            };
        }
        Client::connect_read_only_in(paths, port)
    } else {
        Client::connect_in(paths, port)
    };
    match c {
        Ok(c) => Conn::Engine(c),
        Err(reason) => Conn::Unreachable {
            pid: holder.pid,
            reason,
        },
    }
}

impl Conn {
    /// Why there is no engine to use, in one clause.
    pub fn why_not(&self) -> String {
        match self {
            Conn::Engine(_) => String::new(),
            Conn::NotRunning => "no engine is running".into(),
            Conn::Unreachable { pid, reason } => {
                format!("the engine (pid {pid}) could not be reached: {reason}")
            }
        }
    }

    /// Say on stderr that this command is working locally instead.
    pub fn note_local(&self, what: &str) {
        match self {
            Conn::Engine(_) => {}
            Conn::NotRunning => eprintln!("mulchie: {}; {what} ({START_HINT})", self.why_not()),
            Conn::Unreachable { .. } => eprintln!(
                "mulchie: {}; {what} (`mulchie doctor` shows more)",
                self.why_not()
            ),
        }
    }

    /// For commands that cannot work without an engine.
    pub fn explain_required(&self, what: &str) -> String {
        match self {
            Conn::Engine(_) => String::new(),
            Conn::NotRunning => format!("{what} needs a running engine; {START_HINT}"),
            Conn::Unreachable { .. } => format!(
                "{what} needs the engine, but {}; run `mulchie doctor`",
                self.why_not()
            ),
        }
    }
}

/// A one-line message for an API error, with advice for the codes a person
/// can act on.
pub fn describe(e: &ApiError) -> String {
    let hint = match e.code.as_str() {
        "unreachable" => "the engine stopped answering; is it still running? (`mulchie doctor`)",
        "timeout" => "the engine is busy or stuck; try again, or check `mulchie doctor`",
        "unauthorized" => {
            "the engine refused the token; restart the engine so both use the same token file"
        }
        "bad_host" | "origin_refused" => "the request was refused as not local",
        "too_many_connections" => "the engine is handling many requests; try again shortly",
        "plan_required" | "items_required" | "expected_head_required" => {
            "this Mulchie CLI and the engine disagree; are they the same version?"
        }
        "updates_off" => {
            "update checks are off; turn them on with `mulchie config set updates.check on`"
        }
        "not_lock_holder" => {
            "this engine does not hold the engine lock (another one does), so it cannot save the config; `mulchie doctor` shows which engine runs"
        }
        "invalid_config" => "the engine kept the config as it was; nothing changed",
        "unknown_session" => "`mulchie sessions` lists the sessions the engine knows",
        "invalid_session_id" => "a session id looks like s-<start time>-<pid>, from `mulchie sessions`",
                "not_found" | "method_not_allowed" => {
            "the engine does not know this request; is it an older version?"
        }
        _ => "",
    };
    if hint.is_empty() {
        format!("{} [{}]", e.message, e.code)
    } else {
        format!("{} [{}]; {hint}", e.message, e.code)
    }
}

/// `GET`/`POST` with the core's per-route timeout.
pub fn call(c: &Client, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
    c.request(
        method,
        path,
        body,
        mulchie_core::api::timeout_for(method, path),
    )
}
