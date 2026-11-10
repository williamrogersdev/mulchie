//! `mulchie doctor`: check the install end to end. Nothing here changes
//! anything: it reads files, metadata, the lock, `/v1/health`, and runs
//! `git --version` and the service manager's read-only status query.

use crate::conn::START_HINT;
use crate::service::{self, Platform};
use mulchie_core::api::{self, Client};
use mulchie_core::{lock, Config, Paths};
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Pass,
    Info,
    Warn,
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Pass => "pass",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
    }
}

/// What the checks look at besides `Paths`, so tests can fake it.
pub struct Env {
    pub exe: PathBuf,
    /// The `mulchie` found on PATH.
    pub on_path: Option<PathBuf>,
    pub git: String,
    pub platform: Platform,
    /// Query launchctl/systemctl (off in tests).
    pub service_manager: bool,
}

impl Env {
    pub fn real() -> Self {
        Self {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mulchie")),
            on_path: service::which("mulchie"),
            git: "git".into(),
            platform: Platform::current(),
            service_manager: true,
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub fn checks(paths: &Paths, env: &Env) -> Vec<Check> {
    let mut v = Vec::new();
    let uid = unsafe { libc::getuid() };

    v.push(check(
        "version",
        Level::Info,
        format!(
            "mulchie {} at {}",
            env!("CARGO_PKG_VERSION"),
            env.exe.display()
        ),
    ));

    v.push(match &env.on_path {
        None => check(
            "path",
            Level::Warn,
            format!(
                "`mulchie` is not on PATH; add {} to PATH so agents and scripts can find it",
                env.exe.parent().unwrap_or(Path::new("/")).display()
            ),
        ),
        Some(p) if same_file(p, &env.exe) => check(
            "path",
            Level::Pass,
            format!("`mulchie` on PATH is {}", p.display()),
        ),
        Some(p) => check(
            "path",
            Level::Warn,
            format!(
                "`mulchie` on PATH is {}, not this binary ({}); they may be different versions",
                p.display(),
                env.exe.display()
            ),
        ),
    });

    // Config file.
    let file = paths.config_file();
    let loaded = Config::load_from(paths);
    v.push(match (&loaded.error, file.exists()) {
        (Some(e), _) => check(
            "config",
            Level::Fail,
            format!(
                "{}: {e}. Mulchie runs with safe defaults (automatic cleanup off) and never overwrites the file; fix it or move it aside",
                file.display()
            ),
        ),
        (None, false) => check(
            "config",
            Level::Pass,
            format!("no {} yet; using the defaults", file.display()),
        ),
        (None, true) => check(
            "config",
            Level::Pass,
            format!("{} parses and validates", file.display()),
        ),
    });

    // Config directory permissions.
    let dir = &paths.config_dir;
    v.push(match std::fs::symlink_metadata(dir) {
        Err(_) => check(
            "config dir",
            Level::Warn,
            format!(
                "{} does not exist yet; the engine creates it (mode 700)",
                dir.display()
            ),
        ),
        Ok(m) if !m.is_dir() => check(
            "config dir",
            Level::Fail,
            format!("{} is not a directory", dir.display()),
        ),
        Ok(m) if m.uid() != uid => check(
            "config dir",
            Level::Fail,
            format!(
                "{} belongs to uid {}, not you (uid {uid})",
                dir.display(),
                m.uid()
            ),
        ),
        Ok(m) if m.mode() & 0o022 != 0 => check(
            "config dir",
            Level::Fail,
            format!(
                "{} has mode {:o}, writable by others; run `chmod 700 {}`",
                dir.display(),
                m.mode() & 0o777,
                dir.display()
            ),
        ),
        Ok(m) if m.mode() & 0o077 != 0 => check(
            "config dir",
            Level::Warn,
            format!(
                "{} has mode {:o}; others can list it. `chmod 700 {}` is recommended",
                dir.display(),
                m.mode() & 0o777,
                dir.display()
            ),
        ),
        Ok(_) => check(
            "config dir",
            Level::Pass,
            format!("{} is private (700)", dir.display()),
        ),
    });

    // Token.
    let token = api::token_path_in(paths);
    v.push(if std::fs::symlink_metadata(&token).is_err() {
        check(
            "token",
            Level::Warn,
            format!(
                "{} does not exist yet; the engine creates it",
                token.display()
            ),
        )
    } else {
        match api::read_token(paths) {
            Ok(_) => check(
                "token",
                Level::Pass,
                format!("{} is yours and private (600)", token.display()),
            ),
            Err(e) => check("token", Level::Fail, e.to_string()),
        }
    });

    // Engine.
    let holder = lock::running_engine(paths);
    let app = holder.as_ref().is_some_and(|h| {
        use mulchie_core::process::{ProcessSource, SystemProcesses};
        SystemProcesses::new()
            .get(h.pid)
            .and_then(|p| p.exe)
            .is_some_and(|e| service::is_app_exe(&e))
    });
    v.push(engine_check(paths, holder.as_ref(), app));

    // git.
    v.push(
        match std::process::Command::new(&env.git)
            .arg("--version")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
        {
            Ok(o) if o.status.success() => check(
                "git",
                Level::Pass,
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            ),
            Ok(o) => check(
                "git",
                Level::Fail,
                format!(
                    "`{} --version` failed: {}",
                    env.git,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Err(e) => check(
                "git",
                Level::Fail,
                format!("git was not found ({e}); worktree scanning and sweeps need it"),
            ),
        },
    );

    // Service.
    v.push(service_check(paths, env, holder.is_some() && app));

    // MCP.
    let bin = match &env.on_path {
        Some(p) if same_file(p, &env.exe) => "mulchie".to_string(),
        _ => env.exe.display().to_string(),
    };
    v.push(check(
        "mcp",
        Level::Info,
        format!(
            "agents connect with `{bin} mcp`; `{bin} mcp install` sets it up for Claude Code, Claude Desktop, Cursor or Codex"
        ),
    ));
    v
}

fn engine_check(paths: &Paths, holder: Option<&lock::Holder>, app: bool) -> Check {
    let Some(h) = holder else {
        return check("engine", Level::Warn, format!("not running; {START_HINT}"));
    };
    let who = if app { "the Mulchie app" } else { "an engine" };
    let Some(port) = h.port else {
        return check(
            "engine",
            Level::Fail,
            format!(
                "{who} (pid {}) holds the engine lock but announced no API port; its port may be taken. Check its log, or restart it",
                h.pid
            ),
        );
    };
    let c = match Client::connect_in(paths, port) {
        Ok(c) => c,
        Err(e) => {
            return check(
                "engine",
                Level::Fail,
                format!("{who} (pid {}, port {port}) is not usable: {e}", h.pid),
            )
        }
    };
    let health = match c.request("GET", "/v1/health", None, api::HEALTH_TIMEOUT) {
        Ok(v) => v,
        Err(e) => {
            return check(
                "engine",
                Level::Fail,
                format!(
                    "{who} (pid {}, port {port}) did not answer: {}",
                    h.pid, e.message
                ),
            )
        }
    };
    let age = health["sample_age_secs"].as_u64();
    let version = health["version"].as_str().unwrap_or("?");
    let mut detail = format!(
        "{who} runs (pid {}, port {port}, version {version}), health {}",
        h.pid,
        health["health"].as_str().unwrap_or("?")
    );
    match age {
        Some(a) => detail.push_str(&format!(", last sample {a}s ago")),
        None => detail.push_str(", no sample yet"),
    }
    if health["stale"] == true {
        return check(
            "engine",
            if age.is_none() {
                Level::Warn
            } else {
                Level::Fail
            },
            format!("{detail}; its figures are stale. If this persists, restart it"),
        );
    }
    if version != env!("CARGO_PKG_VERSION") {
        return check(
            "engine",
            Level::Warn,
            format!("{detail}; this CLI is {}", env!("CARGO_PKG_VERSION")),
        );
    }
    check("engine", Level::Pass, detail)
}

fn service_check(paths: &Paths, env: &Env, app_engine: bool) -> Check {
    let file = service::unit_path(env.platform, paths);
    let installed = file.exists();
    let st = if env.service_manager {
        Some(service::state(paths, env.platform))
    } else {
        None
    };
    let name = env.platform.name();
    if !installed {
        let why = if app_engine && env.platform == Platform::Launchd {
            "not needed while the Mulchie app runs the engine"
        } else {
            "only needed for a headless engine: `mulchie service install`"
        };
        return check(
            "service",
            Level::Info,
            format!("{name} not installed ({why})"),
        );
    }
    let Some(st) = st else {
        return check(
            "service",
            Level::Info,
            format!("{name} installed at {}", file.display()),
        );
    };
    if !st.current {
        return check(
            "service",
            Level::Warn,
            format!(
                "{} differs from what this mulchie would write (another binary path?); `mulchie service install --force` rewrites it",
                file.display()
            ),
        );
    }
    match st.loaded {
        Some(true) => check(
            "service",
            Level::Pass,
            format!("{name} installed and loaded ({})", file.display()),
        ),
        Some(false) => check(
            "service",
            Level::Warn,
            format!(
                "{name} installed at {} but not loaded; `mulchie service install` loads it",
                file.display()
            ),
        ),
        None => check(
            "service",
            Level::Warn,
            format!("{name} installed, but the service manager could not be asked"),
        ),
    }
}

pub fn command(paths: &Paths, json_out: bool) -> i32 {
    let list = checks(paths, &Env::real());
    let failed = list.iter().any(|c| c.level == Level::Fail);
    if json_out {
        crate::commands::print_json(&serde_json::json!({ "ok": !failed, "checks": list }));
    } else {
        for c in &list {
            println!("{}  {:<11} {}", c.level.word(), c.name, c.detail);
        }
        if failed {
            println!("\nSome checks failed.");
        }
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env(dir: &Path) -> Env {
        Env {
            exe: dir.join("bin/mulchie"),
            on_path: None,
            git: "git".into(),
            platform: Platform::current(),
            service_manager: false,
        }
    }

    fn level(list: &[Check], name: &str) -> Level {
        list.iter().find(|c| c.name == name).unwrap().level
    }

    #[test]
    fn fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let list = checks(&paths, &env(dir.path()));
        assert_eq!(level(&list, "config"), Level::Pass);
        assert_eq!(level(&list, "config dir"), Level::Warn);
        assert_eq!(level(&list, "token"), Level::Warn);
        assert_eq!(level(&list, "engine"), Level::Warn);
        assert_eq!(level(&list, "path"), Level::Warn);
        assert_eq!(level(&list, "service"), Level::Info);
        assert!(!list.iter().any(|c| c.level == Level::Fail), "{list:#?}");
        // Read-only: nothing was created.
        assert!(!paths.config_dir.exists());
    }

    #[test]
    fn broken_config_and_permissions_fail() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::set_permissions(&paths.config_dir, std::fs::Permissions::from_mode(0o777))
            .unwrap();
        std::fs::write(paths.config_file(), br#"{"worktree_idle_days": 0}"#).unwrap();
        let tok = api::token_path_in(&paths);
        std::fs::write(&tok, "ab".repeat(32)).unwrap();
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o644)).unwrap();
        let list = checks(&paths, &env(dir.path()));
        assert_eq!(level(&list, "config"), Level::Fail);
        let cfg = list.iter().find(|c| c.name == "config").unwrap();
        assert!(cfg.detail.contains("worktree_idle_days"), "{}", cfg.detail);
        assert_eq!(level(&list, "config dir"), Level::Fail);
        assert_eq!(level(&list, "token"), Level::Fail);
        // Nothing was repaired.
        let mode = std::fs::metadata(&tok).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn healthy_private_setup_and_missing_git() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        api::load_or_create_token_in(&paths).unwrap();
        let mut e = env(dir.path());
        e.git = "/nonexistent/git-for-mulchie-tests".into();
        let list = checks(&paths, &e);
        assert_eq!(level(&list, "config dir"), Level::Pass);
        assert_eq!(level(&list, "token"), Level::Pass);
        assert_eq!(level(&list, "git"), Level::Fail);
    }

    #[test]
    fn running_engine_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let engine = mulchie_core::Engine::new(paths.clone(), Config::load_from(&paths));
        let mut lk = engine.acquire_lock().unwrap();
        let server = api::serve(engine.clone(), 0).unwrap();
        lk.set_port(Some(server.port())).unwrap();
        let list = checks(&paths, &env(dir.path()));
        let e = list.iter().find(|c| c.name == "engine").unwrap();
        // No sample yet: stale, but only a warning until the first sample.
        assert_eq!(e.level, Level::Warn, "{}", e.detail);
        assert!(e.detail.contains(&format!("port {}", server.port())));
        engine.set_snapshot(crate::commands::sample());
        let list = checks(&paths, &env(dir.path()));
        assert_eq!(level(&list, "engine"), Level::Pass);
        server.stop();
    }
}
