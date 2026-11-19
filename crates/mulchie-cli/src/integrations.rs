//! `mulchie integrations`: turn read-only links to Vercel, Sentry and
//! PostHog on and off ([`mulchie_core::integrations`]).
//!
//! The token is read from standard input (without echo on a terminal) and
//! written to `<config dir>/integrations/<provider>.token` by the core; it
//! is never an argument and never printed. The provider's settings go into
//! the config through the engine when one runs, else into the file under
//! the engine lock.

use crate::cli::{IntegrationsCommand, ProviderArg};
use crate::commands::{fail, print_json, s, u};
use crate::conn::{self, Conn};
use crate::observe::config_write_path;
use mulchie_core::integrations::{self as core, Provider, Secret};
use mulchie_core::{Config, Engine, Paths};
use serde_json::{json, Map, Value};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

pub fn command(paths: &Paths, c: IntegrationsCommand) -> i32 {
    match c {
        IntegrationsCommand::Add {
            provider,
            project,
            org,
            team,
            host,
            environment,
            release_property,
            workspace,
            out,
        } => {
            let fields = Fields {
                project,
                org,
                team,
                host,
                environment,
                release_property,
                workspace,
            };
            match add(
                paths,
                provider,
                &fields,
                &mut std::io::stdin(),
                std::io::stdin().is_terminal(),
            ) {
                Ok(v) => {
                    if out.json {
                        print_json(&v);
                    } else {
                        println!(
                            "{} is on ({}). The engine polls it every {} minutes; `mulchie integrations list` shows how it went.",
                            provider.core().title(),
                            s(&v, "saved"),
                            u(&v, "interval_mins")
                        );
                    }
                    0
                }
                Err(e) => fail(e),
            }
        }
        IntegrationsCommand::List { out } => list(paths, out.json),
        IntegrationsCommand::Remove { provider, out } => match remove(paths, provider) {
            Ok(v) => {
                if out.json {
                    print_json(&v);
                } else if v["token_deleted"] == true {
                    println!(
                        "{} is off and its token was deleted.",
                        provider.core().title()
                    );
                } else {
                    println!(
                        "{} is off; there was no saved token.",
                        provider.core().title()
                    );
                }
                0
            }
            Err(e) => fail(e),
        },
        IntegrationsCommand::Deploys { provider, out } => deploys(paths, provider, out.json),
        IntegrationsCommand::Check { out } => check(paths, out.json),
    }
}

/// The settings `add` takes, besides the token.
pub struct Fields {
    pub project: String,
    pub org: Option<String>,
    pub team: Option<String>,
    pub host: Option<String>,
    pub environment: Option<String>,
    pub release_property: Option<String>,
    pub workspace: Option<PathBuf>,
}

/// The config merge patch that turns `p` on with `f`.
pub fn add_patch(p: Provider, f: &Fields) -> Result<Value, String> {
    let mut m = Map::new();
    m.insert("enabled".into(), json!(true));
    m.insert("project".into(), json!(f.project));
    let only = |name: &str, given: bool, ok: bool| -> Result<(), String> {
        if given && !ok {
            return Err(format!("--{name} does not apply to {}", p.name()));
        }
        Ok(())
    };
    only("org", f.org.is_some(), p == Provider::Sentry)?;
    only("team", f.team.is_some(), p == Provider::Vercel)?;
    only("host", f.host.is_some(), p != Provider::Vercel)?;
    only(
        "environment",
        f.environment.is_some(),
        p == Provider::Sentry,
    )?;
    only(
        "release-property",
        f.release_property.is_some(),
        p == Provider::Posthog,
    )?;
    if p == Provider::Sentry && f.org.is_none() {
        return Err("sentry needs --org (the organization slug)".into());
    }
    for (k, v) in [
        ("org", &f.org),
        ("team", &f.team),
        ("host", &f.host),
        ("environment", &f.environment),
        ("release_property", &f.release_property),
    ] {
        if let Some(v) = v {
            m.insert(k.into(), json!(v));
        }
    }
    if let Some(w) = &f.workspace {
        let w =
            std::fs::canonicalize(w).map_err(|e| format!("--workspace {}: {e}", w.display()))?;
        m.insert("workspace".into(), json!(w));
    }
    Ok(json!({ "integrations": { p.name(): Value::Object(m) } }))
}

/// Read the token: a line without echo on a terminal, else all of `input`
/// (at most a few KB).
fn read_secret(input: &mut dyn Read, tty: bool) -> Result<Secret, String> {
    let mut buf = Vec::new();
    if tty {
        eprint!("Paste the token (it is not shown), then press Enter: ");
        let _ = std::io::stderr().flush();
        let _quiet = NoEcho::new();
        let mut byte = [0u8; 1];
        while buf.len() <= core::MAX_TOKEN + 2 {
            match input.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => buf.push(byte[0]),
                Err(e) => return Err(format!("could not read the token: {e}")),
            }
        }
        eprintln!();
    } else {
        input
            .take(core::MAX_TOKEN as u64 + 2)
            .read_to_end(&mut buf)
            .map_err(|e| format!("could not read the token: {e}"))?;
    }
    let text = String::from_utf8(buf).map_err(|_| "the token is not text".to_string())?;
    Secret::new(&text).map_err(|e| format!("{e}; pipe or paste the token on standard input"))
}

/// Echo off on standard input while it lives; put back on drop, whatever
/// way the read ends.
struct NoEcho(Option<libc::termios>);

impl NoEcho {
    fn new() -> Self {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return NoEcho(None);
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            t.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) != 0 {
                return NoEcho(None);
            }
            NoEcho(Some(saved))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            }
        }
    }
}

/// Save the token, then turn the provider on in the config. When the config
/// change is refused, a token saved by this call is deleted again.
pub fn add(
    paths: &Paths,
    p: ProviderArg,
    f: &Fields,
    input: &mut dyn Read,
    tty: bool,
) -> Result<Value, String> {
    let p = p.core();
    let patch = add_patch(p, f)?;
    // Check the settings against the file before asking for the token.
    let mut cur = serde_json::to_value(Config::load_from(paths).config).unwrap_or(Value::Null);
    mulchie_core::engine::merge_patch(&mut cur, &patch);
    let probe: Config = serde_json::from_value(cur).map_err(|e| e.to_string())?;
    if let Err(e) = probe.validate() {
        return Err(e.join("; "));
    }
    let token = read_secret(input, tty)?;
    let had = core::token_present(paths, p);
    core::save_token(paths, p, &token).map_err(|e| format!("could not save the token: {e}"))?;
    match patch_config(paths, &patch) {
        Ok((cfg, how)) => Ok(json!({
            "ok": true,
            "provider": p.name(),
            "saved": how,
            "interval_mins": cfg["integrations"]["interval_mins"],
            "settings": cfg["integrations"][p.name()],
        })),
        Err(e) => {
            if !had {
                let _ = core::remove_token(paths, p);
            }
            Err(e)
        }
    }
}

/// Turn `p` off and delete its token file.
pub fn remove(paths: &Paths, p: ProviderArg) -> Result<Value, String> {
    let p = p.core();
    let patch = json!({ "integrations": { p.name(): { "enabled": false } } });
    let (_, how) = patch_config(paths, &patch)?;
    let deleted = core::remove_token(paths, p)
        .map_err(|e| format!("{} is off, but its token was not deleted: {e}", p.title()))?;
    Ok(json!({ "ok": true, "provider": p.name(), "token_deleted": deleted, "saved": how }))
}

const LOCK_TRIES: usize = 15;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Apply a JSON merge patch to the config: through the engine when one
/// runs, else to the file under the engine lock. Returns the saved config
/// and where it went, in words.
fn patch_config(paths: &Paths, patch: &Value) -> Result<(Value, String), String> {
    for attempt in 0..LOCK_TRIES {
        if attempt > 0 {
            std::thread::sleep(LOCK_WAIT);
        }
        match conn::connect(paths) {
            Conn::Engine(c) => {
                let cur =
                    conn::call(&c, "GET", "/v1/config", None).map_err(|e| conn::describe(&e))?;
                if let Some(e) = cur["config_error"].as_str() {
                    return Err(format!("the config file was not used ({e}); fix it first"));
                }
                let path = config_write_path(cur["revision"].as_str());
                let v = match conn::call(&c, "PATCH", &path, Some(patch)) {
                    Err(e) if e.code == "config_changed" => {
                        conn::call(&c, "PATCH", "/v1/config", Some(patch))
                    }
                    r => r,
                }
                .map_err(|e| e.message.clone())?;
                return Ok((v["config"].clone(), "through the engine".into()));
            }
            Conn::NotRunning => {
                let engine = Engine::new(paths.clone(), Config::load_from(paths));
                let Ok(_lock) = engine.acquire_lock() else {
                    continue;
                };
                let loaded = Config::load_from(paths);
                if let Some(e) = loaded.error {
                    return Err(format!(
                        "{} was not used ({e}); fix or remove it first",
                        paths.config_file().display()
                    ));
                }
                let engine = Engine::new(paths.clone(), loaded);
                let cfg = engine.patch_config(patch).map_err(|e| e.to_string())?;
                return Ok((
                    serde_json::to_value(cfg).unwrap_or(Value::Null),
                    "in the file; it applies when the engine starts".into(),
                ));
            }
            Conn::Unreachable { reason, .. } if reason.contains("no API listener") => continue,
            other => return Err(other.explain_required("`mulchie integrations`")),
        }
    }
    Err("another process holds the engine lock; try again in a moment".into())
}

/// What `list` shows without an engine: the config, the token files and
/// the cached results.
fn local_status(paths: &Paths) -> Value {
    let cfg = Config::load_from(paths).config.integrations;
    let cache = core::load_cache(paths);
    let providers: Vec<Value> = Provider::ALL
        .into_iter()
        .map(|p| {
            let c = cache.providers.get(&p).cloned().unwrap_or_default();
            json!({
                "provider": p,
                "enabled": p.enabled(&cfg),
                "host": p.host(&cfg),
                "token": core::token_present(paths, p),
                "last_attempt_unix": c.last_attempt_unix,
                "last_success_unix": c.last_success_unix,
                "last_error": c.last_error,
                "failures": c.failures,
                "deploys": c.deploys.len(),
            })
        })
        .collect();
    json!({
        "network": core::network_built(),
        "interval_mins": cfg.interval_mins,
        "providers": providers,
        "engine": false,
    })
}

fn list(paths: &Paths, json_out: bool) -> i32 {
    let v = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "GET", "/v1/integrations", None) {
            Ok(v) => v,
            Err(e) => return fail(conn::describe(&e)),
        },
        _ => local_status(paths),
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    if v["network"] == false {
        println!("This build of Mulchie has no network support: no provider is polled.");
    }
    for p in v["providers"].as_array().into_iter().flatten() {
        let name = s(p, "provider");
        let state = if p["enabled"] == true { "on" } else { "off" };
        let token = if p["token"] == true {
            "token saved"
        } else {
            "no token"
        };
        let mut line = format!("{name:<8} {state:<3}  {token}");
        if p["enabled"] == true {
            line.push_str(&format!("  {}", s(p, "host")));
            if let Some(proj) = p["project"].as_str().filter(|x| !x.is_empty()) {
                line.push_str(&format!("  {proj}"));
            }
            line.push_str(&format!("  {} deploys", u(p, "deploys")));
        }
        println!("{line}");
        if let Some(e) = p["last_error"].as_str() {
            println!("         last poll failed: {e}");
        }
    }
    if v["engine"] == false {
        println!("(No engine is running: this is the last saved state; nothing is polled until one starts.)");
    }
    0
}

fn deploys(paths: &Paths, provider: Option<ProviderArg>, json_out: bool) -> i32 {
    let Conn::Engine(c) = conn::connect(paths) else {
        return fail(conn::connect(paths).explain_required("`mulchie integrations deploys`"));
    };
    let path = match provider {
        Some(p) => format!("/v1/deploys?provider={}", p.core().name()),
        None => "/v1/deploys".into(),
    };
    let v = match conn::call(&c, "GET", &path, None) {
        Ok(v) => v,
        Err(e) => return fail(conn::describe(&e)),
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    let list = v["deploys"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No deploys yet (`mulchie integrations list` shows each provider's last poll).");
    }
    for d in list {
        let sha: String = s(&d, "sha").chars().take(7).collect();
        let metrics: Vec<String> = d["metrics"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                format!(
                    "{} {}",
                    s(m, "metric"),
                    core::human_value(m["value_ms"].as_f64().unwrap_or(0.0))
                )
            })
            .collect();
        let who: Vec<String> = d["commits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| match (c["agent"].as_str(), s(c, "made")) {
                (Some(a), _) => a.to_string(),
                (None, "made_elsewhere") => "made elsewhere".into(),
                _ => "no session".into(),
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        println!(
            "{:<8} {:<28} {:<7} {}  {}",
            s(&d, "provider"),
            crate::commands::truncate(s(&d, "id"), 28),
            if sha.is_empty() { "-".into() } else { sha },
            if metrics.is_empty() {
                "-".into()
            } else {
                metrics.join(", ")
            },
            who.join(", ")
        );
    }
    0
}

fn check(paths: &Paths, json_out: bool) -> i32 {
    let Conn::Engine(c) = conn::connect(paths) else {
        return fail(conn::connect(paths).explain_required("`mulchie integrations check`"));
    };
    match conn::call(&c, "POST", "/v1/integrations/check", None) {
        Ok(v) => {
            if json_out {
                print_json(&v);
            } else {
                println!("The engine is polling now; run `mulchie integrations list` in a minute.");
            }
            0
        }
        Err(e) => fail(e.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(project: &str) -> Fields {
        Fields {
            project: project.into(),
            org: None,
            team: None,
            host: None,
            environment: None,
            release_property: None,
            workspace: None,
        }
    }

    fn home() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        std::fs::create_dir_all(&p.home).unwrap();
        (t, p)
    }

    /// A token can never be passed in argv: no `integrations` command takes
    /// an argument named like one.
    #[test]
    fn no_command_takes_a_token_argument() {
        use clap::CommandFactory;
        let cli = crate::cli::Cli::command();
        let ig = cli.find_subcommand("integrations").unwrap();
        for sub in ig.get_subcommands() {
            for a in sub.get_arguments() {
                let id = a.get_id().as_str().to_ascii_lowercase();
                for word in ["token", "key", "secret", "password", "auth"] {
                    assert!(!id.contains(word), "{} --{id}", sub.get_name());
                }
            }
        }
    }

    #[test]
    fn options_that_do_not_apply_are_refused() {
        let mut f = fields("web");
        f.team = Some("t".into());
        assert!(add_patch(Provider::Sentry, &f).is_err());
        assert!(
            add_patch(Provider::Sentry, &fields("web")).is_err(),
            "needs --org"
        );
        let mut f = fields("web");
        f.org = Some("acme".into());
        let v = add_patch(Provider::Sentry, &f).unwrap();
        assert_eq!(v["integrations"]["sentry"]["enabled"], true);
        assert_eq!(v["integrations"]["sentry"]["org"], "acme");
    }

    /// `add` without an engine: the token goes to its private file, the
    /// settings to the config, and neither the config nor the answer holds
    /// the token. `remove` turns it off and deletes the file.
    #[test]
    fn add_and_remove_without_an_engine() {
        let (_t, p) = home();
        // No engine may answer for this temp home.
        let mut f = fields("prj_1");
        f.team = Some("team_2".into());
        let token = "vc_SENTINEL_token_123";
        let v = add(&p, ProviderArg::Vercel, &f, &mut token.as_bytes(), false).unwrap();
        assert!(!v.to_string().contains(token));
        let cfg = Config::load_from(&p).config.integrations;
        assert!(cfg.vercel.enabled);
        assert_eq!(cfg.vercel.team, "team_2");
        let raw = std::fs::read_to_string(p.config_file()).unwrap();
        assert!(!raw.contains(token));
        assert_eq!(
            core::read_token(&p, Provider::Vercel).unwrap().expose(),
            token
        );
        let v = remove(&p, ProviderArg::Vercel).unwrap();
        assert_eq!(v["token_deleted"], true);
        assert!(!core::token_present(&p, Provider::Vercel));
        assert!(!Config::load_from(&p).config.integrations.vercel.enabled);
    }

    #[test]
    fn bad_settings_or_an_empty_token_save_nothing() {
        let (_t, p) = home();
        let mut f = fields("../etc");
        f.team = None;
        assert!(add(&p, ProviderArg::Vercel, &f, &mut "tok".as_bytes(), false).is_err());
        assert!(!core::token_present(&p, Provider::Vercel));
        assert!(add(
            &p,
            ProviderArg::Vercel,
            &fields("prj"),
            &mut "".as_bytes(),
            false
        )
        .is_err());
        assert!(!core::token_present(&p, Provider::Vercel));
        assert!(!Config::load_from(&p).config.integrations.vercel.enabled);
    }
}
