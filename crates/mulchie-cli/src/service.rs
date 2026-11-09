//! `mulchie service install|uninstall|status`: a launchd agent on macOS, a
//! systemd user unit on Linux, running `mulchie serve`.
//!
//! The templates are copies of `packaging/launchd/dev.mulchie.serve.plist`
//! and `packaging/systemd/mulchie.service` kept inside the crate (so `cargo
//! publish` includes them); a test checks they still match.

use crate::cli::ServiceCommand;
use crate::commands::print_json;
use mulchie_core::{lock, Paths};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const LAUNCHD_TEMPLATE: &str = include_str!("../service/dev.mulchie.serve.plist");
pub const SYSTEMD_TEMPLATE: &str = include_str!("../service/mulchie.service");

pub const LAUNCHD_LABEL: &str = "dev.mulchie.serve";
pub const SYSTEMD_UNIT: &str = "mulchie.service";
const SYSTEMD_EXEC_LINE: &str = "ExecStart=%h/.local/bin/mulchie serve";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Launchd,
    Systemd,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Launchd
        } else {
            Platform::Systemd
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Platform::Launchd => "launchd agent",
            Platform::Systemd => "systemd user unit",
        }
    }
}

/// Where the service file lives.
pub fn unit_path(platform: Platform, paths: &Paths) -> PathBuf {
    match platform {
        Platform::Launchd => paths
            .home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
        Platform::Systemd => paths
            .xdg_config_home
            .clone()
            .unwrap_or_else(|| paths.home.join(".config"))
            .join("systemd/user")
            .join(SYSTEMD_UNIT),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A path as one systemd command-line word: `%` and `$` doubled, quoted
/// when it holds whitespace, quotes or backslashes.
fn systemd_word(s: &str) -> String {
    let e = s.replace('%', "%%").replace('$', "$$");
    if e.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
    {
        format!("\"{}\"", e.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        e
    }
}

/// The service file for `exe`, from the packaging template.
pub fn render(platform: Platform, exe: &Path, home: &Path) -> String {
    match platform {
        Platform::Launchd => {
            // Drop the reference copy's instructions; keep the plist.
            let t = LAUNCHD_TEMPLATE;
            let body = match (t.find("<!--"), t.find("-->")) {
                (Some(a), Some(b)) if a < b => {
                    format!("{}{}", &t[..a], t[b + 3..].trim_start_matches('\n'))
                }
                _ => t.to_string(),
            };
            let body = body.replacen(
                "<plist",
                "<!-- Written by `mulchie service install`; remove with `mulchie service uninstall`. -->\n<plist",
                1,
            );
            body.replace("@MULCHIE_BIN@", &xml_escape(&exe.to_string_lossy()))
                .replace("@HOME@", &xml_escape(&home.to_string_lossy()))
        }
        Platform::Systemd => {
            let mut out = String::from(
                "# Written by `mulchie service install`; remove with `mulchie service uninstall`.\n",
            );
            let mut in_header = true;
            for line in SYSTEMD_TEMPLATE.lines() {
                if in_header && (line.starts_with('#') || line.trim().is_empty()) {
                    continue;
                }
                in_header = false;
                if line == SYSTEMD_EXEC_LINE {
                    out.push_str(&format!(
                        "ExecStart={} serve\n",
                        systemd_word(&exe.to_string_lossy())
                    ));
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out
        }
    }
}

/// The binary the service should run: the `mulchie` on PATH when it is this
/// same file (a stable path such as Homebrew's `bin/` link survives
/// upgrades), otherwise this executable.
pub fn service_exe() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let me_real = std::fs::canonicalize(&me).unwrap_or_else(|_| me.clone());
    if let Some(on_path) = which("mulchie") {
        if std::fs::canonicalize(&on_path).ok().as_ref() == Some(&me_real) {
            return Ok(on_path);
        }
    }
    Ok(me_real)
}

/// The first `name` on PATH that is an executable file.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn install_cmds(platform: Platform, file: &Path, reload: bool) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match platform {
        Platform::Launchd => {
            let mut v = Vec::new();
            if reload {
                v.push(s(&["launchctl", "bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid())]));
            }
            v.push(s(&[
                "launchctl",
                "bootstrap",
                &format!("gui/{}", uid()),
                &file.to_string_lossy(),
            ]));
            v
        }
        Platform::Systemd => vec![
            s(&["systemctl", "--user", "daemon-reload"]),
            s(&["systemctl", "--user", "enable", SYSTEMD_UNIT]),
            s(&["systemctl", "--user", "restart", SYSTEMD_UNIT]),
        ],
    }
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:=@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn show_cmd(c: &[String]) -> String {
    c.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

fn run_cmd(c: &[String]) -> Result<(), String> {
    let out = std::process::Command::new(&c[0])
        .args(&c[1..])
        .output()
        .map_err(|e| format!("could not run {}: {e}", c[0]))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "`{}` failed ({}): {}",
            show_cmd(c),
            out.status,
            msg.trim()
        ))
    }
}

/// Whether the service manager has it loaded (launchd) or active (systemd).
/// Read-only; `None` when the tool is missing.
pub fn loaded(platform: Platform) -> Option<bool> {
    let out = match platform {
        Platform::Launchd => std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
            .output(),
        Platform::Systemd => std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT])
            .output(),
    };
    out.ok().map(|o| o.status.success())
}

/// True when `exe` is inside a macOS app bundle and is not the bundled CLI.
pub fn is_app_exe(exe: &Path) -> bool {
    let in_bundle = exe
        .components()
        .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"));
    in_bundle && exe.file_name().is_some_and(|n| n != "mulchie")
}

/// The running engine, and whether it is the menu bar app.
pub fn engine_holder(paths: &Paths) -> Option<(lock::Holder, bool)> {
    use mulchie_core::process::{ProcessSource, SystemProcesses};
    let h = lock::running_engine(paths)?;
    let app = SystemProcesses::new()
        .get(h.pid)
        .and_then(|p| p.exe)
        .is_some_and(|e| is_app_exe(&e));
    Some((h, app))
}

pub fn command(paths: &Paths, cmd: &ServiceCommand) -> i32 {
    let platform = Platform::current();
    match cmd {
        ServiceCommand::Install { force, dry_run } => install(paths, platform, *force, *dry_run),
        ServiceCommand::Uninstall { dry_run } => uninstall(paths, platform, *dry_run),
        ServiceCommand::Status(j) => status(paths, platform, j.json),
    }
}

fn install(paths: &Paths, platform: Platform, force: bool, dry_run: bool) -> i32 {
    let exe = match service_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("mulchie: {e}");
            return 1;
        }
    };
    let s = exe.to_string_lossy();
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        eprintln!(
            "mulchie: note: {} is a cargo build output; the service breaks if it is rebuilt elsewhere or cleaned",
            exe.display()
        );
    }
    if let Some((h, app)) = engine_holder(paths) {
        if app && platform == Platform::Launchd {
            eprintln!(
                "mulchie: the Mulchie app is running the engine (pid {}), so a service is not needed; \
                 the app's engine serves the CLI and MCP. Use --force to install one anyway \
                 (it waits, restarting every 10 s, while the app runs).",
                h.pid
            );
            if !force && !dry_run {
                return 1;
            }
        } else {
            eprintln!(
                "mulchie: note: an engine is already running (pid {}); the service's engine will wait until it stops",
                h.pid
            );
        }
    }
    let file = unit_path(platform, paths);
    let content = render(platform, &exe, &paths.home);
    let existing = std::fs::read_to_string(&file).ok();
    let same = existing.as_deref() == Some(content.as_str());
    if existing.is_some() && !same && !force && !dry_run {
        eprintln!(
            "mulchie: {} already exists with different content; not replacing it without --force \
             (see the difference with `mulchie service install --dry-run`)",
            file.display()
        );
        return 1;
    }
    let is_loaded = loaded(platform).unwrap_or(false);
    if same && is_loaded {
        println!(
            "The {} is already installed and loaded: {}",
            platform.name(),
            file.display()
        );
        return 0;
    }
    let cmds = install_cmds(platform, &file, platform == Platform::Launchd && is_loaded);
    if dry_run {
        if same {
            println!("{} is already up to date.", file.display());
        } else {
            println!(
                "Would write {}{}:\n",
                file.display(),
                if existing.is_some() { " (replacing it)" } else { "" }
            );
            print!("{content}");
            if let Some(old) = &existing {
                println!("\nIt currently contains:\n");
                print!("{old}");
                if !force {
                    println!("\n(Replacing a different file needs --force.)");
                }
            }
        }
        println!("\nWould run:");
        for c in &cmds {
            println!("  {}", show_cmd(c));
        }
        return 0;
    }
    if !same {
        if let Some(dir) = file.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("mulchie: cannot create {}: {e}", dir.display());
                return 1;
            }
        }
        if platform == Platform::Launchd {
            let _ = std::fs::create_dir_all(paths.home.join("Library/Logs"));
        }
        if let Err(e) = mulchie_core::config::write_atomic(&file, content.as_bytes(), 0o644) {
            eprintln!("mulchie: cannot write {}: {e}", file.display());
            return 1;
        }
        println!("Wrote {}", file.display());
    }
    for c in &cmds {
        // `bootout` of a job that is not loaded fails harmlessly.
        let r = run_cmd(c);
        match r {
            Ok(()) => println!("Ran {}", show_cmd(c)),
            Err(e) if c.get(1).map(String::as_str) == Some("bootout") => {
                eprintln!("mulchie: note: {e}")
            }
            Err(e) => {
                eprintln!("mulchie: {e}");
                return 1;
            }
        }
    }
    println!(
        "The engine now starts at login. Check it with `mulchie service status` or `mulchie doctor`."
    );
    0
}

fn looks_like_ours(platform: Platform, content: &str) -> bool {
    match platform {
        Platform::Launchd => content.contains(&format!("<string>{LAUNCHD_LABEL}</string>")),
        Platform::Systemd => content.contains("ExecStart=") && content.contains(" serve"),
    }
}

fn uninstall(paths: &Paths, platform: Platform, dry_run: bool) -> i32 {
    let file = unit_path(platform, paths);
    let existing = std::fs::read_to_string(&file).ok();
    let is_loaded = loaded(platform).unwrap_or(false);
    if existing.is_none() && !is_loaded {
        println!("The {} is not installed ({}).", platform.name(), file.display());
        return 0;
    }
    if let Some(c) = &existing {
        if !looks_like_ours(platform, c) {
            eprintln!(
                "mulchie: {} does not look like Mulchie's service file; not touching it",
                file.display()
            );
            return 1;
        }
    }
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let stop: Vec<Vec<String>> = match platform {
        Platform::Launchd if is_loaded => vec![s(&[
            "launchctl",
            "bootout",
            &format!("gui/{}/{LAUNCHD_LABEL}", uid()),
        ])],
        Platform::Launchd => vec![],
        Platform::Systemd => vec![s(&["systemctl", "--user", "disable", "--now", SYSTEMD_UNIT])],
    };
    let after: Vec<Vec<String>> = match platform {
        Platform::Systemd => vec![s(&["systemctl", "--user", "daemon-reload"])],
        Platform::Launchd => vec![],
    };
    if dry_run {
        println!("Would run:");
        for c in &stop {
            println!("  {}", show_cmd(c));
        }
        if existing.is_some() {
            println!("Would remove {}", file.display());
        }
        for c in &after {
            println!("  {}", show_cmd(c));
        }
        return 0;
    }
    for c in &stop {
        if let Err(e) = run_cmd(c) {
            eprintln!("mulchie: {e}");
            if platform == Platform::Launchd {
                return 1;
            }
        } else {
            println!("Ran {}", show_cmd(c));
        }
    }
    if existing.is_some() {
        if let Err(e) = std::fs::remove_file(&file) {
            eprintln!("mulchie: cannot remove {}: {e}", file.display());
            return 1;
        }
        println!("Removed {}", file.display());
    }
    for c in &after {
        if let Err(e) = run_cmd(c) {
            eprintln!("mulchie: note: {e}");
        }
    }
    println!("The service is removed. Your config and backups in {} are kept.", paths.config_dir.display());
    0
}

/// What `service status` and `doctor` report.
pub struct ServiceState {
    pub file: PathBuf,
    pub installed: bool,
    /// The file matches what `install` would write now.
    pub current: bool,
    pub loaded: Option<bool>,
}

pub fn state(paths: &Paths, platform: Platform) -> ServiceState {
    let file = unit_path(platform, paths);
    let existing = std::fs::read_to_string(&file).ok();
    let current = match (&existing, service_exe()) {
        (Some(c), Ok(exe)) => *c == render(platform, &exe, &paths.home),
        _ => false,
    };
    ServiceState {
        installed: existing.is_some(),
        current,
        loaded: loaded(platform),
        file,
    }
}

fn status(paths: &Paths, platform: Platform, json_out: bool) -> i32 {
    let st = state(paths, platform);
    let engine = engine_holder(paths);
    if json_out {
        print_json(&json!({
            "kind": platform.name(),
            "file": st.file,
            "installed": st.installed,
            "current": st.current,
            "loaded": st.loaded,
            "engine": engine.as_ref().map(|(h, app)| json!({ "pid": h.pid, "port": h.port, "app": app })),
        }));
        return 0;
    }
    println!("{}: {}", platform.name(), st.file.display());
    println!(
        "  installed: {}",
        match (st.installed, st.current) {
            (false, _) => "no",
            (true, true) => "yes",
            (true, false) => "yes, but different from what `mulchie service install` would write now",
        }
    );
    println!(
        "  {}: {}",
        if platform == Platform::Launchd { "loaded" } else { "active" },
        match st.loaded {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown (service manager not found)",
        }
    );
    match engine {
        Some((h, app)) => println!(
            "  engine: running (pid {}{}{})",
            h.pid,
            h.port.map(|p| format!(", port {p}")).unwrap_or_default(),
            if app { ", the Mulchie app" } else { "" }
        ),
        None => println!("  engine: not running"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packaging(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging").join(rel)
    }

    #[test]
    fn templates_match_packaging() {
        for (rel, ours) in [
            ("launchd/dev.mulchie.serve.plist", LAUNCHD_TEMPLATE),
            ("systemd/mulchie.service", SYSTEMD_TEMPLATE),
        ] {
            // Absent in a published crate; present in the repo.
            if let Ok(p) = std::fs::read_to_string(packaging(rel)) {
                assert_eq!(
                    p, ours,
                    "crates/mulchie-cli/service/ is out of date with packaging/{rel}"
                );
            }
        }
    }

    #[test]
    fn launchd_rendering() {
        let out = render(
            Platform::Launchd,
            Path::new("/Users/a b/bin/mulchie&co"),
            Path::new("/Users/a b"),
        );
        assert!(!out.contains('@'), "placeholder left:\n{out}");
        assert!(out.contains("<string>/Users/a b/bin/mulchie&amp;co</string>"));
        assert!(out.contains("<string>/Users/a b/Library/Logs/mulchie-serve.log</string>"));
        assert!(out.contains("<string>dev.mulchie.serve</string>"));
        assert!(out.contains("<string>serve</string>"));
        assert!(!out.contains("Manual install"));
        assert!(out.starts_with("<?xml"));
        assert!(looks_like_ours(Platform::Launchd, &out));
        // Exactly one comment, before <plist>.
        assert_eq!(out.matches("<!--").count(), 1);
    }

    #[test]
    fn systemd_rendering() {
        let out = render(
            Platform::Systemd,
            Path::new("/home/u/.local/bin/mulchie"),
            Path::new("/home/u"),
        );
        assert!(SYSTEMD_TEMPLATE.contains(SYSTEMD_EXEC_LINE));
        assert!(out.contains("\nExecStart=/home/u/.local/bin/mulchie serve\n"));
        assert!(out.starts_with("# Written by"));
        assert!(out.contains("[Unit]") && out.contains("[Install]"));
        assert!(!out.contains("Manual install"));
        assert!(looks_like_ours(Platform::Systemd, &out));
        let odd = render(
            Platform::Systemd,
            Path::new("/opt/my apps/50%/mulchie"),
            Path::new("/h"),
        );
        assert!(odd.contains("ExecStart=\"/opt/my apps/50%%/mulchie\" serve"));
    }

    #[test]
    fn unit_paths() {
        let p = Paths::under(Path::new("/r"));
        assert_eq!(
            unit_path(Platform::Launchd, &p),
            Path::new("/r/home/Library/LaunchAgents/dev.mulchie.serve.plist")
        );
        assert_eq!(
            unit_path(Platform::Systemd, &p),
            Path::new("/r/home/.config/systemd/user/mulchie.service")
        );
        let mut x = p.clone();
        x.xdg_config_home = Some("/x".into());
        assert_eq!(
            unit_path(Platform::Systemd, &x),
            Path::new("/x/systemd/user/mulchie.service")
        );
    }

    #[test]
    fn app_detection() {
        assert!(is_app_exe(Path::new(
            "/Applications/Mulchie.app/Contents/MacOS/mulchie-app"
        )));
        assert!(!is_app_exe(Path::new(
            "/Applications/Mulchie.app/Contents/MacOS/mulchie"
        )));
        assert!(!is_app_exe(Path::new("/usr/local/bin/mulchie")));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("gui/501"), "gui/501");
        assert_eq!(shell_quote("/a b"), "'/a b'");
        assert_eq!(systemd_word("/a/$x"), "/a/$$x");
    }
}
