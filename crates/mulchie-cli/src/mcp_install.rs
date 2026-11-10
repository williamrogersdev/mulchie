//! `mulchie mcp install | uninstall | config`: connect the MCP server to an
//! agent app in one step.
//!
//! Claude Code and Codex are set up through their own CLI when it is on
//! PATH. Claude Desktop, Cursor and Codex without its CLI get their
//! user-level config file edited: other entries are kept, a file that does
//! not parse is refused, the old file is copied to
//! `<file>.mulchie-backup-<unix>` and the new one is written atomically.

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

/// What a file edit would do.
#[derive(Debug, PartialEq)]
pub enum Edit {
    /// Nothing to change.
    Same(String),
    /// Write `text` to the file.
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
    match (want, servers.get(NAME)) {
        (Some(w), Some(cur)) if cur == w => return Ok(Edit::Same("already set up".into())),
        (Some(_), Some(cur)) if !force => {
            return Err(format!(
                "{} already has a different `{NAME}` entry: {cur}. Pass --force to replace it.",
                path.display()
            ))
        }
        (None, None) => return Ok(Edit::Same("not set up".into())),
        _ => {}
    }
    let what = match want {
        Some(w) => {
            servers.insert(NAME.into(), w.clone());
            format!("set mcpServers.{NAME} to {w}")
        }
        None => {
            servers.remove(NAME);
            format!("remove mcpServers.{NAME}")
        }
    };
    let mut text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(Edit::Write { text, what })
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
    let has = doc.get("mcp_servers").and_then(|s| s.get(NAME)).is_some();
    let Some(want) = want else {
        if !has {
            return Ok(Edit::Same("not set up".into()));
        }
        if let Some(t) = doc["mcp_servers"].as_table_like_mut() {
            t.remove(NAME);
        }
        return Ok(Edit::Write {
            text: doc.to_string(),
            what: format!("remove [mcp_servers.{NAME}]"),
        });
    };
    let (cmd, args) = want;
    if has {
        let cur = &doc["mcp_servers"][NAME];
        let cur_cmd = cur.get("command").and_then(|v| v.as_str());
        let cur_args: Option<Vec<&str>> = cur
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect());
        let same = cur_cmd == Some(cmd)
            && cur_args.as_deref()
                == Some(&args.iter().map(String::as_str).collect::<Vec<_>>()[..]);
        if same {
            return Ok(Edit::Same("already set up".into()));
        }
        if !force {
            return Err(format!(
                "{} already has a different [mcp_servers.{NAME}]. Pass --force to replace it.",
                path.display()
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
    Ok(Edit::Write {
        text: doc.to_string(),
        what: format!("set [mcp_servers.{NAME}] command = {cmd:?}, args = {args:?}"),
    })
}

/// Back up and write, or say what would happen.
fn apply(path: &Path, edit: Edit, dry_run: bool, existed: bool) -> Result<String, String> {
    let (text, what) = match edit {
        Edit::Same(s) => return Ok(format!("{}: {s}, nothing to change", path.display())),
        Edit::Write { text, what } => (text, what),
    };
    if dry_run {
        let backup = if existed {
            ", backing up the old file first"
        } else {
            ""
        };
        return Ok(format!(
            "would {what} in {}{backup}. New file:\n{text}",
            path.display()
        ));
    }
    let mut mode = 0o600;
    let mut note = String::new();
    if existed {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path) {
            mode = m.permissions().mode() & 0o7777;
        }
        let backup = PathBuf::from(format!("{}.mulchie-backup-{}", path.display(), now_unix()));
        std::fs::copy(path, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
        note = format!(" (old file saved as {})", backup.display());
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    mulchie_core::config::write_atomic(path, text.as_bytes(), mode)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!("{}: {what}{note}", path.display()))
}

fn edit_file(
    c: McpClient,
    env: &Env,
    m: &Method,
    want: Option<bool>,
    force: bool,
    dry_run: bool,
) -> Result<String, String> {
    let (path, edit) = match m {
        Method::Json { path, typed } => {
            let old = read_text(path)?;
            let w = want.map(|ro| entry(env, ro, *typed));
            (path, edit_json(old.as_deref(), path, w.as_ref(), force)?)
        }
        Method::Toml { path } => {
            let old = read_text(path)?;
            let exe = env.exe();
            let args = want.map(Env::args);
            let w = args.as_ref().map(|a| (exe.as_str(), &a[..]));
            (path, edit_toml(old.as_deref(), path, w, force)?)
        }
        _ => unreachable!("{}", label(c)),
    };
    let existed = path.exists();
    apply(path, edit, dry_run, existed)
}

fn run_cli(program: &Path, args: &[String]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        Err(format!("`{}` failed: {msg}", shell_line(program, args)))
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
        Method::Cli { program } => {
            let add = cli_args(c, env, a.read_only);
            let remove = cli_remove_args(c);
            if a.dry_run {
                let mut s = String::from("would run:\n");
                if a.force {
                    s.push_str(&format!("  {}\n", shell_line(program, &remove)));
                }
                s.push_str(&format!("  {}", shell_line(program, &add)));
                return Ok(s);
            }
            if a.force {
                let _ = run_cli(program, &remove);
            }
            run_cli(program, &add).map_err(|e| {
                format!("{e}\nIf a `{NAME}` server already exists, pass --force to replace it.")
            })?;
            Ok(format!(
                "ran `{}`. {}",
                shell_line(program, &add),
                restart_note(c)
            ))
        }
        _ => {
            let s = edit_file(c, env, &m, Some(a.read_only), a.force, a.dry_run)?;
            Ok(if a.dry_run {
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
            let args = cli_remove_args(c);
            if a.dry_run {
                return Ok(format!("would run:\n  {}", shell_line(program, &args)));
            }
            run_cli(program, &args)?;
            Ok(format!("ran `{}`", shell_line(program, &args)))
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
