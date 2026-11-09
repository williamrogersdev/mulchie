//! Shapes the engine's state for the UI, which polls every 3 s.
//!
//! The poll carries no action list (up to 500 entries) and no config. It
//! carries a revision for each instead, and the UI fetches them only when the
//! revision changes and a view needs them.

use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// A short, stable fingerprint of a JSON value.
pub fn rev(v: &Value) -> String {
    let mut h = DefaultHasher::new();
    v.to_string().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Fingerprint of the action list: its length and newest entry. New entries
/// go to the front, so any change moves one or the other.
pub fn actions_rev(actions: &Value) -> String {
    let list = actions.as_array().map(Vec::as_slice).unwrap_or(&[]);
    rev(&json!([list.len(), list.first()]))
}

const RESTORE_FAILED: &str = "Could not restore the priority";
const RESTORED: &str = "Restored the priority";

/// Whether build processes Mulchie reniced are stuck at the lower priority:
/// the newest priority-restore entry in the journal is a failure and some
/// processes still count as throttled.
pub fn restore_failed(actions: &Value, throttled: u64) -> bool {
    if throttled == 0 {
        return false;
    }
    let list = actions.as_array().map(Vec::as_slice).unwrap_or(&[]);
    list.iter()
        .filter(|a| a["kind"] == "throttle")
        .filter_map(|a| a["detail"].as_str())
        .find(|d| d.starts_with(RESTORE_FAILED) || d.starts_with(RESTORED))
        .is_some_and(|d| d.starts_with(RESTORE_FAILED))
}

/// The state without `actions` and `config`, with `actions_rev`,
/// `actions_count`, `config_rev` and `restore_failed` added.
pub fn slim_state(mut v: Value) -> Value {
    let Some(obj) = v.as_object_mut() else {
        return v;
    };
    let actions = obj.remove("actions").unwrap_or(Value::Null);
    let config = obj.remove("config").unwrap_or(Value::Null);
    let throttled = obj.get("throttled").and_then(Value::as_u64).unwrap_or(0);
    obj.insert("actions_rev".into(), json!(actions_rev(&actions)));
    obj.insert(
        "actions_count".into(),
        json!(actions.as_array().map(Vec::len).unwrap_or(0)),
    );
    obj.insert("config_rev".into(), json!(rev(&config)));
    obj.insert(
        "restore_failed".into(),
        json!(restore_failed(&actions, throttled)),
    );
    v
}

/// The config part of a state: `{config, config_error, rev}`.
pub fn config_view(state: &Value) -> Value {
    json!({
        "config": state["config"],
        "config_error": state["config_error"],
        "rev": rev(&state["config"]),
    })
}

/// The action list of a state, newest first, and its revision.
pub fn actions_view(state: &Value) -> Value {
    let actions = &state["actions"];
    json!({
        "actions": if actions.is_array() { actions.clone() } else { json!([]) },
        "rev": actions_rev(actions),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Value {
        json!({
            "version": "0.3.0",
            "config": {"low_disk_gb": 25},
            "config_error": null,
            "throttled": 2,
            "actions": [
                {"kind": "throttle", "detail": "Could not restore the priority of 2 build process(es); ...", "at_unix": 5},
                {"kind": "throttle", "detail": "Restored the priority of 1 build process(es)", "at_unix": 4}
            ],
            "snapshot": {"cpu_pct": 1.0}
        })
    }

    #[test]
    fn slim_drops_actions_and_config_and_adds_revisions() {
        let s = slim_state(state());
        assert!(s.get("actions").is_none());
        assert!(s.get("config").is_none());
        assert_eq!(s["actions_count"], 2);
        assert_eq!(s["restore_failed"], true);
        assert_eq!(s["config_rev"], config_view(&state())["rev"]);
        assert_eq!(s["actions_rev"], actions_view(&state())["rev"]);
        assert_eq!(s["snapshot"]["cpu_pct"], 1.0);
    }

    #[test]
    fn revisions_change_with_content() {
        let a = state();
        let mut b = state();
        b["config"]["low_disk_gb"] = json!(30);
        assert_ne!(rev(&a["config"]), rev(&b["config"]));
        let mut c = state();
        c["actions"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"kind": "reclaim", "detail": "x"}));
        assert_ne!(actions_rev(&a["actions"]), actions_rev(&c["actions"]));
    }

    #[test]
    fn restore_failed_needs_a_newer_failure_and_throttled_processes() {
        let s = state();
        assert!(restore_failed(&s["actions"], 2));
        assert!(!restore_failed(&s["actions"], 0));
        let ok = json!([
            {"kind": "throttle", "detail": "Restored the priority of 2 build process(es)"},
            {"kind": "throttle", "detail": "Could not restore the priority of 2 build process(es)"}
        ]);
        assert!(!restore_failed(&ok, 2));
        assert!(!restore_failed(&json!([]), 2));
    }
}
