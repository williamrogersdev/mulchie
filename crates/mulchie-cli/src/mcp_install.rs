//! `mulchie mcp install | uninstall | config`: connect the MCP server to an
//! agent app in one step.
//!
//! Claude Code and Codex are set up through their own CLI when it is on
//! PATH. Claude Desktop, Cursor and Codex without its CLI get their
//! user-level config file edited: other entries are kept, a file that does
//! not parse is refused, a symbolic link is followed (a dangling one is
//! refused), the old file is copied to `<file>.mulchie-backup-<unix>`, and
//! the new one is written through a temp file and a rename with the old
//! mode, after checking the app did not save the file meanwhile. Output
//! names only the mulchie entry; other entries' env values are never shown.

use crate::cli::{McpClient, McpInstallArgs, McpUninstallArgs};
use crate::service::{service_exe, which};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const NAME: &str = "mulchie";

/// Finds a program on PATH.
pub type Which = Box<dyn Fn(&str) -> Option<PathBuf>>;

/// Where things are, injected so tests never see the real home.
pub struct Env {
    pub home: PathBuf,
    pub xdg_config: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`, where Claude Code keeps `.claude.json` when set.
    pub claude_config: Option<PathBuf>,
    pub exe: PathBuf,
    pub mac: bool,
    pub which: Which,
}

impl Env {
    fn from_process() -> Result<Self, String> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Ok(Self {
            home: var("HOME").ok_or("HOME is not set")?,
            xdg_config: var("XDG_CONFIG_HOME").filter(|p| p.is_absolute()),
            codex_home: var("CODEX_HOME").filter(|p| p.is_absolute()),
            claude_config: var("CLAUDE_CONFIG_DIR").filter(|p| p.is_absolute()),
            exe: service_exe()?,
            mac: cfg!(target_os = "macos"),
            which: Box::new(which),
        })
    }

    fn args(read_only: bool) -> Vec<String> {
        let mut a = vec!["mcp".to_string()];
        if read_only {
            a.push("--read-only".into());
        }
        a
    }

    fn exe(&self) -> String {
        self.exe.to_string_lossy().into_owned()
    }

    fn desktop_config(&self) -> PathBuf {
        if self.mac {
            self.home
                .join("Library/Application Support/Claude/claude_desktop_config.json")
        } else {
            self.xdg_config
                .clone()
                .unwrap_or_else(|| self.home.join(".config"))
                .join("Claude/claude_desktop_config.json")
        }
    }

    /// Claude Code's user config, read only to see an existing entry.
    fn claude_json(&self) -> PathBuf {
        self.claude_config
            .as_deref()
            .unwrap_or(&self.home)
            .join(".claude.json")
    }

    fn codex_dir(&self) -> PathBuf {
        self.codex_home
            .clone()
            .unwrap_or_else(|| self.home.join(".codex"))
    }
}

pub const ALL: [McpClient; 4] = [
    McpClient::ClaudeCode,
    McpClient::ClaudeDesktop,
    McpClient::Cursor,
    McpClient::Codex,
];

pub fn label(c: McpClient) -> &'static str {
    match c {
        McpClient::ClaudeCode => "claude-code",
        McpClient::ClaudeDesktop => "claude-desktop",
        McpClient::Cursor => "cursor",
        McpClient::Codex => "codex",
    }
}

/// How a client gets set up.
#[derive(Debug, PartialEq)]
pub enum Method {
    /// Run the client's own CLI.
    Cli { program: PathBuf },
    /// Edit `mcpServers` in a JSON file; `typed` adds `"type": "stdio"`.
    Json { path: PathBuf, typed: bool },
    /// Edit `[mcp_servers.mulchie]` in a TOML file.
    Toml { path: PathBuf },
    /// Cannot be done from here; the text says what to do instead.
    Manual(String),
}

pub fn method(c: McpClient, env: &Env) -> Method {
    match c {
        McpClient::ClaudeCode => match (env.which)("claude") {
            Some(program) => Method::Cli { program },
            None => Method::Manual(
                "the `claude` command is not on PATH; install Claude Code, or run `claude mcp add` yourself (see `mulchie mcp config`)".into(),
            ),
        },
        McpClient::Codex => match (env.which)("codex") {
            Some(program) => Method::Cli { program },
            None => Method::Toml {
                path: env.codex_dir().join("config.toml"),
            },
        },
        McpClient::ClaudeDesktop => Method::Json {
            path: env.desktop_config(),
            typed: false,
        },
        McpClient::Cursor => Method::Json {
            path: env.home.join(".cursor/mcp.json"),
            typed: true,
        },
    }
}

/// Why this client looks installed, if it does.
pub fn detected(c: McpClient, env: &Env) -> Option<String> {
    match c {
        McpClient::ClaudeCode => {
            (env.which)("claude").map(|p| format!("{} is on PATH", p.display()))
        }
        McpClient::Codex => (env.which)("codex")
            .map(|p| format!("{} is on PATH", p.display()))
            .or_else(|| {
                let d = env.codex_dir();
                d.is_dir().then(|| format!("{} exists", d.display()))
            }),
        McpClient::ClaudeDesktop => {
            let d = env.desktop_config();
            let dir = d.parent()?;
            dir.is_dir().then(|| format!("{} exists", dir.display()))
        }
        McpClient::Cursor => {
            let d = env.home.join(".cursor");
            d.is_dir().then(|| format!("{} exists", d.display()))
        }
    }
}

// ------------------------------------------------------------ entry and IO

fn entry(env: &Env, read_only: bool, typed: bool) -> Value {
    let mut e = Map::new();
    if typed {
        e.insert("type".into(), json!("stdio"));
    }
    e.insert("command".into(), json!(env.exe()));
    e.insert("args".into(), json!(Env::args(read_only)));
    Value::Object(e)
}

fn cli_args(c: McpClient, env: &Env, read_only: bool) -> Vec<String> {
    let mut a: Vec<String> = match c {
        McpClient::ClaudeCode => vec!["mcp", "add", "--scope", "user", NAME, "--"],
        _ => vec!["mcp", "add", NAME, "--"],
    }
    .into_iter()
    .map(String::from)
    .collect();
    a.push(env.exe());
    a.extend(Env::args(read_only));
    a
}

fn cli_remove_args(c: McpClient) -> Vec<String> {
    match c {
        McpClient::ClaudeCode => vec!["mcp", "remove", "--scope", "user", NAME],
        _ => vec!["mcp", "remove", NAME],
    }
    .into_iter()
    .map(String::from)
    .collect()
}

fn shell_line(program: &Path, args: &[String]) -> String {
    let q = |s: &str| {
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
        {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', r"'\''"))
        }
    };
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::iter::once(q(&name))
        .chain(args.iter().map(|a| q(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An existing entry for people: command and args, env names with values masked.
pub fn describe(cur: &Value) -> String {
    let t = if cur.get("transport").is_some() {
        &cur["transport"]
    } else {
        cur
    };
    let mut s = format!(
        "command {}, args {}",
        t.get("command")
            .map_or("(none)".to_string(), |c| c.to_string()),
        t.get("args").map_or("[]".to_string(), |a| a.to_string())
    );
    let names: Vec<String> = t
        .get("env")
        .and_then(Value::as_object)
        .map(|e| e.keys().map(|k| format!("{k}=***")).collect())
        .unwrap_or_default();
    if !names.is_empty() {
        s.push_str(&format!(", env {}", names.join(" ")));
    }
    for k in ["url", "headers"] {
        if t.get(k).is_some_and(|v| !v.is_null()) {
            s.push_str(&format!(", {k} ***"));
        }
    }
    s
}

/// Whether an entry already runs this executable with these arguments.
fn same_entry(cur: &Value, exe: &str, args: &[String]) -> bool {
    let t = if cur.get("transport").is_some() {
        &cur["transport"]
    } else {
        cur
    };
    t["command"] == exe && t["args"] == json!(args)
}

/// What a file edit would do.
#[derive(Debug, PartialEq)]
pub enum Edit {
    /// Nothing to change.
    Same(String),
    /// Write `text` to the file. `what` names only the mulchie entry.
    Write { text: String, what: String },
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// The new JSON text for an install (`want` Some) or uninstall (None).
/// Other entries keep their content and order.
pub fn edit_json(
    old: Option<&str>,
    path: &Path,
    want: Option<&Value>,
    force: bool,
) -> Result<Edit, String> {
    let mut root: Value = match old {
        None => json!({}),
        Some(s) if s.trim().is_empty() => json!({}),
        Some(s) => serde_json::from_str(s).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); fix it, or add the server by hand (see `mulchie mcp config`). It was left as it is.",
                path.display()
            )
        })?,
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(format!(
            "{} is not a JSON object; it was left as it is",
            path.display()
        ));
    };
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err(format!(
            "mcpServers in {} is not an object; it was left as it is",
            path.display()
        ));
    };
    let cur = servers.get(NAME).cloned();
    let what = match (want, &cur) {
        (Some(w), Some(c)) if c == w => return Ok(Edit::Same("already set up".into())),
        (Some(_), Some(c)) if !force => {
            return Err(format!(
                "{} already has a different `{NAME}` entry ({}). Pass --force to replace it.",
                path.display(),
                describe(c)
            ))
        }
        (None, None) => return Ok(Edit::Same("not set up".into())),
        (Some(w), Some(c)) => {
            let what = format!("replace mcpServers.{NAME} ({}) with {w}", describe(c));
            servers.insert(NAME.into(), w.clone());
            what
        }
        (Some(w), None) => {
            servers.insert(NAME.into(), w.clone());
            format!("add mcpServers.{NAME}: {w}")
        }
        (None, Some(c)) => {
            servers.shift_remove(NAME);
            format!("remove mcpServers.{NAME} ({})", describe(c))
        }
    };
    let mut text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(Edit::Write { text, what })
}

fn toml_entry(item: &toml_edit::Item) -> Value {
    let s = |k: &str| item.get(k).and_then(|v| v.as_str()).map(String::from);
    let args: Vec<String> = item
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let env: Map<String, Value> = item
        .get("env")
        .and_then(|e| e.as_table_like())
        .map(|t| {
            t.iter()
                .map(|(k, _)| (k.to_string(), json!("***")))
                .collect()
        })
        .unwrap_or_default();
    json!({ "command": s("command"), "args": args, "env": env, "url": s("url") })
}

/// The new TOML text for an install (`want` Some) or uninstall (None).
pub fn edit_toml(
    old: Option<&str>,
    path: &Path,
    want: Option<(&str, &[String])>,
    force: bool,
) -> Result<Edit, String> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let mut doc: DocumentMut = old.unwrap_or("").parse().map_err(|e| {
        format!(
            "{} is not valid TOML ({e}); fix it, or add the server by hand. It was left as it is.",
            path.display()
        )
    })?;
    let cur = doc
        .get("mcp_servers")
        .and_then(|s| s.get(NAME))
        .map(toml_entry);
    let Some(want) = want else {
        let Some(c) = cur else {
            return Ok(Edit::Same("not set up".into()));
        };
        if let Some(t) = doc["mcp_servers"].as_table_like_mut() {
            t.remove(NAME);
        }
        return Ok(Edit::Write {
            text: doc.to_string(),
            what: format!("remove [mcp_servers.{NAME}] ({})", describe(&c)),
        });
    };
    let (cmd, args) = want;
    if let Some(c) = &cur {
        if same_entry(c, cmd, args) {
            return Ok(Edit::Same("already set up".into()));
        }
        if !force {
            return Err(format!(
                "{} already has a different [mcp_servers.{NAME}] ({}). Pass --force to replace it.",
                path.display(),
                describe(c)
            ));
        }
    }
    if doc.get("mcp_servers").is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = Item::Table(t);
    }
    let Some(servers) = doc["mcp_servers"].as_table_mut() else {
        return Err(format!(
            "mcp_servers in {} is not a table; it was left as it is",
            path.display()
        ));
    };
    let mut t = Table::new();
    t["command"] = value(cmd);
    let mut a = Array::new();
    for x in args {
        a.push(x.as_str());
    }
    t["args"] = value(a);
    servers.insert(NAME, Item::Table(t));
    let new = format!("command = {cmd:?}, args = {args:?}");
    Ok(Edit::Write {
        text: doc.to_string(),
        what: match &cur {
            Some(c) => format!("replace [mcp_servers.{NAME}] ({}) with {new}", describe(c)),
            None => format!("add [mcp_servers.{NAME}] {new}"),
        },
    })
}

/// The file to write: `path`, or the file its symbolic link points to.
/// A link to nothing is refused.
pub fn resolve(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path).map_err(|_| {
            let to = std::fs::read_link(path)
                .map(|t| t.display().to_string())
                .unwrap_or_else(|_| "?".into());
            format!(
                "{} is a symbolic link to {to}, which does not exist. Fix or remove the link, then try again. Nothing was changed.",
                path.display()
            )
        }),
        Ok(m) if !m.is_file() => Err(format!(
            "{} is not a regular file; it was left as it is",
            path.display()
        )),
        Ok(_) => Ok(path.to_path_buf()),
    }
}

/// Write `text` to `target` through a temp file and a rename, with `mode`.
/// If the file no longer holds `expected` just before the rename (the app
/// saved it meanwhile), nothing is written.
pub fn write_checked(
    target: &Path,
    text: &str,
    mode: u32,
    expected: Option<&str>,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rand = mulchie_core::paths::random_hex(8).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{name}.mulchie-tmp-{rand}"));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.sync_all()
    })();
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&tmp);
        Err(msg)
    };
    if let Err(e) = res {
        return fail(format!("cannot write {}: {e}", tmp.display()));
    }
    match read_text(target) {
        Ok(now) if now.as_deref() == expected => {}
        Ok(_) => {
            return fail(format!(
                "{} changed while Mulchie was editing it, so nothing was written. Try again.",
                target.display()
            ))
        }
        Err(e) => return fail(e),
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        return fail(format!("cannot write {}: {e}", target.display()));
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Back up and write, or say what would happen. `shown` is the path the
/// app uses; `target` is where the bytes live (the same unless a link).
fn apply(
    shown: &Path,
    target: &Path,
    old: Option<&str>,
    edit: Edit,
    dry_run: bool,
) -> Result<String, String> {
    let (text, what) = match edit {
        Edit::Same(s) => return Ok(format!("{}: {s}, nothing to change", shown.display())),
        Edit::Write { text, what } => (text, what),
    };
    let via = if shown != target {
        format!(" (a link to {})", target.display())
    } else {
        String::new()
    };
    if dry_run {
        let backup = if old.is_some() {
            ", backing up the old file first"
        } else {
            ""
        };
        return Ok(format!(
            "would {what} in {}{via}{backup}; the rest of the file is kept",
            shown.display()
        ));
    }
    let mut mode = 0o600;
    let mut note = String::new();
    if old.is_some() {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(target) {
            mode = m.permissions().mode() & 0o7777;
        }
        let backup = PathBuf::from(format!(
            "{}.mulchie-backup-{}",
            target.display(),
            now_unix()
        ));
        std::fs::copy(target, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", target.display()))?;
        note = format!(" (old file saved as {})", backup.display());
    } else if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    write_checked(target, &text, mode, old)?;
    Ok(format!("{}{via}: {what}{note}", shown.display()))
}

fn edit_file(
    c: McpClient,
    env: &Env,
    m: &Method,
    want: Option<bool>,
    force: bool,
    dry_run: bool,
) -> Result<String, String> {
    let (Method::Json { path, .. } | Method::Toml { path }) = m else {
        unreachable!("{}", label(c))
    };
    let target = resolve(path)?;
    let old = read_text(&target)?;
    let edit = match m {
        Method::Json { typed, .. } => {
            let w = want.map(|ro| entry(env, ro, *typed));
            edit_json(old.as_deref(), path, w.as_ref(), force)?
        }
        _ => {
            let exe = env.exe();
            let args = want.map(Env::args);
            let w = args.as_ref().map(|a| (exe.as_str(), &a[..]));
            edit_toml(old.as_deref(), path, w, force)?
        }
    };
    apply(path, &target, old.as_deref(), edit, dry_run)
}

/// Run a client's CLI. `shown` is how the command is named in errors
/// (never with arguments that may hold secrets).
fn run_cli(program: &Path, args: &[String], shown: &str) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = stdout;
        }
        Err(format!("`{shown}` failed: {msg}"))
    }
}

/// The client's existing `mulchie` entry, read without changing anything.
fn cli_existing(c: McpClient, env: &Env, program: &Path) -> Result<Option<Value>, String> {
    match c {
        McpClient::ClaudeCode => {
            let p = env.claude_json();
            let Some(text) = read_text(&p)? else {
                return Ok(None);
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "cannot read Claude Code's config {} ({e}), so its existing entries cannot be checked; nothing was changed",
                    p.display()
                )
            })?;
            Ok(v.get("mcpServers").and_then(|s| s.get(NAME)).cloned())
        }
        _ => {
            let args: Vec<String> = ["mcp", "get", NAME, "--json"].map(String::from).to_vec();
            match run_cli(program, &args, "codex mcp get mulchie --json") {
                Ok(out) => {
                    let v: Value = serde_json::from_str(&out)
                        .map_err(|e| format!("cannot read `codex mcp get` output: {e}"))?;
                    Ok(Some(v))
                }
                Err(e) if e.contains("No MCP server") => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Keep a copy of an entry we are about to remove, readable only by the user.
fn save_entry(env: &Env, cur: &Value) -> Result<PathBuf, String> {
    let dir = env
        .claude_json()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env.home.clone());
    let rand = mulchie_core::paths::random_hex(4).map_err(|e| e.to_string())?;
    let p = dir.join(format!(
        ".mulchie-replaced-entry-{}-{rand}.json",
        now_unix()
    ));
    let text = serde_json::to_string_pretty(cur).map_err(|e| e.to_string())?;
    write_checked(&p, &text, 0o600, None)?;
    Ok(p)
}

fn install_cli(
    c: McpClient,
    env: &Env,
    program: &Path,
    a: &McpInstallArgs,
) -> Result<String, String> {
    let add = cli_args(c, env, a.read_only);
    let existing = cli_existing(c, env, program)?;
    if let Some(cur) = &existing {
        if same_entry(cur, &env.exe(), &Env::args(a.read_only)) {
            return Ok(format!("{}: already set up, nothing to change", label(c)));
        }
        if !a.force {
            return Err(format!(
                "{} already has a different `{NAME}` server ({}). Pass --force to replace it.",
                label(c),
                describe(cur)
            ));
        }
    }
    // Codex's `mcp add` replaces an entry in place; Claude Code's refuses,
    // so there the old one is removed first and put back if adding fails.
    let remove_first = existing.is_some() && c == McpClient::ClaudeCode;
    let remove = cli_remove_args(c);
    if a.dry_run {
        let mut s = String::new();
        if let Some(cur) = &existing {
            s.push_str(&format!(
                "would replace the existing `{NAME}` server ({})\n",
                describe(cur)
            ));
        }
        s.push_str("would run:\n");
        if remove_first {
            s.push_str(&format!("  {}\n", shell_line(program, &remove)));
        }
        s.push_str(&format!("  {}", shell_line(program, &add)));
        if remove_first {
            s.push_str("\nand put the old entry back if adding fails");
        }
        return Ok(s);
    }
    let mut steps = Vec::new();
    let mut saved = None;
    if remove_first {
        let cur = existing.as_ref().expect("checked above");
        let p = save_entry(env, cur)?;
        run_cli(program, &remove, &shell_line(program, &remove))
            .map_err(|e| format!("{e}\nThe existing entry was not removed, so nothing changed."))?;
        steps.push(format!(
            "removed the previous `{NAME}` server ({}); a copy is in {}",
            describe(cur),
            p.display()
        ));
        saved = Some(p);
    }
    match run_cli(program, &add, &shell_line(program, &add)) {
        Ok(_) => {
            steps.push(format!("ran `{}`", shell_line(program, &add)));
            Ok(format!("{}. {}", steps.join("; "), restart_note(c)))
        }
        Err(e) => {
            let mut msg = String::new();
            for s in &steps {
                msg.push_str(&format!("{s}\n"));
            }
            msg.push_str(&e);
            if let (true, Some(cur), Some(p)) = (remove_first, &existing, &saved) {
                let mut restore: Vec<String> = ["mcp", "add-json", "--scope", "user", NAME]
                    .map(String::from)
                    .to_vec();
                restore.push(cur.to_string());
                match run_cli(program, &restore, "claude mcp add-json --scope user mulchie …") {
                    Ok(_) => msg.push_str(&format!(
                        "\nThe previous `{NAME}` server was put back, so nothing changed."
                    )),
                    Err(e2) => msg.push_str(&format!(
                        "\nPutting the previous server back also failed: {e2}\nIts settings are in {}; add them back with `claude mcp add-json --scope user {NAME} \"$(cat {})\"`.",
                        p.display(),
                        p.display()
                    )),
                }
            } else if existing.is_some() {
                msg.push_str(&format!(
                    "\nCheck what is set now with `{} mcp get {NAME}`.",
                    label(c)
                ));
            } else if !a.force {
                msg.push_str(&format!(
                    "\nIf a `{NAME}` server already exists in another scope, remove it or pass --force."
                ));
            }
            Err(msg)
        }
    }
}

// ---------------------------------------------------------------- commands

fn restart_note(c: McpClient) -> &'static str {
    match c {
        McpClient::ClaudeCode => "Start a new Claude Code session (or run /mcp) to use it.",
        McpClient::Codex => "Start a new Codex session to use it.",
        McpClient::ClaudeDesktop => "Quit and reopen Claude Desktop to use it.",
        McpClient::Cursor => "Restart Cursor (or reload MCP servers in its settings) to use it.",
    }
}

pub fn install_in(env: &Env, a: &McpInstallArgs) -> Result<String, String> {
    let Some(c) = a.client else {
        return Ok(overview(env, a.read_only));
    };
    let m = method(c, env);
    match &m {
        Method::Manual(why) => Err(format!("cannot set up {}: {why}", label(c))),
        Method::Cli { program } => install_cli(c, env, program, a),
        _ => {
            let s = edit_file(c, env, &m, Some(a.read_only), a.force, a.dry_run)?;
            Ok(if a.dry_run || s.contains("nothing to change") {
                s
            } else {
                format!("{s}. {}", restart_note(c))
            })
        }
    }
}

pub fn uninstall_in(env: &Env, a: &McpUninstallArgs) -> Result<String, String> {
    let Some(c) = a.client else {
        return Err(format!(
            "say which app with --client ({})",
            ALL.map(label).join(", ")
        ));
    };
    let m = method(c, env);
    match &m {
        Method::Manual(why) => Err(format!("cannot change {}: {why}", label(c))),
        Method::Cli { program } => {
            let Some(cur) = cli_existing(c, env, program)? else {
                return Ok(format!("{}: not set up, nothing to change", label(c)));
            };
            let args = cli_remove_args(c);
            let line = shell_line(program, &args);
            if a.dry_run {
                return Ok(format!(
                    "would remove the `{NAME}` server ({}) by running:\n  {line}",
                    describe(&cur)
                ));
            }
            run_cli(program, &args, &line)?;
            Ok(format!("ran `{line}`"))
        }
        _ => edit_file(c, env, &m, None, false, a.dry_run),
    }
}

/// What `mcp install` with no `--client` prints.
fn overview(env: &Env, read_only: bool) -> String {
    let mut s = String::from("Agent apps Mulchie can connect to:\n");
    for c in ALL {
        let found = detected(c, env);
        let how = match method(c, env) {
            Method::Cli { program } => format!(
                "run `{}`",
                shell_line(&program, &cli_args(c, env, read_only))
            ),
            Method::Json { path, .. } | Method::Toml { path } => format!("edit {}", path.display()),
            Method::Manual(why) => why,
        };
        s.push_str(&format!(
            "\n  {:<15} {}\n  {:<15} would {how}\n",
            label(c),
            match &found {
                Some(why) => format!("found ({why})"),
                None => "not found".into(),
            },
            ""
        ));
    }
    let ro = if read_only { " --read-only" } else { "" };
    s.push_str(&format!(
        "\nConnect one with `mulchie mcp install --client <app>{ro}` (add --dry-run to see the change first).\n"
    ));
    s
}

/// The snippet for any client that takes `mcpServers` JSON.
pub fn snippet(env: &Env, read_only: bool) -> Value {
    json!({ "mcpServers": { NAME: entry(env, read_only, false) } })
}

fn report(r: Result<String, String>) -> i32 {
    match r {
        Ok(s) => {
            println!("{}", s.trim_end());
            0
        }
        Err(e) => {
            eprintln!("mulchie: {e}");
            1
        }
    }
}

pub fn install(a: &McpInstallArgs) -> i32 {
    report(Env::from_process().and_then(|env| install_in(&env, a)))
}

pub fn uninstall(a: &McpUninstallArgs) -> i32 {
    report(Env::from_process().and_then(|env| uninstall_in(&env, a)))
}

pub fn config(read_only: bool) -> i32 {
    report(
        Env::from_process()
            .map(|env| serde_json::to_string_pretty(&snippet(&env, read_only)).unwrap_or_default()),
    )
}

#[cfg(test)]
#[path = "mcp_install_tests.rs"]
mod tests;
