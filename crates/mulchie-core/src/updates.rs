//! Update and advisory checks. Findings only: nothing is ever installed or
//! upgraded; each finding names the exact command a person can run.
//!
//! Sources:
//! - Mulchie: the latest GitHub release of `williamrogersdev/mulchie`.
//! - Agent tools: the installed version from the tool's own `--version`
//!   (10 s timeout); the latest from the npm registry's dist-tags for tools
//!   published there (Claude Code, Codex, Gemini CLI). Tools with no
//!   documented source of a latest version (`cursor-agent`) are listed with
//!   their installed version only.
//! - Homebrew: `brew outdated --json=v2` with auto-update, hints and
//!   analytics off, run only when Homebrew's local API cache exists, so it
//!   reads local data.
//! - Advisories (opt-in, `updates.advisories`): package names and versions
//!   from the `Cargo.lock`, `package-lock.json` (v1, v2, v3) and `go.mod` at
//!   the root of each workspace in use, sent to OSV.dev `querybatch` in
//!   bounded batches (with its per-query pagination), then the details of
//!   each advisory found. One finding per workspace and advisory, with the
//!   fixed version.
//!
//! Every request is HTTPS with certificate verification, a timeout, a size
//! cap and `User-Agent: mulchie/<version>`, behind the [`Http`] trait (tests
//! use a fake). Versions are compared as semver; a version that does not
//! parse never produces an "outdated" claim. Parsing never panics on what a
//! server or a lockfile holds.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::UpdatesConfig;
use crate::consumers::Severity;
use crate::watch::{Evidence, Finding, FindingCategory, FindingKind, Suggestion};

pub const USER_AGENT: &str = concat!("mulchie/", env!("CARGO_PKG_VERSION"));
pub const MULCHIE_RELEASE_URL: &str =
    "https://api.github.com/repos/williamrogersdev/mulchie/releases/latest";
pub const MULCHIE_RELEASES_PAGE: &str =
    "https://github.com/williamrogersdev/mulchie/releases/latest";
pub const OSV_BATCH_URL: &str = "https://api.osv.dev/v1/querybatch";
pub const OSV_VULN_URL: &str = "https://api.osv.dev/v1/vulns/";
const NPM_DIST_TAGS: &str = "https://registry.npmjs.org/-/package/";

/// Every request: connect, send and read within this.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const BREW_TIMEOUT: Duration = Duration::from_secs(60);
/// Response size caps.
const SMALL_REPLY: usize = 256 * 1024;
const OSV_BATCH_REPLY: usize = 16 << 20;
const OSV_VULN_REPLY: usize = 2 << 20;
const BREW_REPLY: usize = 8 << 20;
const LOCKFILE_MAX: u64 = 32 << 20;
/// OSV querybatch: queries per request, pages followed per query, advisory
/// details fetched per run, packages per run, workspaces per run.
pub const OSV_BATCH: usize = 1000;
pub const OSV_MAX_PAGES: usize = 10;
pub const OSV_MAX_DETAILS: usize = 200;
pub const MAX_PACKAGES: usize = 20_000;
pub const MAX_WORKSPACES: usize = 50;
/// The first automatic check waits this long after the engine starts.
pub const FIRST_CHECK_DELAY_SECS: u64 = 300;
/// A failed check is retried after this, doubling up to the interval.
pub const RETRY_BASE_SECS: u64 = 900;

// ------------------------------------------------------------------ http

#[derive(Debug, Clone, PartialEq)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Outbound HTTPS. The real one is [`Https`]; tests use a fake.
pub trait Http: Send + Sync {
    fn get(&self, url: &str, max_bytes: usize) -> Result<HttpReply, String>;
    fn post_json(&self, url: &str, body: &[u8], max_bytes: usize) -> Result<HttpReply, String>;
}

/// A build without the `network` feature: every request fails, nothing
/// leaves the machine.
pub struct NoNetwork;

impl Http for NoNetwork {
    fn get(&self, _: &str, _: usize) -> Result<HttpReply, String> {
        Err("this build of Mulchie has no network support".into())
    }
    fn post_json(&self, _: &str, _: &[u8], _: usize) -> Result<HttpReply, String> {
        Err("this build of Mulchie has no network support".into())
    }
}

/// HTTPS through ureq with rustls and the webpki roots: certificates are
/// verified, plain HTTP is refused, at most 3 redirects.
#[cfg(feature = "network")]
pub struct Https {
    agent: ureq::Agent,
}

#[cfg(feature = "network")]
impl Https {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .https_only(true)
            .max_redirects(3)
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: config.into(),
        }
    }

    fn read(
        r: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        max: usize,
    ) -> Result<HttpReply, String> {
        let mut r = r.map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let body = r
            .body_mut()
            .with_config()
            .limit(max as u64)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok(HttpReply { status, body })
    }
}

#[cfg(feature = "network")]
impl Default for Https {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "network")]
impl Http for Https {
    fn get(&self, url: &str, max: usize) -> Result<HttpReply, String> {
        Self::read(
            self.agent
                .get(url)
                .header("Accept", "application/json")
                .call(),
            max,
        )
    }
    fn post_json(&self, url: &str, body: &[u8], max: usize) -> Result<HttpReply, String> {
        Self::read(
            self.agent
                .post(url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send(body),
            max,
        )
    }
}

/// The HTTPS client of this build.
pub fn default_http() -> std::sync::Arc<dyn Http> {
    #[cfg(feature = "network")]
    {
        std::sync::Arc::new(Https::new())
    }
    #[cfg(not(feature = "network"))]
    {
        std::sync::Arc::new(NoNetwork)
    }
}

/// GET returning JSON from a 200 reply.
fn get_json(http: &dyn Http, url: &str, max: usize) -> Result<Value, String> {
    let r = http.get(url, max)?;
    if r.status != 200 {
        return Err(format!("HTTP {} from {}", r.status, host_of(url)));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("bad JSON from {}: {e}", host_of(url)))
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(url)
}

// --------------------------------------------------------------- commands

/// Running local commands (version commands, `brew`). Tests use a fake.
pub trait Runner: Send + Sync {
    /// The executable `name` on this machine, if any.
    fn find(&self, name: &str) -> Option<PathBuf>;
    /// Run it with a timeout; `Some((exited 0, stdout))`, `None` when it
    /// could not run or timed out.
    fn run(
        &self,
        prog: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        timeout: Duration,
        max_stdout: usize,
    ) -> Option<(bool, String)>;
}

/// The real runner: `PATH` plus the usual install places (the engine may
/// run as a service with a short `PATH`).
pub struct SystemRunner {
    pub home: PathBuf,
}

impl Runner for SystemRunner {
    fn find(&self, name: &str) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        for d in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/usr/bin",
        ] {
            dirs.push(PathBuf::from(d));
        }
        for d in [".local/bin", ".cargo/bin", ".npm-global/bin", ".bun/bin"] {
            dirs.push(self.home.join(d));
        }
        dirs.into_iter().map(|d| d.join(name)).find(|p| {
            std::fs::metadata(p).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
    }

    fn run(
        &self,
        prog: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        timeout: Duration,
        max_stdout: usize,
    ) -> Option<(bool, String)> {
        let mut cmd = std::process::Command::new(prog);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .env("NO_COLOR", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let o = crate::git::run_capped(cmd, timeout, max_stdout).ok()?;
        Some((o.success, String::from_utf8_lossy(&o.stdout).into_owned()))
    }
}

/// Runs nothing and finds nothing.
pub struct NoCommands;

impl Runner for NoCommands {
    fn find(&self, _: &str) -> Option<PathBuf> {
        None
    }
    fn run(
        &self,
        _: &Path,
        _: &[&str],
        _: &[(&str, &str)],
        _: Duration,
        _: usize,
    ) -> Option<(bool, String)> {
        None
    }
}

// --------------------------------------------------------------- versions

/// A semver version, tolerating a leading `v`. `None` for anything else.
pub fn parse_version(s: &str) -> Option<semver::Version> {
    let s = s.trim();
    let s = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    semver::Version::parse(s).ok()
}

/// The first semver version among the words of a `--version` output
/// (`2.0.14 (Claude Code)`, `codex-cli 0.46.0`).
pub fn version_in(text: &str) -> Option<semver::Version> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '"' | '\''))
        .take(64)
        .find_map(parse_version)
}

/// Semver precedence (build metadata ignored).
fn precedence(a: &semver::Version, b: &semver::Version) -> std::cmp::Ordering {
    (a.major, a.minor, a.patch)
        .cmp(&(b.major, b.minor, b.patch))
        .then_with(|| a.pre.cmp(&b.pre))
}

/// `Some(true)` when `latest` is newer than `installed`; `None` when either
/// is not semver (never a false "outdated").
pub fn newer(latest: &str, installed: &str) -> Option<bool> {
    let (l, i) = (parse_version(latest)?, parse_version(installed)?);
    Some(precedence(&l, &i) == std::cmp::Ordering::Greater)
}

// -------------------------------------------------------------- lockfiles

/// One package version from a lockfile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pkg {
    /// OSV ecosystem: `crates.io`, `npm` or `Go`.
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    /// A direct dependency of the workspace, when the lockfile says.
    pub direct: bool,
}

fn pkg(eco: &str, name: &str, version: &str, direct: bool) -> Pkg {
    Pkg {
        ecosystem: eco.into(),
        name: name.into(),
        version: version.into(),
        direct,
    }
}

/// `Cargo.lock` (v1 to v4): every `[[package]]` from a registry. Path and
/// git dependencies have no published advisories to match.
pub fn parse_cargo_lock(text: &str) -> Result<Vec<Pkg>, String> {
    #[derive(Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<P>,
    }
    #[derive(Deserialize)]
    struct P {
        name: String,
        version: String,
        #[serde(default)]
        source: Option<String>,
    }
    let lock: Lock = toml::from_str(text).map_err(|e| format!("Cargo.lock: {e}"))?;
    Ok(lock
        .package
        .into_iter()
        .filter(|p| {
            p.source
                .as_deref()
                .is_some_and(|s| s.starts_with("registry+") || s.starts_with("sparse+"))
        })
        .map(|p| pkg("crates.io", &p.name, &p.version, false))
        .collect())
}

/// `package-lock.json`: v2 and v3 `packages` (keys `node_modules/…`, the
/// root `""` lists the direct dependencies), else v1 `dependencies`
/// (nested). Linked and non-registry entries are skipped.
pub fn parse_package_lock(text: &str) -> Result<Vec<Pkg>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("package-lock.json: {e}"))?;
    let mut out = Vec::new();
    if let Some(pkgs) = v.get("packages").and_then(Value::as_object) {
        let root = pkgs.get("").cloned().unwrap_or(Value::Null);
        let direct: BTreeSet<&str> = [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ]
        .iter()
        .filter_map(|k| root.get(*k).and_then(Value::as_object))
        .flat_map(|m| m.keys().map(String::as_str))
        .collect();
        for (key, p) in pkgs {
            let Some(at) = key.rfind("node_modules/") else {
                continue;
            };
            if p.get("link").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&key[at + "node_modules/".len()..]);
            let Some(ver) = p.get("version").and_then(Value::as_str) else {
                continue;
            };
            if name.is_empty() || parse_version(ver).is_none() {
                continue;
            }
            let top = key == &format!("node_modules/{name}");
            out.push(pkg("npm", name, ver, top && direct.contains(name)));
        }
        return Ok(out);
    }
    fn walk(m: &serde_json::Map<String, Value>, out: &mut Vec<Pkg>, depth: usize) {
        if depth > 64 {
            return;
        }
        for (name, p) in m {
            if let Some(ver) = p.get("version").and_then(Value::as_str) {
                if parse_version(ver).is_some() {
                    out.push(pkg("npm", name, ver, false));
                }
            }
            if let Some(d) = p.get("dependencies").and_then(Value::as_object) {
                walk(d, out, depth + 1);
            }
        }
    }
    if let Some(d) = v.get("dependencies").and_then(Value::as_object) {
        walk(d, &mut out, 0);
        return Ok(out);
    }
    Err("package-lock.json: neither packages nor dependencies".into())
}

/// `go.mod`: `require` lines and blocks; modules a `replace` names are
/// skipped (the version used is not the one required). OSV's Go ranges are
/// semver without the `v`, so it is dropped (and `+incompatible` with it).
pub fn parse_go_mod(text: &str) -> Vec<Pkg> {
    let mut reqs: Vec<(String, String, bool)> = Vec::new();
    let mut replaced: BTreeSet<String> = BTreeSet::new();
    let mut block: Option<&str> = None;
    for raw in text.lines() {
        let indirect = raw.contains("// indirect");
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if block.is_some() && line == ")" {
            block = None;
            continue;
        }
        let (verb, rest) = match block {
            Some(b) => (b, line),
            None => {
                let mut it = line.splitn(2, char::is_whitespace);
                let verb = it.next().unwrap_or("");
                let rest = it.next().unwrap_or("").trim();
                if rest == "(" {
                    block = match verb {
                        "require" => Some("require"),
                        "replace" => Some("replace"),
                        _ => Some("other"),
                    };
                    continue;
                }
                (verb, rest)
            }
        };
        match verb {
            "require" => {
                let w: Vec<&str> = rest.split_whitespace().collect();
                if w.len() >= 2 {
                    reqs.push((
                        w[0].trim_matches('"').to_string(),
                        w[1].to_string(),
                        indirect,
                    ));
                }
            }
            "replace" => {
                if let Some(left) = rest.split("=>").next() {
                    if let Some(m) = left.split_whitespace().next() {
                        replaced.insert(m.trim_matches('"').to_string());
                    }
                }
            }
            _ => {}
        }
    }
    reqs.into_iter()
        .filter(|(m, _, _)| !replaced.contains(m))
        .filter_map(|(m, v, ind)| {
            let v = v.strip_prefix('v')?;
            let v = v.strip_suffix("+incompatible").unwrap_or(v);
            parse_version(v)?;
            Some(pkg("Go", &m, v, !ind))
        })
        .collect()
}

/// The packages of the lockfiles at `ws`'s root, and why any could not be
/// read.
pub fn workspace_packages(ws: &Path) -> (Vec<Pkg>, Vec<String>) {
    let mut out = Vec::new();
    let mut errs = Vec::new();
    let read = |name: &str| -> Option<Result<String, String>> {
        let p = ws.join(name);
        let m = std::fs::metadata(&p).ok()?;
        if !m.is_file() {
            return None;
        }
        if m.len() > LOCKFILE_MAX {
            return Some(Err(format!(
                "{} is larger than 32 MB; skipped",
                p.display()
            )));
        }
        Some(std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display())))
    };
    let mut take = |r: Result<Vec<Pkg>, String>, errs: &mut Vec<String>| match r {
        Ok(v) => out.extend(v),
        Err(e) => errs.push(format!("{}: {e}", ws.display())),
    };
    if let Some(t) = read("Cargo.lock") {
        take(t.and_then(|t| parse_cargo_lock(&t)), &mut errs);
    }
    if let Some(t) = read("package-lock.json") {
        take(t.and_then(|t| parse_package_lock(&t)), &mut errs);
    }
    if let Some(t) = read("go.mod") {
        take(t.map(|t| parse_go_mod(&t)), &mut errs);
    }
    out.sort();
    out.dedup_by(|a, b| a.ecosystem == b.ecosystem && a.name == b.name && a.version == b.version);
    (out, errs)
}

// -------------------------------------------------------------------- OSV

/// An advisory id is used in a URL path: only these characters.
fn valid_vuln_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// The advisory ids of each package, in order, through `querybatch`: at
/// most [`OSV_BATCH`] queries per request, each query's `next_page_token`
/// followed up to [`OSV_MAX_PAGES`] times.
pub fn osv_query(http: &dyn Http, pkgs: &[Pkg]) -> Result<Vec<Vec<String>>, String> {
    let mut out: Vec<Vec<String>> = vec![Vec::new(); pkgs.len()];
    for (chunk_no, chunk) in pkgs.chunks(OSV_BATCH).enumerate() {
        let base = chunk_no * OSV_BATCH;
        // (index in chunk, page token)
        let mut pending: Vec<(usize, Option<String>)> =
            (0..chunk.len()).map(|i| (i, None)).collect();
        let mut pages = 0;
        while !pending.is_empty() {
            if pages >= OSV_MAX_PAGES {
                return Err(format!(
                    "OSV kept paginating after {OSV_MAX_PAGES} pages; results are incomplete"
                ));
            }
            pages += 1;
            let queries: Vec<Value> = pending
                .iter()
                .map(|(i, tok)| {
                    let p = &chunk[*i];
                    let mut q = json!({
                        "package": { "name": p.name, "ecosystem": p.ecosystem },
                        "version": p.version,
                    });
                    if let Some(t) = tok {
                        q["page_token"] = json!(t);
                    }
                    q
                })
                .collect();
            let body =
                serde_json::to_vec(&json!({ "queries": queries })).map_err(|e| e.to_string())?;
            let r = http.post_json(OSV_BATCH_URL, &body, OSV_BATCH_REPLY)?;
            if r.status != 200 {
                return Err(format!("HTTP {} from api.osv.dev", r.status));
            }
            let v: Value = serde_json::from_slice(&r.body)
                .map_err(|e| format!("bad JSON from api.osv.dev: {e}"))?;
            let results = v
                .get("results")
                .and_then(Value::as_array)
                .ok_or("api.osv.dev: no results array")?;
            if results.len() != pending.len() {
                return Err(format!(
                    "api.osv.dev answered {} results for {} queries",
                    results.len(),
                    pending.len()
                ));
            }
            let mut next = Vec::new();
            for ((i, _), res) in pending.iter().zip(results) {
                if let Some(vs) = res.get("vulns").and_then(Value::as_array) {
                    for id in vs
                        .iter()
                        .filter_map(|x| x.get("id").and_then(Value::as_str))
                    {
                        if valid_vuln_id(id) && !out[base + i].iter().any(|x| x == id) {
                            out[base + i].push(id.to_string());
                        }
                    }
                }
                if let Some(t) = res
                    .get("next_page_token")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    next.push((*i, Some(t.to_string())));
                }
            }
            pending = next;
        }
    }
    Ok(out)
}

/// What a finding needs from one advisory.
#[derive(Debug, Clone, PartialEq)]
pub struct Vuln {
    pub id: String,
    pub summary: String,
    pub severity: Severity,
    pub aliases: Vec<String>,
    /// (ecosystem, package name) → fixed versions.
    pub fixed: BTreeMap<(String, String), Vec<String>>,
}

fn severity_word(s: &str) -> Option<Severity> {
    match s.to_ascii_uppercase().as_str() {
        "CRITICAL" | "HIGH" => Some(Severity::High),
        "MODERATE" | "MEDIUM" => Some(Severity::Medium),
        "LOW" => Some(Severity::Low),
        _ => None,
    }
}

/// Parse `GET /v1/vulns/{id}`. Missing fields are tolerated.
pub fn parse_vuln(v: &Value) -> Result<Vuln, String> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .filter(|i| valid_vuln_id(i))
        .ok_or("advisory without a valid id")?
        .to_string();
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| v.get("details").and_then(Value::as_str))
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    let mut severity = v
        .pointer("/database_specific/severity")
        .and_then(Value::as_str)
        .and_then(severity_word);
    let mut fixed: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for a in v
        .get("affected")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(eco), Some(name)) = (
            a.pointer("/package/ecosystem").and_then(Value::as_str),
            a.pointer("/package/name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if severity.is_none() {
            severity = a
                .pointer("/ecosystem_specific/severity")
                .and_then(Value::as_str)
                .and_then(severity_word);
        }
        let e = fixed
            .entry((eco.to_string(), name.to_string()))
            .or_default();
        for r in a
            .get("ranges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for ev in r
                .get("events")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(f) = ev.get("fixed").and_then(Value::as_str) {
                    if !e.iter().any(|x| x == f) && f.len() <= 64 {
                        e.push(f.to_string());
                    }
                }
            }
        }
    }
    let aliases = v
        .get("aliases")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|s| valid_vuln_id(s))
                .take(8)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Ok(Vuln {
        id,
        summary,
        severity: severity.unwrap_or(Severity::Medium),
        aliases,
        fixed,
    })
}

/// The lowest fixed version above `p`'s, when one parses.
pub fn fixed_for(v: &Vuln, p: &Pkg) -> Option<String> {
    let installed = parse_version(&p.version)?;
    v.fixed
        .get(&(p.ecosystem.clone(), p.name.clone()))?
        .iter()
        .filter_map(|f| parse_version(f).map(|pv| (pv, f)))
        .filter(|(pv, _)| precedence(pv, &installed) == std::cmp::Ordering::Greater)
        .min_by(|a, b| precedence(&a.0, &b.0))
        .map(|(_, f)| f.trim_start_matches('v').to_string())
}

/// The command that moves `p` to `fixed` in `ws`.
pub fn advisory_command(p: &Pkg, fixed: Option<&str>, ws: &Path) -> String {
    let at = format!("in {}", ws.display());
    match (p.ecosystem.as_str(), fixed) {
        ("crates.io", Some(f)) => format!(
            "cargo update -p {}@{} --precise {f} ({at})",
            p.name, p.version
        ),
        ("crates.io", None) => format!("cargo update -p {}@{} ({at})", p.name, p.version),
        ("npm", Some(f)) if p.direct => format!("npm install {}@{f} ({at})", p.name),
        ("npm", _) => format!("npm audit fix ({at})"),
        ("Go", Some(f)) => format!("go get {}@v{f} && go mod tidy ({at})", p.name),
        ("Go", None) => format!("go get -u {} && go mod tidy ({at})", p.name),
        _ => format!("update {} {at}", p.name),
    }
}

// ---------------------------------------------------------------- report

/// One tool and its versions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolVersion {
    pub name: String,
    pub path: Option<PathBuf>,
    pub installed: Option<String>,
    pub latest: Option<String>,
    /// Installed and latest both parse as semver and latest is newer.
    pub outdated: bool,
    /// What updates it, when `outdated`.
    pub command: Option<String>,
    /// Where `latest` came from (`github`, `npm`), or why there is none.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrewOutdated {
    pub name: String,
    pub installed: String,
    pub current: String,
    pub cask: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Advisory {
    pub workspace: PathBuf,
    pub id: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub severity: Severity,
    /// The affected packages of the workspace: `name version` each.
    pub packages: Vec<Pkg>,
    pub fixed: Vec<Option<String>>,
    pub commands: Vec<String>,
}

/// The result of one check. A source that failed keeps its previous result
/// and says why in `errors`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Report {
    pub checked_unix: u64,
    pub mulchie: Option<ToolVersion>,
    pub agents: Vec<ToolVersion>,
    /// `None`: not checked (off, no brew, or no local data).
    pub homebrew: Option<Vec<BrewOutdated>>,
    /// `None`: advisories are off.
    pub advisories: Option<Vec<Advisory>>,
    /// Packages sent to OSV in the last advisory check.
    pub packages_checked: usize,
    /// `source: why`, for every source that failed.
    pub errors: Vec<String>,
}

/// Agent tools: (name, executable, npm package with the latest version,
/// self-update command when the tool documents one).
const AGENT_TOOLS: &[(&str, &str, Option<&str>, Option<&str>)] = &[
    (
        "claude",
        "claude",
        Some("@anthropic-ai/claude-code"),
        Some("claude update"),
    ),
    ("codex", "codex", Some("@openai/codex"), None),
    ("gemini", "gemini", Some("@google/gemini-cli"), None),
    ("cursor-agent", "cursor-agent", None, None),
];

/// How a binary at `path` was installed, as the update command.
fn brew_command(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let after = |m: &str| {
        s.find(m)
            .map(|i| &s[i + m.len()..])
            .and_then(|r| r.split('/').next())
            .filter(|n| !n.is_empty())
            .map(String::from)
    };
    if let Some(c) = after("/Caskroom/") {
        return Some(format!("brew upgrade --cask {c}"));
    }
    after("/Cellar/").map(|f| format!("brew upgrade {f}"))
}

fn mulchie_command(exe: Option<&Path>, home: &Path) -> String {
    let Some(exe) = exe else {
        return format!("Download it from {MULCHIE_RELEASES_PAGE}");
    };
    let s = exe.to_string_lossy();
    if s.contains("/Caskroom/mulchie/") || s.contains("/Mulchie.app/") {
        "brew upgrade --cask williamrogersdev/tap/mulchie".into()
    } else if s.contains("/Cellar/mulchie-cli/") {
        "brew upgrade williamrogersdev/tap/mulchie-cli".into()
    } else if exe.starts_with(home.join(".cargo/bin")) {
        "cargo install mulchie".into()
    } else if exe.starts_with(home.join(".local/bin")) {
        "curl -fsSL https://mulchie.com/install.sh | sh".into()
    } else {
        format!("Download it from {MULCHIE_RELEASES_PAGE}")
    }
}

/// Everything a check needs besides the network and the runner.
pub struct CheckInput<'a> {
    pub cfg: &'a UpdatesConfig,
    pub home: &'a Path,
    /// This binary (for the Mulchie update command).
    pub exe: Option<&'a Path>,
    pub version: &'a str,
    /// Workspaces whose lockfiles are checked for advisories.
    pub workspaces: &'a [PathBuf],
    /// Homebrew's local API cache, which must exist for `brew outdated`.
    pub brew_cache: Option<PathBuf>,
    pub now: u64,
}

/// Homebrew's local API cache.
pub fn brew_api_cache(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/Homebrew/api/formula.jws.json")
    } else {
        home.join(".cache/Homebrew/api/formula.jws.json")
    }
}

/// Run every enabled check. `prev` supplies results for sources that fail.
pub fn check(http: &dyn Http, run: &dyn Runner, inp: &CheckInput, prev: Option<&Report>) -> Report {
    let mut r = Report {
        checked_unix: inp.now,
        ..Report::default()
    };
    if inp.cfg.check == crate::config::UpdateCheck::Off {
        return r;
    }
    if inp.cfg.mulchie {
        match get_json(http, MULCHIE_RELEASE_URL, SMALL_REPLY) {
            Ok(v) => {
                let latest = v.get("tag_name").and_then(Value::as_str).map(|t| {
                    t.trim_start_matches('v')
                        .chars()
                        .take(64)
                        .collect::<String>()
                });
                let outdated = latest
                    .as_deref()
                    .and_then(|l| newer(l, inp.version))
                    .unwrap_or(false);
                r.mulchie = Some(ToolVersion {
                    name: "mulchie".into(),
                    path: inp.exe.map(Path::to_path_buf),
                    installed: Some(inp.version.to_string()),
                    latest,
                    outdated,
                    command: outdated.then(|| mulchie_command(inp.exe, inp.home)),
                    source: "github".into(),
                });
            }
            Err(e) => {
                r.errors.push(format!("mulchie: {e}"));
                r.mulchie = prev.and_then(|p| p.mulchie.clone());
            }
        }
    }
    if inp.cfg.agents {
        for (name, exe, npm, self_update) in AGENT_TOOLS {
            let Some(path) = run.find(exe) else { continue };
            let installed = run
                .run(&path, &["--version"], &[], VERSION_TIMEOUT, 64 * 1024)
                .filter(|(ok, _)| *ok)
                .and_then(|(_, out)| version_in(&out))
                .map(|v| v.to_string());
            let real = std::fs::canonicalize(&path).unwrap_or(path.clone());
            let (latest, source) = match npm {
                Some(pkgname) => {
                    let url = format!("{NPM_DIST_TAGS}{}/dist-tags", pkgname.replace('/', "%2f"));
                    match get_json(http, &url, SMALL_REPLY) {
                        Ok(v) => (
                            v.get("latest")
                                .and_then(Value::as_str)
                                .map(|s| s.chars().take(64).collect()),
                            "npm".to_string(),
                        ),
                        Err(e) => {
                            r.errors.push(format!("{name}: {e}"));
                            let old = prev
                                .and_then(|p| p.agents.iter().find(|a| a.name == *name))
                                .and_then(|a| a.latest.clone());
                            (old, "npm".to_string())
                        }
                    }
                }
                None => (
                    None,
                    "no documented source of the latest version".to_string(),
                ),
            };
            let outdated = match (&latest, &installed) {
                (Some(l), Some(i)) => newer(l, i).unwrap_or(false),
                _ => false,
            };
            let command = outdated.then(|| {
                brew_command(&real)
                    .or_else(|| self_update.map(String::from))
                    .or_else(|| npm.map(|p| format!("npm install -g {p}@latest")))
                    .unwrap_or_default()
            });
            r.agents.push(ToolVersion {
                name: name.to_string(),
                path: Some(path),
                installed,
                latest,
                outdated,
                command,
                source,
            });
        }
    }
    if inp.cfg.homebrew {
        r.homebrew = homebrew(run, inp.brew_cache.as_deref(), &mut r.errors)
            .or_else(|| prev.and_then(|p| p.homebrew.clone()));
    }
    if inp.cfg.advisories {
        match advisories(http, inp.workspaces) {
            Ok((a, n, errs)) => {
                r.advisories = Some(a);
                r.packages_checked = n;
                r.errors.extend(errs);
            }
            Err(e) => {
                r.errors.push(format!("advisories: {e}"));
                r.advisories = prev.and_then(|p| p.advisories.clone());
                r.packages_checked = prev.map(|p| p.packages_checked).unwrap_or(0);
            }
        }
    }
    r
}

fn homebrew(
    run: &dyn Runner,
    cache: Option<&Path>,
    errors: &mut Vec<String>,
) -> Option<Vec<BrewOutdated>> {
    let brew = run.find("brew")?;
    if !cache.is_some_and(|c| c.is_file()) {
        // Without local data `brew outdated` would fetch it.
        return None;
    }
    let env = [
        ("HOMEBREW_NO_AUTO_UPDATE", "1"),
        ("HOMEBREW_NO_ENV_HINTS", "1"),
        ("HOMEBREW_NO_ANALYTICS", "1"),
    ];
    // `brew outdated` may exit non-zero when something is outdated: read
    // the JSON either way.
    let Some((_, out)) = run.run(
        &brew,
        &["outdated", "--json=v2"],
        &env,
        BREW_TIMEOUT,
        BREW_REPLY,
    ) else {
        errors.push("homebrew: brew outdated did not finish".into());
        return None;
    };
    match parse_brew_outdated(&out) {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!("homebrew: {e}"));
            None
        }
    }
}

/// `brew outdated --json=v2`. Pinned formulae are left out.
pub fn parse_brew_outdated(text: &str) -> Result<Vec<BrewOutdated>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    let mut out = Vec::new();
    for (key, cask) in [("formulae", false), ("casks", true)] {
        for f in v.get(key).and_then(Value::as_array).into_iter().flatten() {
            if f.get("pinned").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(name) = f.get("name").and_then(Value::as_str) else {
                continue;
            };
            let installed = f
                .get("installed_versions")
                .and_then(Value::as_array)
                .and_then(|a| a.last())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let current = f
                .get("current_version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(BrewOutdated {
                name: name.chars().take(128).collect(),
                installed,
                current,
                cask,
            });
        }
    }
    if v.get("formulae").is_none() && v.get("casks").is_none() {
        return Err("no formulae or casks".into());
    }
    Ok(out)
}

/// (advisories, packages sent, lockfile errors)
fn advisories(
    http: &dyn Http,
    workspaces: &[PathBuf],
) -> Result<(Vec<Advisory>, usize, Vec<String>), String> {
    let mut by_ws: Vec<(PathBuf, Vec<Pkg>)> = Vec::new();
    let mut errs = Vec::new();
    let mut all: BTreeSet<Pkg> = BTreeSet::new();
    for ws in workspaces.iter().take(MAX_WORKSPACES) {
        let (pkgs, e) = workspace_packages(ws);
        errs.extend(e);
        if !pkgs.is_empty() {
            all.extend(pkgs.iter().map(|p| Pkg {
                direct: false,
                ..p.clone()
            }));
            by_ws.push((ws.clone(), pkgs));
        }
    }
    let mut all: Vec<Pkg> = all.into_iter().collect();
    if all.len() > MAX_PACKAGES {
        errs.push(format!(
            "advisories: {} packages; only the first {MAX_PACKAGES} were checked",
            all.len()
        ));
        all.truncate(MAX_PACKAGES);
    }
    if all.is_empty() {
        return Ok((Vec::new(), 0, errs));
    }
    let ids = osv_query(http, &all)?;
    let mut hits: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for (p, v) in all.iter().zip(&ids) {
        if !v.is_empty() {
            hits.insert(
                (p.ecosystem.clone(), p.name.clone(), p.version.clone()),
                v.clone(),
            );
        }
    }
    let unique: BTreeSet<&String> = ids.iter().flatten().collect();
    let mut details: HashMap<String, Vuln> = HashMap::new();
    for id in unique.into_iter().take(OSV_MAX_DETAILS) {
        match get_json(http, &format!("{OSV_VULN_URL}{id}"), OSV_VULN_REPLY)
            .and_then(|v| parse_vuln(&v))
        {
            Ok(v) => {
                details.insert(id.clone(), v);
            }
            Err(e) => errs.push(format!("advisories: {id}: {e}")),
        }
    }
    let mut out = Vec::new();
    for (ws, pkgs) in &by_ws {
        let mut per: BTreeMap<String, Advisory> = BTreeMap::new();
        for p in pkgs {
            let Some(vs) = hits.get(&(p.ecosystem.clone(), p.name.clone(), p.version.clone()))
            else {
                continue;
            };
            for id in vs {
                let d = details.get(id);
                let fixed = d.and_then(|d| fixed_for(d, p));
                let a = per.entry(id.clone()).or_insert_with(|| Advisory {
                    workspace: ws.clone(),
                    id: id.clone(),
                    aliases: d.map(|d| d.aliases.clone()).unwrap_or_default(),
                    summary: d.map(|d| d.summary.clone()).unwrap_or_default(),
                    severity: d.map(|d| d.severity).unwrap_or(Severity::Medium),
                    packages: Vec::new(),
                    fixed: Vec::new(),
                    commands: Vec::new(),
                });
                a.commands.push(advisory_command(p, fixed.as_deref(), ws));
                a.fixed.push(fixed);
                a.packages.push(p.clone());
            }
        }
        out.extend(per.into_values());
    }
    Ok((out, all.len(), errs))
}

// --------------------------------------------------------------- findings

fn suggestion(cmd: &str) -> Suggestion {
    Suggestion {
        label: format!("Run: {cmd}"),
        request: None,
        api: None,
        reversible: false,
    }
}

/// Findings (category `update`) for a report.
pub fn findings(r: &Report) -> Vec<Finding> {
    let now = r.checked_unix;
    let mk = |id: String,
              kind: FindingKind,
              sev: Severity,
              title: String,
              why: &str,
              ev: Evidence,
              cmds: &[String]| Finding {
        id,
        kind,
        category: FindingCategory::Update,
        severity: sev,
        title,
        why: why.into(),
        evidence: ev,
        suggested: cmds.iter().map(|c| suggestion(c)).collect(),
        first_seen_unix: now,
        last_seen_unix: now,
        dismissed: false,
        dismissed_until: None,
    };
    let mut out = Vec::new();
    let tools = r
        .mulchie
        .iter()
        .map(|t| (t, FindingKind::MulchieUpdate))
        .chain(r.agents.iter().map(|t| (t, FindingKind::AgentUpdate)));
    for (t, kind) in tools {
        if !t.outdated {
            continue;
        }
        let (Some(l), Some(i)) = (&t.latest, &t.installed) else {
            continue;
        };
        let cmd = t.command.clone().unwrap_or_default();
        out.push(mk(
            format!("{}:{}:{l}", kind.code(), t.name),
            kind,
            Severity::Low,
            format!("{} {l} is available (installed: {i})", t.name),
            "Newer releases fix bugs and security issues. Mulchie only reports this; it installs nothing.",
            Evidence {
                exe: t.path.clone(),
                detail: Some(format!("latest from {}; update with: {cmd}", t.source)),
                ..Evidence::default()
            },
            &[cmd],
        ));
    }
    if let Some(b) = r.homebrew.as_ref().filter(|b| !b.is_empty()) {
        let names: Vec<String> = b
            .iter()
            .take(30)
            .map(|p| format!("{} {} → {}", p.name, p.installed, p.current))
            .collect();
        out.push(mk(
            "homebrew_outdated".into(),
            FindingKind::HomebrewOutdated,
            Severity::Info,
            format!("{} Homebrew package(s) are outdated", b.len()),
            "From Homebrew's local data (no update was fetched). Mulchie only reports this.",
            Evidence {
                detail: Some(names.join(", ")),
                ..Evidence::default()
            },
            &["brew upgrade".to_string()],
        ));
    }
    for a in r.advisories.iter().flatten() {
        let pk: Vec<String> = a
            .packages
            .iter()
            .zip(&a.fixed)
            .map(|(p, f)| match f {
                Some(f) => format!("{} {} (fixed in {f})", p.name, p.version),
                None => format!("{} {} (no fixed version listed)", p.name, p.version),
            })
            .collect();
        let alias = a
            .aliases
            .iter()
            .find(|x| x.starts_with("CVE-"))
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        let mut cmds = a.commands.clone();
        cmds.dedup();
        out.push(mk(
            format!("advisory:{}:{}", a.workspace.display(), a.id),
            FindingKind::Advisory,
            a.severity,
            format!(
                "{}{alias} affects {} in {}{}",
                a.id,
                pk.join(", "),
                a.workspace.display(),
                if a.summary.is_empty() {
                    String::new()
                } else {
                    format!(": {}", a.summary)
                }
            ),
            "A published advisory (OSV.dev) matches a locked dependency version. Mulchie only reports it.",
            Evidence {
                workspace: Some(a.workspace.clone()),
                detail: Some(format!("https://osv.dev/vulnerability/{}", a.id)),
                ..Evidence::default()
            },
            &cmds,
        ));
    }
    out
}

// ------------------------------------------------------------------ cache

/// `<config dir>/updates.json`: the last report and when to check again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Cache {
    pub report: Option<Report>,
    /// No automatic check before this.
    pub next_unix: u64,
    /// Consecutive checks with errors (for the backoff).
    pub failures: u32,
}

pub fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("updates.json")
}

pub fn load_cache(config_dir: &Path) -> Cache {
    std::fs::read(cache_path(config_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_cache(config_dir: &Path, c: &Cache) -> std::io::Result<()> {
    crate::config::create_private_dir(config_dir)?;
    let data = serde_json::to_vec_pretty(c).map_err(std::io::Error::other)?;
    crate::config::write_atomic(&cache_path(config_dir), &data, 0o600)
}

/// When to check next after a check at `now`: the interval, or after errors
/// [`RETRY_BASE_SECS`] doubling per consecutive failure, up to the interval.
pub fn next_check(now: u64, interval_secs: u64, failures: u32) -> u64 {
    if failures == 0 {
        return now + interval_secs;
    }
    let back = RETRY_BASE_SECS.saturating_mul(1u64 << (failures - 1).min(16));
    now + back.min(interval_secs)
}

#[cfg(test)]
#[path = "updates_tests.rs"]
pub(crate) mod tests;
