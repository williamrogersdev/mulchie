//! Read-only links to the hosting and monitoring tools a developer already
//! uses: Vercel (deployments), Sentry (releases and transaction p95) and
//! PostHog (web vitals per release). Mulchie lists recent deploys with their
//! commit, reads one performance number per deploy, matches each deploy's
//! commit to the moves of `HEAD` it recorded ([`crate::commits`]) and raises
//! a `production_slower` finding when a deploy made a number worse.
//!
//! What goes over the network, and only from the engine that holds the lock,
//! only for a provider the user turned on (`integrations.<provider>.enabled`)
//! and only in a build with the `network` feature: authenticated `GET` (and
//! for PostHog one `POST` of a read-only HogQL query) requests to that
//! provider's own API host, over HTTPS with certificate verification, with no
//! redirects followed. The request carries the provider token, the project
//! identifiers from the config and nothing about this machine.
//!
//! Tokens live in `<config dir>/integrations/<provider>.token` (mode 0600 in
//! a 0700 directory, written with [`write_atomic`], read refusing symbolic
//! links, other owners, looser modes and anything but a regular file). A
//! token is held in a [`Secret`], which has no `Serialize` and whose `Debug`
//! prints nothing of it; every error string from a request is passed through
//! [`scrub`] before it is kept. Tokens are never in the config, argv, logs,
//! the journal, findings or API responses.
//!
//! `remove_token` deletes the token file: a documented exception to "only
//! the safety gate deletes" (Mulchie's own file, a regular file of exactly
//! that name; a symbolic link is refused, not followed or removed).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::commits::{self, CommitEntry};
use crate::config::{create_private_dir, write_atomic, IntegrationsConfig};
use crate::consumers::Severity;
use crate::paths::Paths;
use crate::perf;
use crate::updates::HttpReply;
use crate::watch::{ApiRequest, Evidence, Finding, FindingCategory, FindingKind, Suggestion};

#[path = "integrations_posthog.rs"]
pub mod posthog;
#[path = "integrations_sentry.rs"]
pub mod sentry;
#[path = "integrations_vercel.rs"]
pub mod vercel;

/// Longest token accepted.
pub const MAX_TOKEN: usize = 4096;
/// Response size cap per request.
pub const MAX_REPLY: usize = 4 << 20;
/// Deploys kept per provider (the newest).
pub const MAX_DEPLOYS: usize = 50;
/// Commits listed in one finding, at most (the newest).
pub const MAX_EVIDENCE_COMMITS: usize = 20;
/// The first automatic poll waits this long after the engine starts.
pub const FIRST_POLL_DELAY_SECS: u64 = 60;
/// A requested poll runs no sooner than this after the last one.
pub const MIN_MANUAL_GAP_SECS: u64 = 60;
/// Backoff after errors stops growing here.
pub const MAX_BACKOFF_SECS: u64 = 24 * 3600;
/// What a build without the `network` feature says.
pub const NO_NETWORK: &str = "this build of Mulchie has no network support";

// --------------------------------------------------------------- providers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Vercel,
    Sentry,
    Posthog,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Vercel, Provider::Sentry, Provider::Posthog];
    pub const NAMES: [&'static str; 3] = ["vercel", "sentry", "posthog"];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Vercel => "vercel",
            Provider::Sentry => "sentry",
            Provider::Posthog => "posthog",
        }
    }

    /// As people write it.
    pub fn title(self) -> &'static str {
        match self {
            Provider::Vercel => "Vercel",
            Provider::Sentry => "Sentry",
            Provider::Posthog => "PostHog",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn enabled(self, cfg: &IntegrationsConfig) -> bool {
        match self {
            Provider::Vercel => cfg.vercel.enabled,
            Provider::Sentry => cfg.sentry.enabled,
            Provider::Posthog => cfg.posthog.enabled,
        }
    }

    /// The one API host requests for this provider may go to.
    pub fn host(self, cfg: &IntegrationsConfig) -> String {
        match self {
            Provider::Vercel => vercel::HOST.to_string(),
            Provider::Sentry => cfg.sentry.host.clone(),
            Provider::Posthog => cfg.posthog.host.clone(),
        }
    }

    /// The configured checkout of the project, for commit attribution.
    pub fn workspace(self, cfg: &IntegrationsConfig) -> Option<PathBuf> {
        match self {
            Provider::Vercel => cfg.vercel.workspace.clone(),
            Provider::Sentry => cfg.sentry.workspace.clone(),
            Provider::Posthog => cfg.posthog.workspace.clone(),
        }
    }

    /// The source that talks to this provider with `cfg`.
    pub fn source(self, cfg: &IntegrationsConfig) -> Box<dyn DeploySource> {
        match self {
            Provider::Vercel => Box::new(vercel::Vercel(cfg.vercel.clone())),
            Provider::Sentry => Box::new(sentry::Sentry(cfg.sentry.clone())),
            Provider::Posthog => Box::new(posthog::PostHog(cfg.posthog.clone())),
        }
    }
}

// ------------------------------------------------------------------ secret

/// A provider token. Not `Serialize`, not `Clone`, and its `Debug` shows
/// nothing of it, so it cannot reach a log, a response or a file by
/// accident.
pub struct Secret(String);

impl Secret {
    /// A token as typed or read: surrounding whitespace dropped; refused
    /// when empty, longer than [`MAX_TOKEN`], or holding anything but
    /// printable ASCII without spaces.
    pub fn new(raw: &str) -> Result<Secret, String> {
        let t = raw.trim();
        if t.is_empty() {
            return Err("the token is empty".into());
        }
        if t.len() > MAX_TOKEN {
            return Err(format!("the token is longer than {MAX_TOKEN} bytes"));
        }
        if !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("the token must be printable ASCII without spaces".into());
        }
        Ok(Secret(t.to_string()))
    }

    /// The token itself, for the `Authorization` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// `text` with every copy of the token replaced by `[redacted]`.
pub fn scrub(text: &str, token: &Secret) -> String {
    if token.0.is_empty() {
        return text.to_string();
    }
    text.replace(&token.0, "[redacted]")
}

// ------------------------------------------------------------- token files

pub fn token_dir(paths: &Paths) -> PathBuf {
    paths.config_dir.join("integrations")
}

pub fn token_path(paths: &Paths, p: Provider) -> PathBuf {
    token_dir(paths).join(format!("{}.token", p.name()))
}

fn perm(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg)
}

/// Store `token` for `p`: mode 0600 in a 0700 directory, through a temp
/// file and rename. A symbolic link or anything but a regular file in its
/// place is refused.
pub fn save_token(paths: &Paths, p: Provider, token: &Secret) -> std::io::Result<()> {
    create_private_dir(&paths.config_dir)?;
    let dir = token_dir(paths);
    create_private_dir(&dir)?;
    let path = token_path(paths, p);
    if let Ok(m) = std::fs::symlink_metadata(&path) {
        if !m.file_type().is_file() {
            return Err(perm(format!(
                "{} is not a regular file; remove it first",
                path.display()
            )));
        }
    }
    let mut data = token.0.clone().into_bytes();
    data.push(b'\n');
    write_atomic(&path, &data, 0o600)
}

/// The stored token for `p`. Refused when the directory or file is not
/// owned by this user, is readable by others, is a symbolic link or not a
/// regular file, or holds no valid token.
pub fn read_token(paths: &Paths, p: Provider) -> std::io::Result<Secret> {
    let uid = unsafe { libc::getuid() };
    let dir = token_dir(paths);
    let d = std::fs::symlink_metadata(&dir)?;
    if !d.is_dir() || d.uid() != uid || d.mode() & 0o077 != 0 {
        return Err(perm(format!(
            "{} must be a directory owned by you with mode 700",
            dir.display()
        )));
    }
    let path = token_path(paths, p);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)?;
    let m = f.metadata()?;
    if !m.file_type().is_file() {
        return Err(perm(format!("{} is not a regular file", path.display())));
    }
    if m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(perm(format!(
            "{} must be owned by you with mode 600",
            path.display()
        )));
    }
    let mut s = String::new();
    f.take(MAX_TOKEN as u64 + 2).read_to_string(&mut s)?;
    Secret::new(&s).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })
}

/// Whether a token file is in place for `p` (not whether it is valid).
pub fn token_present(paths: &Paths, p: Provider) -> bool {
    std::fs::symlink_metadata(token_path(paths, p)).is_ok_and(|m| m.file_type().is_file())
}

/// Delete the token file of `p`. Only a regular file of exactly that name
/// inside `<config dir>/integrations/`; a symbolic link or anything else is
/// refused and left in place. `Ok(false)` when there was none.
pub fn remove_token(paths: &Paths, p: Provider) -> std::io::Result<bool> {
    let path = token_path(paths, p);
    let dir = token_dir(paths);
    match std::fs::symlink_metadata(&dir) {
        Ok(d) if d.is_dir() => {}
        Ok(_) => {
            return Err(perm(format!("{} is not a directory", dir.display())));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {
            std::fs::remove_file(&path)?;
            Ok(true)
        }
        Ok(_) => Err(perm(format!(
            "{} is not a regular file, so Mulchie left it in place",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// -------------------------------------------------------------------- http

/// Authenticated HTTPS to a provider's API. The real one is [`Https`];
/// tests use a fake. The token is passed per call and never stored.
pub trait AuthHttp: Send + Sync {
    fn get(&self, url: &str, token: &Secret, max_bytes: usize) -> Result<HttpReply, String>;
    fn post_json(
        &self,
        url: &str,
        token: &Secret,
        body: &[u8],
        max_bytes: usize,
    ) -> Result<HttpReply, String>;
}

/// A build without the `network` feature: every request fails.
pub struct NoNetwork;

impl AuthHttp for NoNetwork {
    fn get(&self, _: &str, _: &Secret, _: usize) -> Result<HttpReply, String> {
        Err(NO_NETWORK.into())
    }
    fn post_json(&self, _: &str, _: &Secret, _: &[u8], _: usize) -> Result<HttpReply, String> {
        Err(NO_NETWORK.into())
    }
}

/// HTTPS through ureq with rustls and the webpki roots: certificates are
/// verified, plain HTTP is refused, and no redirect is followed (a 3xx is
/// an error), so the token only ever goes to the host asked for.
#[cfg(feature = "network")]
pub struct Https {
    agent: ureq::Agent,
}

#[cfg(feature = "network")]
impl Https {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(crate::updates::HTTP_TIMEOUT))
            .http_status_as_error(false)
            .https_only(true)
            .max_redirects(0)
            .user_agent(crate::updates::USER_AGENT)
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
impl AuthHttp for Https {
    fn get(&self, url: &str, token: &Secret, max: usize) -> Result<HttpReply, String> {
        Self::read(
            self.agent
                .get(url)
                .header("Accept", "application/json")
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .call(),
            max,
        )
    }
    fn post_json(
        &self,
        url: &str,
        token: &Secret,
        body: &[u8],
        max: usize,
    ) -> Result<HttpReply, String> {
        Self::read(
            self.agent
                .post(url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .send(body),
            max,
        )
    }
}

/// The HTTPS client of this build.
pub fn default_http() -> std::sync::Arc<dyn AuthHttp> {
    #[cfg(feature = "network")]
    {
        std::sync::Arc::new(Https::new())
    }
    #[cfg(not(feature = "network"))]
    {
        std::sync::Arc::new(NoNetwork)
    }
}

/// Whether this build can reach providers at all.
pub const fn network_built() -> bool {
    cfg!(feature = "network")
}

/// The host of an `https://` URL, or `None` for any other scheme or a URL
/// with credentials or a port.
pub fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    if auth.is_empty() || auth.contains('@') || auth.contains(':') {
        return None;
    }
    Some(auth)
}

/// One request through `http`, only to `host` over HTTPS; the reply must be
/// 200 with JSON. Errors name the host and status, never the body, and are
/// scrubbed of the token.
pub(crate) fn fetch_json(
    http: &dyn AuthHttp,
    host: &str,
    url: &str,
    token: &Secret,
    post: Option<&[u8]>,
) -> Result<Value, String> {
    if https_host(url) != Some(host) {
        return Err(format!("refused a request outside https://{host}"));
    }
    let r = match post {
        Some(b) => http.post_json(url, token, b, MAX_REPLY),
        None => http.get(url, token, MAX_REPLY),
    }
    .map_err(|e| scrub(&format!("request to {host} failed: {e}"), token))?;
    match r.status {
        200 => {}
        300..=399 => {
            return Err(format!(
                "{host} answered with a redirect (HTTP {}); Mulchie follows none",
                r.status
            ))
        }
        401 | 403 => {
            return Err(format!(
                "{host} refused the token (HTTP {}); check its scopes or add a new one with `mulchie integrations add`",
                r.status
            ))
        }
        404 => {
            return Err(format!(
                "{host} found no such project (HTTP 404); check the identifiers in `mulchie integrations list`"
            ))
        }
        429 => return Err(format!("{host} is rate limiting requests (HTTP 429)")),
        s => return Err(format!("HTTP {s} from {host}")),
    }
    serde_json::from_slice(&r.body).map_err(|_| format!("{host} did not answer with JSON"))
}

/// `s` with characters outside unreserved URL characters percent-encoded.
pub fn enc(s: &str) -> String {
    crate::api::query_escape(s)
}

// ----------------------------------------------------------------- deploys

/// One performance number of a deploy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricValue {
    /// `build_duration`, `p95`, `lcp_p75`, `inp_p75`.
    pub metric: String,
    /// In milliseconds.
    pub value_ms: f64,
    /// How many measurements it is made of, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<u64>,
}

/// A deploy or release as a provider lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deploy {
    pub provider: Provider,
    /// The provider's id: a deployment id, or a release version.
    pub id: String,
    /// The git commit it was built from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// When it was created or released (or first seen, for PostHog).
    pub at_unix: u64,
    #[serde(default)]
    pub metrics: Vec<MetricValue>,
}

impl Deploy {
    pub fn metric(&self, name: &str) -> Option<&MetricValue> {
        self.metrics.iter().find(|m| m.metric == name)
    }

    /// The commit's first 7 digits, else the id.
    pub fn short(&self) -> String {
        match &self.sha {
            Some(s) => s.chars().take(7).collect(),
            None => self.id.chars().take(24).collect(),
        }
    }
}

/// A source of deploys and their numbers. Two operations, both read-only.
pub trait DeploySource {
    fn provider(&self) -> Provider;
    /// Recent deploys (production ones where the provider tells them
    /// apart), any order.
    fn deploys(&self, http: &dyn AuthHttp, token: &Secret, now: u64)
        -> Result<Vec<Deploy>, String>;
    /// Fill in `metrics` of `deploys`.
    fn metrics(
        &self,
        http: &dyn AuthHttp,
        token: &Secret,
        deploys: &mut [Deploy],
    ) -> Result<(), String>;
}

/// A metric a provider reports: its name and how a title says it.
pub fn metric_label(metric: &str) -> &'static str {
    match metric {
        "build_duration" => "build duration",
        "p95" => "transaction p95",
        "lcp_p75" => "LCP (p75)",
        "inp_p75" => "INP (p75)",
        _ => "metric",
    }
}

/// Both operations of `src`, newest deploy first, at most [`MAX_DEPLOYS`].
pub fn poll(
    src: &dyn DeploySource,
    http: &dyn AuthHttp,
    token: &Secret,
    now: u64,
) -> Result<Vec<Deploy>, String> {
    let mut d = src.deploys(http, token, now)?;
    d.sort_by(|a, b| b.at_unix.cmp(&a.at_unix).then_with(|| b.id.cmp(&a.id)));
    d.dedup_by(|a, b| a.id == b.id);
    d.truncate(MAX_DEPLOYS);
    src.metrics(http, token, &mut d)?;
    Ok(d)
}

/// A commit SHA in a release name: the whole name when it is 7 to 40 hex
/// digits, or the part after the last `@` or `+` (`app@<sha>`).
pub fn sha_in(version: &str) -> Option<String> {
    let hex = |s: &str| (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit());
    let v = version.trim();
    if hex(v) {
        return Some(v.to_ascii_lowercase());
    }
    let tail = v.rsplit(['@', '+']).next()?;
    (tail.len() < v.len() && hex(tail)).then(|| tail.to_ascii_lowercase())
}

/// A full commit id as git writes it.
pub fn valid_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Seconds since 1970 for an RFC 3339 / ISO 8601 UTC time such as
/// `2026-11-02T10:04:05.123456Z` or `2026-11-02T10:04:05+01:00`.
pub fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let oh: i64 = o.get(1..3)?.parse().ok()?;
            let om: i64 = o.get(4..6)?.parse().ok()?;
            let v = oh * 3600 + om * 60;
            if o.starts_with('+') {
                v
            } else {
                -v
            }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = days * 86_400 + h * 3600 + mi * 60 + se - offset;
    u64::try_from(t).ok()
}

// ------------------------------------------------------------- attribution

/// One commit in a deploy, as this machine saw it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployCommit {
    /// The commit `HEAD` moved to.
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// `session` (an agent session was working there), `no_session` (seen
    /// on this machine, no session) or `made_elsewhere` (this machine never
    /// recorded it).
    pub made: String,
}

impl DeployCommit {
    fn of(e: &CommitEntry) -> Self {
        Self {
            sha: e.head.clone(),
            at_unix: Some(e.at_unix),
            branch: e.branch.clone(),
            session_id: e.session_id.clone(),
            agent: e.agent.clone(),
            made: if e.session_id.is_some() {
                "session".into()
            } else {
                "no_session".into()
            },
        }
    }

    fn elsewhere(sha: &str) -> Self {
        Self {
            sha: sha.to_string(),
            at_unix: None,
            branch: None,
            session_id: None,
            agent: None,
            made: "made_elsewhere".into(),
        }
    }
}

/// What a deploy contains: the recorded moves of `HEAD` from the previous
/// deploy's commit to this one's, the workspace they were in, and whether
/// the list was cut.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub commits: Vec<DeployCommit>,
    #[serde(default)]
    pub commits_truncated: bool,
}

/// The commits between `from` (the previous deploy's commit) and `to`,
/// looked up in `workspace` when given, else in any workspace whose record
/// has `to`. A commit this machine never recorded comes back alone, marked
/// `made_elsewhere`.
pub fn attribute(
    log: &[CommitEntry],
    workspace: Option<&Path>,
    from: Option<&str>,
    to: Option<&str>,
) -> Attribution {
    let Some(to) = to else {
        return Attribution::default();
    };
    let ws: Option<PathBuf> = match workspace {
        Some(w) => Some(w.to_path_buf()),
        None => log
            .iter()
            .rev()
            .find(|e| commits::same_commit(&e.head, to))
            .map(|e| e.workspace.clone()),
    };
    let Some(ws) = ws else {
        return Attribution {
            workspace: None,
            commits: vec![DeployCommit::elsewhere(to)],
            commits_truncated: false,
        };
    };
    let mut moves = match from {
        Some(f) => commits::between(log, &ws, f, to),
        None => commits::attribute(log, &ws, to)
            .cloned()
            .into_iter()
            .collect(),
    };
    if moves.is_empty() {
        if let Some(e) = commits::attribute(log, &ws, to) {
            moves.push(e.clone());
        }
    }
    let truncated = moves.len() > MAX_EVIDENCE_COMMITS;
    if truncated {
        moves.drain(..moves.len() - MAX_EVIDENCE_COMMITS);
    }
    let commits: Vec<DeployCommit> = if moves.is_empty() {
        vec![DeployCommit::elsewhere(to)]
    } else {
        moves.iter().map(DeployCommit::of).collect()
    };
    Attribution {
        workspace: Some(ws),
        commits,
        commits_truncated: truncated,
    }
}

// ----------------------------------------------------------------- finding

/// Evidence of a `production_slower` finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployEvidence {
    pub provider: Provider,
    pub metric: String,
    pub metric_label: String,
    /// The deploy after which it got worse.
    pub deploy_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub deploy_unix: u64,
    /// The deploy just before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_deploy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sha: Option<String>,
    /// The median over the deploys before, and how many.
    pub before_ms: f64,
    pub baseline_deploys: usize,
    /// The metric of the deploy (the median when several deploys in a row
    /// are slower).
    pub after_ms: f64,
    pub ratio: f64,
    #[serde(flatten)]
    pub attribution: Attribution,
}

/// `120 ms`, `2.4 s`, `45s`.
pub fn human_value(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{} ms", ms.round() as u64)
    } else if ms < 10_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        perf::human_ms(ms as u64)
    }
}

/// `production_slower:<provider>:<metric>:<deploy id>`.
pub fn finding_id(p: Provider, metric: &str, deploy_id: &str) -> String {
    format!("production_slower:{}:{metric}:{deploy_id}", p.name())
}

/// The `production_slower` findings for each provider's deploys (newest
/// first, as [`poll`] returns them), with attribution from `log`.
pub fn findings(
    deploys: &[(Provider, Vec<Deploy>)],
    log: &[CommitEntry],
    cfg: &IntegrationsConfig,
    now: u64,
) -> Vec<Finding> {
    let t = cfg.thresholds();
    let mut out = Vec::new();
    for (p, list) in deploys {
        // Oldest first.
        let all: Vec<&Deploy> = list.iter().rev().collect();
        let mut names: Vec<&str> = all
            .iter()
            .flat_map(|d| d.metrics.iter().map(|m| m.metric.as_str()))
            .collect();
        names.sort();
        names.dedup();
        for metric in names {
            let with: Vec<(usize, &Deploy, f64)> = all
                .iter()
                .enumerate()
                .filter_map(|(i, d)| {
                    d.metric(metric)
                        .filter(|m| m.value_ms.is_finite() && m.value_ms >= 0.0)
                        .map(|m| (i, *d, m.value_ms))
                })
                .collect();
            let keep = (cfg.baseline_deploys + 1) * 4;
            let with = &with[with.len().saturating_sub(keep)..];
            let series: Vec<f64> = with.iter().map(|w| w.2).collect();
            let Some(s) = perf::slower(&series, &t) else {
                continue;
            };
            let (idx, dep, _) = with[s.first_slow];
            let prev = idx.checked_sub(1).map(|i| all[i]);
            let attribution = attribute(
                log,
                p.workspace(cfg).as_deref(),
                prev.and_then(|d| d.sha.as_deref()),
                dep.sha.as_deref(),
            );
            let after = perf::median(&s.recent).unwrap_or(s.baseline);
            let label = metric_label(metric);
            let title = format!(
                "Production {label} on {} went from {} to {} after deploy {}",
                p.title(),
                human_value(s.baseline),
                human_value(after),
                dep.short()
            );
            let who: Vec<String> = attribution
                .commits
                .iter()
                .filter_map(|c| c.agent.clone().or_else(|| c.session_id.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let made = if dep.sha.is_none() {
                format!(
                    "{} gave no commit for this deploy, so Mulchie cannot say what was in it.",
                    p.title()
                )
            } else if attribution.workspace.is_none() {
                "This machine never recorded its commit: it was made elsewhere.".to_string()
            } else if who.is_empty() {
                format!(
                    "This machine recorded {} move(s) of HEAD in it, with no agent session working there.",
                    attribution.commits.len()
                )
            } else {
                format!(
                    "This machine recorded {} move(s) of HEAD in it while {} worked there: see evidence.deploy.commits.",
                    attribution.commits.len(),
                    who.join(", ")
                )
            };
            let vercel_note = if *p == Provider::Vercel {
                " Vercel's API offers no per-deployment latency, so this is how long the deploy took to build, not how fast it serves."
            } else {
                ""
            };
            let why = format!(
                "The {label} reported by {} for this deploy is more than {}× the median of the {} deploys before it ({}) and more than {} ms higher. {made}{vercel_note} Mulchie only reports this; it changes nothing.",
                p.title(),
                cfg.slower_ratio,
                s.baseline_runs,
                human_value(s.baseline),
                cfg.min_increase_ms
            );
            let severity = if s.ratio >= 2.0 {
                Severity::Medium
            } else {
                Severity::Low
            };
            out.push(Finding {
                id: finding_id(*p, metric, &dep.id),
                kind: FindingKind::ProductionSlower,
                category: FindingCategory::Performance,
                severity,
                title,
                why,
                evidence: Evidence {
                    name: Some(format!("{} {metric}", p.name())),
                    workspace: attribution.workspace.clone(),
                    detail: Some(format!(
                        "{} {label}: {} before (median of {} deploys), {} after deploy {}",
                        p.title(),
                        human_value(s.baseline),
                        s.baseline_runs,
                        human_value(after),
                        dep.id
                    )),
                    deploy: Some(Box::new(DeployEvidence {
                        provider: *p,
                        metric: metric.to_string(),
                        metric_label: label.to_string(),
                        deploy_id: dep.id.clone(),
                        sha: dep.sha.clone(),
                        deploy_unix: dep.at_unix,
                        previous_deploy_id: prev.map(|d| d.id.clone()),
                        previous_sha: prev.and_then(|d| d.sha.clone()),
                        before_ms: s.baseline,
                        baseline_deploys: s.baseline_runs,
                        after_ms: after,
                        ratio: s.ratio,
                        attribution,
                    })),
                    ..Evidence::default()
                },
                suggested: vec![Suggestion::call(
                    format!("List {} deploys", p.title()),
                    ApiRequest::get(format!("/v1/deploys?provider={}", p.name())),
                    true,
                )],
                first_seen_unix: now,
                last_seen_unix: now,
                dismissed: false,
                dismissed_until: None,
            });
        }
    }
    out
}

// ------------------------------------------------------------------- cache

/// Where one provider stands. No token, ever.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProviderCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix: Option<u64>,
    /// The last error, in words (scrubbed of the token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Consecutive polls that failed.
    #[serde(default)]
    pub failures: u32,
    /// No automatic poll before this.
    #[serde(default)]
    pub next_unix: u64,
    /// The deploys of the last successful poll, newest first.
    #[serde(default)]
    pub deploys: Vec<Deploy>,
}

/// `<config dir>/integrations.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Cache {
    #[serde(default)]
    pub providers: std::collections::BTreeMap<Provider, ProviderCache>,
}

pub fn cache_path(paths: &Paths) -> PathBuf {
    paths.config_dir.join("integrations.json")
}

/// The cache, or an empty one when the file is missing, not a plain file,
/// over 8 MB or unreadable.
pub fn load_cache(paths: &Paths) -> Cache {
    let p = cache_path(paths);
    let ok =
        std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_file() && m.len() <= 8 << 20);
    if !ok {
        return Cache::default();
    }
    std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_cache(paths: &Paths, c: &Cache) -> std::io::Result<()> {
    create_private_dir(&paths.config_dir)?;
    let data = serde_json::to_vec_pretty(c).map_err(std::io::Error::other)?;
    write_atomic(&cache_path(paths), &data, 0o600)
}

/// When to poll next after a poll at `now`: the interval, doubled per
/// consecutive failure up to a day.
pub fn next_poll(now: u64, interval_secs: u64, failures: u32) -> u64 {
    let back = interval_secs.saturating_mul(1u64 << failures.min(16));
    now + back.min(MAX_BACKOFF_SECS.max(interval_secs))
}

#[cfg(test)]
#[path = "integrations_tests.rs"]
pub(crate) mod tests;
