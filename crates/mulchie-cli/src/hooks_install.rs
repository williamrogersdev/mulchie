//! `mulchie hooks install | uninstall | status`: add `mulchie hook` to an
//! agent app's user-level hook configuration.
//!
//! The same care as `mcp install`: a file that does not parse is refused, a
//! symbolic link is followed (a dangling one is refused), the old file is
//! copied to `<file>.mulchie-backup-<unix>`, and the new one is written
//! through a temp file and a rename with the old mode, after checking the
//! app did not save the file meanwhile. Other keys and other hooks keep
//! their content and order.
//!
//! Mulchie's entries are recognised by their command: an executable named
//! `mulchie`, then `hook <client>`. Install adds or updates exactly those,
//! and uninstall removes exactly those, so a user's own hook on the same
//! event is never touched.

use crate::cli::{GuardMode, HookClient};
use crate::mcp_install::{now_unix, read_text, resolve, write_checked};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// One event Mulchie registers for, in a client's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSpec {
    /// The client's event name.
    pub native: &'static str,
    /// The client-neutral name passed to `mulchie hook`.
    pub neutral: &'static str,
    /// Only for the client's shell tool, where the client filters by tool.
    pub matcher: Option<&'static str>,
    /// In the client's unit (seconds, or milliseconds for Gemini).
    pub timeout: u64,
}

const fn ev(
    native: &'static str,
    neutral: &'static str,
    matcher: Option<&'static str>,
    timeout: u64,
) -> EventSpec {
    EventSpec {
        native,
        neutral,
        matcher,
        timeout,
    }
}

/// The events each client documents for session start, prompts, before and
/// after a shell command, stop and session end.
pub fn events_for(c: HookClient) -> &'static [EventSpec] {
    const CLAUDE: [EventSpec; 6] = [
        ev("SessionStart", "session_start", None, 5),
        ev("UserPromptSubmit", "prompt", None, 5),
        ev("PreToolUse", "pre_tool", Some("Bash"), 5),
        ev("PostToolUse", "post_tool", Some("Bash"), 5),
        ev("Stop", "stop", None, 5),
        ev("SessionEnd", "session_end", None, 5),
    ];
    // Codex allows SessionEnd at most 3 s.
    const CODEX: [EventSpec; 6] = [
        ev("SessionStart", "session_start", None, 5),
        ev("UserPromptSubmit", "prompt", None, 5),
        ev("PreToolUse", "pre_tool", Some("Bash"), 5),
        ev("PostToolUse", "post_tool", Some("Bash"), 5),
        ev("Stop", "stop", None, 5),
        ev("SessionEnd", "session_end", None, 2),
    ];
    const CURSOR: [EventSpec; 6] = [
        ev("sessionStart", "session_start", None, 5),
        ev("beforeSubmitPrompt", "prompt", None, 5),
        ev("beforeShellExecution", "pre_tool", None, 5),
        ev("afterShellExecution", "post_tool", None, 5),
        ev("stop", "stop", None, 5),
        ev("sessionEnd", "session_end", None, 5),
    ];
    const GEMINI: [EventSpec; 6] = [
        ev("SessionStart", "session_start", None, 5000),
        ev("BeforeAgent", "prompt", None, 5000),
        ev("BeforeTool", "pre_tool", Some("run_shell_command"), 5000),
        ev("AfterTool", "post_tool", Some("run_shell_command"), 5000),
        ev("AfterAgent", "stop", None, 5000),
        ev("SessionEnd", "session_end", None, 5000),
    ];
    const COPILOT: [EventSpec; 6] = [
        ev("sessionStart", "session_start", None, 5),
        ev("userPromptSubmitted", "prompt", None, 5),
        ev("preToolUse", "pre_tool", None, 5),
        ev("postToolUse", "post_tool", None, 5),
        ev("agentStop", "stop", None, 5),
        ev("sessionEnd", "session_end", None, 5),
    ];
    match c {
        HookClient::ClaudeCode => &CLAUDE,
        HookClient::Codex => &CODEX,
        HookClient::Cursor => &CURSOR,
        HookClient::Gemini => &GEMINI,
        HookClient::Copilot => &COPILOT,
    }
}

pub const ALL: [HookClient; 5] = [
    HookClient::ClaudeCode,
    HookClient::Codex,
    HookClient::Cursor,
    HookClient::Gemini,
    HookClient::Copilot,
];

/// Event, then matcher groups, then handlers; or event, then handlers.
fn grouped(c: HookClient) -> bool {
    matches!(
        c,
        HookClient::ClaudeCode | HookClient::Codex | HookClient::Gemini
    )
}

/// Files with a top-level `"version": 1`.
fn versioned(c: HookClient) -> bool {
    matches!(c, HookClient::Cursor | HookClient::Copilot)
}

/// Where things are, injected so tests never see the real home.
pub struct HookEnv {
    pub home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`
    pub claude_config: Option<PathBuf>,
    /// `CODEX_HOME`
    pub codex_home: Option<PathBuf>,
    /// `COPILOT_HOME`
    pub copilot_home: Option<PathBuf>,
    /// The absolute path of the `mulchie` binary to run.
    pub exe: PathBuf,
}

impl HookEnv {
    fn from_process() -> Result<Self, String> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        Ok(Self {
            home: var("HOME").ok_or("HOME is not set")?,
            claude_config: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            copilot_home: var("COPILOT_HOME"),
            exe: crate::service::service_exe()?,
        })
    }

    pub fn file(&self, c: HookClient) -> PathBuf {
        let under =
            |v: &Option<PathBuf>, dflt: &str| v.clone().unwrap_or_else(|| self.home.join(dflt));
        match c {
            HookClient::ClaudeCode => under(&self.claude_config, ".claude").join("settings.json"),
            HookClient::Codex => under(&self.codex_home, ".codex").join("hooks.json"),
            HookClient::Cursor => self.home.join(".cursor/hooks.json"),
            HookClient::Gemini => self.home.join(".gemini/settings.json"),
            HookClient::Copilot => under(&self.copilot_home, ".copilot").join("hooks/mulchie.json"),
        }
    }
}

/// `s` quoted for a POSIX shell when it needs it.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Words of a shell command line: quotes and backslashes, nothing else.
pub fn sh_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                in_word = true;
                for d in it.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    cur.push(d);
                }
            }
            '"' => {
                in_word = true;
                while let Some(d) = it.next() {
                    match d {
                        '"' => break,
                        '\\' => {
                            if let Some(e) = it.next() {
                                cur.push(e);
                            }
                        }
                        _ => cur.push(d),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(e) = it.next() {
                    cur.push(e);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// The command line written into the client's file.
pub fn hook_command(
    exe: &Path,
    c: HookClient,
    spec: &EventSpec,
    guard: Option<GuardMode>,
) -> String {
    command_for(exe, c, spec.neutral, guard)
}

/// The command for a client-neutral event name.
fn command_for(exe: &Path, c: HookClient, neutral: &str, guard: Option<GuardMode>) -> String {
    let mut s = format!(
        "{} hook {} {}",
        sh_quote(&exe.to_string_lossy()),
        c.name(),
        neutral
    );
    if let (Some(g), "pre_tool") = (guard, neutral) {
        s.push_str(&format!(" --guard {}", g.as_str()));
    }
    s
}

/// The executable of `cmd` when `cmd` is exactly a command Mulchie writes for
/// client `c` (any event it has registered for, any guard), and nothing else:
/// a user's own command that starts the same way is not ours.
pub fn ours_exe(cmd: &str, c: HookClient) -> Option<String> {
    let guards = [
        None,
        Some(GuardMode::Off),
        Some(GuardMode::Warn),
        Some(GuardMode::Block),
    ];
    for w in sh_words(cmd) {
        if Path::new(&w).file_name().and_then(|n| n.to_str()) != Some("mulchie") {
            continue;
        }
        let exe = Path::new(&w);
        for ev in crate::hook_wire::EVENTS {
            for g in guards {
                if forms(exe, c, ev, g).iter().any(|f| f == cmd) {
                    return Some(w);
                }
            }
        }
    }
    None
}

/// Every command text Mulchie has written for one event.
fn forms(exe: &Path, c: HookClient, neutral: &str, guard: Option<GuardMode>) -> Vec<String> {
    vec![command_for(exe, c, neutral, guard)]
}

fn command_of(handler: &Value) -> Option<&str> {
    handler
        .get("command")
        .or_else(|| handler.get("bash"))
        .and_then(Value::as_str)
}

/// Whether `handler` is one Mulchie added for client `c`.
pub fn is_ours(handler: &Value, c: HookClient) -> bool {
    command_of(handler).is_some_and(|cmd| ours_exe(cmd, c).is_some())
}

/// The fields of a handler Mulchie owns: its command and its timeout. On an
/// update only these change; a handler's other fields and its group's
/// matcher stay as they are.
const OWNED: [&str; 4] = ["command", "bash", "timeout", "timeoutSec"];

fn owned_differ(have: &Value, want: &Value) -> bool {
    OWNED.iter().any(|k| have.get(*k) != want.get(*k))
}

fn set_owned(have: &mut Value, want: &Value) {
    if let (Some(h), Some(w)) = (have.as_object_mut(), want.as_object()) {
        for k in OWNED {
            match w.get(k) {
                Some(v) => {
                    h.insert(k.into(), v.clone());
                }
                None => {
                    h.shift_remove(k);
                }
            }
        }
    }
}

/// The handler object for one event.
pub fn handler(exe: &Path, c: HookClient, spec: &EventSpec, guard: Option<GuardMode>) -> Value {
    let cmd = hook_command(exe, c, spec, guard);
    match c {
        HookClient::ClaudeCode | HookClient::Codex => {
            json!({ "type": "command", "command": cmd, "timeout": spec.timeout })
        }
        HookClient::Gemini => {
            json!({ "name": "mulchie", "type": "command", "command": cmd, "timeout": spec.timeout })
        }
        HookClient::Cursor => json!({ "command": cmd, "timeout": spec.timeout }),
        HookClient::Copilot => {
            json!({ "type": "command", "bash": cmd, "timeoutSec": spec.timeout })
        }
    }
}

/// What goes into the event's array: a matcher group or the handler.
fn entry(c: HookClient, spec: &EventSpec, h: Value) -> Value {
    if !grouped(c) {
        return h;
    }
    match spec.matcher {
        Some(m) => json!({ "matcher": m, "hooks": [h] }),
        None => json!({ "hooks": [h] }),
    }
}

/// Positions of Mulchie's handlers in one event's array:
/// (entry index, handler index within its group, if grouped).
fn find_ours(arr: &[Value], c: HookClient) -> Vec<(usize, Option<usize>)> {
    let mut out = Vec::new();
    for (i, e) in arr.iter().enumerate() {
        if grouped(c) {
            for (j, h) in e["hooks"].as_array().into_iter().flatten().enumerate() {
                if is_ours(h, c) {
                    out.push((i, Some(j)));
                }
            }
        } else if is_ours(e, c) {
            out.push((i, None));
        }
    }
    out
}

/// Remove Mulchie's handlers from one event's array. Groups and the array
/// itself are dropped only when that removal emptied them. Returns how many
/// were removed.
fn remove_ours(arr: &mut Vec<Value>, c: HookClient) -> usize {
    let mut removed = 0;
    let mut i = 0;
    while i < arr.len() {
        if grouped(c) {
            let mut emptied = false;
            if let Some(hs) = arr[i].get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !is_ours(h, c));
                removed += before - hs.len();
                emptied = before > 0 && hs.is_empty();
            }
            if emptied {
                arr.remove(i);
                continue;
            }
        } else if is_ours(&arr[i], c) {
            arr.remove(i);
            removed += 1;
            continue;
        }
        i += 1;
    }
    removed
}

/// The result of planning an edit.
#[derive(Debug, PartialEq)]
pub enum Change {
    Same(String),
    Write { text: String, what: Vec<String> },
}

fn parse_root(old: Option<&str>, path: &Path, c: HookClient) -> Result<Value, String> {
    let root: Value = match old {
        None => json!({}),
        Some(s) if s.trim().is_empty() => json!({}),
        Some(s) => serde_json::from_str(s).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); fix it first. It was left as it is.",
                path.display()
            )
        })?,
    };
    let Value::Object(mut obj) = root else {
        return Err(format!(
            "{} is not a JSON object; it was left as it is",
            path.display()
        ));
    };
    if versioned(c) && !obj.contains_key("version") {
        // Put "version" first, as the clients' examples do.
        let mut o = Map::new();
        o.insert("version".into(), json!(1));
        o.extend(obj);
        obj = o;
    }
    match obj.get("hooks") {
        None | Some(Value::Object(_)) => {}
        Some(_) => {
            return Err(format!(
                "`hooks` in {} is not an object; it was left as it is",
                path.display()
            ))
        }
    }
    Ok(Value::Object(obj))
}

fn text_of(root: &Value) -> Result<String, String> {
    let mut t = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    t.push('\n');
    Ok(t)
}

/// Plan an install into the file's text `old`.
pub fn plan_install(
    old: Option<&str>,
    path: &Path,
    c: HookClient,
    exe: &Path,
    guard: Option<GuardMode>,
) -> Result<Change, String> {
    let mut root = parse_root(old, path, c)?;
    let hooks = root
        .as_object_mut()
        .expect("an object")
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("checked");
    let mut what = Vec::new();
    for spec in events_for(c) {
        let want_h = handler(exe, c, spec, guard);
        let want = entry(c, spec, want_h.clone());
        let arr = hooks.entry(spec.native).or_insert_with(|| json!([]));
        let Some(arr) = arr.as_array_mut() else {
            return Err(format!(
                "hooks.{} in {} is not a list; it was left as it is",
                spec.native,
                path.display()
            ));
        };
        let found = find_ours(arr, c);
        match found.first() {
            None => {
                arr.push(want);
                what.push(format!("add {}: {}", spec.native, want_h));
            }
            Some(&(i, j)) => {
                let have = match j {
                    Some(j) => &mut arr[i]["hooks"][j],
                    None => &mut arr[i],
                };
                if owned_differ(have, &want_h) {
                    set_owned(have, &want_h);
                    what.push(format!("update {}: {}", spec.native, want_h));
                }
                // A second copy of Mulchie's hook on one event is removed.
                if found.len() > 1 {
                    let keep = found[0];
                    let mut seen = 0;
                    let mut idx = 0;
                    arr.retain_mut(|e| {
                        let this = idx;
                        idx += 1;
                        if grouped(c) {
                            if let Some(hs) = e.get_mut("hooks").and_then(Value::as_array_mut) {
                                let before = hs.len();
                                let mut k = 0;
                                hs.retain(|h| {
                                    let pos = k;
                                    k += 1;
                                    let ours = is_ours(h, c);
                                    let drop = ours && (this, Some(pos)) != keep;
                                    if drop {
                                        seen += 1;
                                    }
                                    !drop
                                });
                                // A group goes only when this emptied it.
                                return !(hs.is_empty() && before > 0);
                            }
                            true
                        } else {
                            let drop = is_ours(e, c) && (this, None) != keep;
                            if drop {
                                seen += 1;
                            }
                            !drop
                        }
                    });
                    what.push(format!("remove {seen} duplicate {} hook(s)", spec.native));
                }
            }
        }
    }
    // Events Mulchie no longer registers for (from an older install).
    let known: Vec<&str> = events_for(c).iter().map(|s| s.native).collect();
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for k in keys {
        if known.contains(&k.as_str()) {
            continue;
        }
        if let Some(arr) = hooks.get_mut(&k).and_then(Value::as_array_mut) {
            if remove_ours(arr, c) > 0 {
                what.push(format!("remove {k}"));
                if arr.is_empty() {
                    hooks.shift_remove(&k);
                }
            }
        }
    }
    if what.is_empty() {
        return Ok(Change::Same("already set up".into()));
    }
    Ok(Change::Write {
        text: text_of(&root)?,
        what,
    })
}

/// Plan removing exactly Mulchie's handlers from the file's text `old`.
pub fn plan_uninstall(old: Option<&str>, path: &Path, c: HookClient) -> Result<Change, String> {
    if old.is_none() {
        return Ok(Change::Same("no file, nothing to remove".into()));
    }
    let mut root = parse_root(old, path, c)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(Change::Same("not set up".into()));
    };
    let mut what = Vec::new();
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for k in keys {
        if let Some(arr) = hooks.get_mut(&k).and_then(Value::as_array_mut) {
            let n = remove_ours(arr, c);
            if n > 0 {
                what.push(format!("remove {k} ({n} hook(s))"));
                if arr.is_empty() {
                    hooks.shift_remove(&k);
                }
            }
        }
    }
    if what.is_empty() {
        return Ok(Change::Same("not set up".into()));
    }
    Ok(Change::Write {
        text: text_of(&root)?,
        what,
    })
}

/// A line diff of `old` and `new`, with two lines of context.
pub fn diff(old: &str, new: &str, path: &Path) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = format!("--- {}\n+++ {}\n", path.display(), path.display());
    if a.len() * b.len() > 4_000_000 {
        for l in &a {
            out.push_str(&format!("-{l}\n"));
        }
        for l in &b {
            out.push_str(&format!("+{l}\n"));
        }
        return out;
    }
    // Longest common subsequence, from the end.
    let (n, m) = (a.len(), b.len());
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || t[i + 1][j] >= t[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    const CTX: usize = 2;
    let near: Vec<bool> = (0..ops.len())
        .map(|k| {
            let lo = k.saturating_sub(CTX);
            let hi = (k + CTX + 1).min(ops.len());
            ops[lo..hi].iter().any(|(c, _)| *c != ' ')
        })
        .collect();
    let mut skipped = false;
    let mut printed = false;
    for (k, (c, l)) in ops.iter().enumerate() {
        if near[k] {
            if skipped && printed {
                out.push_str("@@\n");
            }
            skipped = false;
            printed = true;
            out.push_str(&format!("{c}{l}\n"));
        } else {
            skipped = true;
        }
    }
    out
}

fn after_note(c: HookClient) -> &'static str {
    match c {
        HookClient::ClaudeCode => "Claude Code reloads settings.json on its own; `/hooks` in Claude Code lists them. Sessions already running may need a restart.",
        HookClient::Codex => "Codex runs new or changed hooks only after you review and trust them: run `/hooks` in Codex (or Settings > Hooks in the app) and trust the mulchie hooks.",
        HookClient::Cursor => "If Cursor does not pick up the hooks, restart it.",
        HookClient::Gemini => "Restart Gemini CLI so it loads the hooks.",
        HookClient::Copilot => "New Copilot CLI sessions load the hooks.",
    }
}

/// Back up and write, or show the diff.
fn apply(
    shown: &Path,
    target: &Path,
    old: Option<&str>,
    change: Change,
    dry_run: bool,
) -> Result<(String, bool), String> {
    let (text, what) = match change {
        Change::Same(s) => {
            return Ok((
                format!("{}: {s}, nothing to change", shown.display()),
                false,
            ))
        }
        Change::Write { text, what } => (text, what),
    };
    let via = if shown != target {
        format!(" (a link to {})", target.display())
    } else {
        String::new()
    };
    if dry_run {
        return Ok((
            format!(
                "Would change {}{via}:\n{}Nothing was changed (--dry-run).",
                shown.display(),
                diff(old.unwrap_or(""), &text, shown)
            ),
            false,
        ));
    }
    let mut mode = 0o600;
    let mut note = String::new();
    if old.is_some() {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(target) {
            mode = m.permissions().mode() & 0o7777;
        }
        let rand = mulchie_core::paths::random_hex(4).map_err(|e| e.to_string())?;
        let backup = PathBuf::from(format!(
            "{}.mulchie-backup-{}-{rand}",
            target.display(),
            now_unix()
        ));
        std::fs::copy(target, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", target.display()))?;
        note = format!("\nOld file saved as {}", backup.display());
    } else if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    write_checked(target, &text, mode, old)?;
    Ok((
        format!("{}{via}:\n  {}{note}", shown.display(), what.join("\n  ")),
        true,
    ))
}

pub fn install_in(
    env: &HookEnv,
    c: HookClient,
    dry_run: bool,
    guard: Option<GuardMode>,
) -> Result<String, String> {
    let path = env.file(c);
    let target = resolve(&path)?;
    let old = read_text(&target)?;
    let change = plan_install(old.as_deref(), &path, c, &env.exe, guard)?;
    let (msg, wrote) = apply(&path, &target, old.as_deref(), change, dry_run)?;
    Ok(if wrote {
        format!("{msg}\n{}", after_note(c))
    } else {
        msg
    })
}

pub fn uninstall_in(env: &HookEnv, c: HookClient, dry_run: bool) -> Result<String, String> {
    let path = env.file(c);
    let target = resolve(&path)?;
    let old = read_text(&target)?;
    let change = plan_uninstall(old.as_deref(), &path, c)?;
    Ok(apply(&path, &target, old.as_deref(), change, dry_run)?.0)
}

/// Per client: the file, Mulchie's events in it, and the binary they run.
pub fn status_in(env: &HookEnv) -> Value {
    let mut out = Vec::new();
    for c in ALL {
        let path = env.file(c);
        let mut v = json!({ "client": c.name(), "file": path, "exists": path.exists() });
        let text = resolve(&path).and_then(|t| read_text(&t));
        match text {
            Err(e) => v["error"] = json!(e),
            Ok(None) => v["events"] = json!([]),
            Ok(Some(s)) => match serde_json::from_str::<Value>(&s) {
                Err(e) => v["error"] = json!(format!("not valid JSON: {e}")),
                Ok(root) => {
                    let mut events = Vec::new();
                    let mut others = 0;
                    let mut exes = Vec::new();
                    for (k, arr) in root["hooks"].as_object().into_iter().flatten() {
                        let arr = arr.as_array().cloned().unwrap_or_default();
                        let ours = find_ours(&arr, c);
                        let total: usize = arr
                            .iter()
                            .map(|e| {
                                if grouped(c) {
                                    e["hooks"].as_array().map_or(0, Vec::len)
                                } else {
                                    1
                                }
                            })
                            .sum();
                        if !ours.is_empty() {
                            events.push(k.clone());
                            others += total - ours.len();
                            for (i, j) in ours {
                                let h = match j {
                                    Some(j) => &arr[i]["hooks"][j],
                                    None => &arr[i],
                                };
                                if let Some(e) = command_of(h).and_then(|cmd| ours_exe(cmd, c)) {
                                    exes.push(e);
                                }
                            }
                        }
                    }
                    exes.sort();
                    exes.dedup();
                    let current = env.exe.to_string_lossy().into_owned();
                    v["events"] = json!(events);
                    v["other_hooks_on_these_events"] = json!(others);
                    v["runs"] = json!(exes);
                    v["runs_this_binary"] =
                        json!(!exes.is_empty() && exes.iter().all(|e| *e == current));
                }
            },
        }
        out.push(v);
    }
    json!(out)
}

fn render_status(v: &Value) -> String {
    let mut s = String::new();
    for c in v.as_array().into_iter().flatten() {
        let name = c["client"].as_str().unwrap_or("");
        let file = c["file"].as_str().unwrap_or("");
        let line = if let Some(e) = c["error"].as_str() {
            format!("{name:<12} {file}: {e}")
        } else {
            let evs: Vec<&str> = c["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if evs.is_empty() {
                format!("{name:<12} not installed ({file})")
            } else {
                let stale = if c["runs_this_binary"] == false {
                    format!("; runs {} (re-run install to use this binary)", c["runs"])
                } else {
                    String::new()
                };
                format!("{name:<12} {} in {file}{stale}", evs.join(", "))
            }
        };
        s.push_str(&line);
        s.push('\n');
    }
    s
}

fn report(r: Result<String, String>) -> i32 {
    match r {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("mulchie: {e}");
            1
        }
    }
}

pub fn install(c: HookClient, dry_run: bool, guard: Option<GuardMode>) -> i32 {
    report(HookEnv::from_process().and_then(|e| install_in(&e, c, dry_run, guard)))
}

pub fn uninstall(c: HookClient, dry_run: bool) -> i32 {
    report(HookEnv::from_process().and_then(|e| uninstall_in(&e, c, dry_run)))
}

pub fn status(json_out: bool) -> i32 {
    match HookEnv::from_process() {
        Ok(e) => {
            let v = status_in(&e);
            if json_out {
                crate::commands::print_json(&v);
            } else {
                print!("{}", render_status(&v));
            }
            0
        }
        Err(e) => report(Err(e)),
    }
}

#[cfg(test)]
#[path = "hooks_install_tests.rs"]
mod tests;
