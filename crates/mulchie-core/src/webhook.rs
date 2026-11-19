//! An optional webhook for issue events ([`crate::issues`]): a new issue,
//! one that came back after it was resolved, one the user resolved.
//!
//! Off until `webhook.url` is set. Then the engine that holds the lock, in a
//! build with the `network` feature, posts one small JSON body per event to
//! that URL from its own thread (`mulchie-webhook`), through a bounded queue
//! that drops, and counts, events when full. Each post has a 10 s timeout,
//! follows no redirect and is tried at most three times.
//!
//! The URL may hold a secret (a Slack incoming webhook's path is one), so it
//! is shown only as [`redact_url`]: scheme, host and port, and the path
//! replaced by `/[redacted]`. That redacted value, sent back unchanged in a
//! config write, means "keep the saved URL" ([`keep_redacted`]); it is never
//! saved ([`crate::config::Config::save_to`]) and never posted to.
//!
//! What is sent ([`payload`]): the event name, the issue's fields (title and
//! culprit passed through [`crate::redact`] again), for performance issues
//! the commit range from the finding's evidence with the session and agent
//! behind each commit, and the machine's host name only with
//! `webhook.include_host`. Nothing else about the machine.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::time::Duration;

use crate::issues::Issue;
use crate::watch::{Finding, FindingCategory};

/// Each post's timeout.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Tries per event: the first post and at most two retries.
pub const ATTEMPTS: u32 = 3;
/// Events waiting to be sent, at most; more are dropped and counted.
pub const QUEUE: usize = 64;
/// What replaces the URL's path, query and fragment wherever it is shown.
pub const REDACTED_PATH: &str = "/[redacted]";
/// A reply larger than this is not read further (it is never shown).
#[cfg(feature = "network")]
const MAX_REPLY: u64 = 64 << 10;
/// The longest URL taken.
const MAX_URL: usize = 2048;
/// Commits listed in a payload, at most (the newest).
const MAX_COMMITS: usize = 20;

pub const NO_NETWORK: &str = "this build of Mulchie has no network support, so the webhook is off";

/// What a webhook can be told about.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEvent {
    /// An issue seen for the first time.
    NewIssue,
    /// A resolved issue with a new finding.
    Regressed,
    /// The user marked an issue resolved.
    ResolvedByUser,
    /// `POST /v1/webhook/test`; not something `webhook.events` can name.
    Test,
}

impl WebhookEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::NewIssue => "new_issue",
            Self::Regressed => "regressed",
            Self::ResolvedByUser => "resolved_by_user",
            Self::Test => "test",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Self::NewIssue => "new issue",
            Self::Regressed => "issue came back",
            Self::ResolvedByUser => "issue resolved",
            Self::Test => "test event",
        }
    }
}

/// How the body is shaped.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebhookFormat {
    /// The payload of [`payload`].
    #[default]
    Json,
    /// `{"text": "<one line>"}` for a Slack incoming webhook.
    Slack,
}

// --------------------------------------------------------------------- url

/// The parts of a URL the webhook takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts<'a> {
    /// `https` or `http`.
    pub scheme: &'a str,
    /// Host and port as written (`hooks.slack.com`, `127.0.0.1:8080`,
    /// `[::1]:9000`).
    pub authority: &'a str,
    /// The host alone, without brackets.
    pub host: &'a str,
    /// Everything after the authority: path, query and fragment.
    pub rest: &'a str,
}

/// Split and check a webhook URL: `https://` to any host, `http://` only to
/// a loopback address (`localhost`, `127.0.0.0/8`, `[::1]`), no user name or
/// password, no spaces, control characters, backslashes or brackets outside
/// an IPv6 host. Errors never repeat the URL.
pub fn parse_url(url: &str) -> Result<UrlParts<'_>, &'static str> {
    if url.len() > MAX_URL {
        return Err("it is longer than 2048 characters");
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err("it has spaces, control characters or backslashes");
    }
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err("it must start with https:// (or http:// to a loopback address)");
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() {
        return Err("it has no host");
    }
    if authority.contains('@') {
        return Err("it has a user name or password; put a secret in the path or query instead");
    }
    if tail.contains(['[', ']']) {
        return Err("it has '[' or ']' outside an IPv6 host");
    }
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let (h, after) = r
            .split_once(']')
            .ok_or("its IPv6 host has no closing ']'")?;
        if h.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("its IPv6 host is not an IPv6 address");
        }
        let port = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .ok_or("it has text after the IPv6 host")?,
            ),
        };
        (h, port)
    } else {
        if authority.contains(['[', ']']) {
            return Err("it has '[' or ']' outside an IPv6 host");
        }
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let ipv6 = authority.starts_with('[');
    if host.is_empty()
        || (!ipv6
            && !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.')))
    {
        return Err("its host is not a host name or address");
    }
    if let Some(p) = port {
        if !p.parse::<u16>().is_ok_and(|p| p != 0) {
            return Err("its port is not a number from 1 to 65535");
        }
    }
    if scheme == "http" && !loopback(host) {
        return Err("plain http:// is taken only to a loopback address (localhost, 127.0.0.1, [::1]); use https://");
    }
    Ok(UrlParts {
        scheme,
        authority,
        host,
        rest: tail,
    })
}

/// `localhost`, or an address that is loopback (`127.0.0.0/8`, `::1`).
fn loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|a| a.is_loopback())
}

/// The URL as Mulchie shows it: `https://hooks.slack.com/[redacted]`, with
/// the port when there is one. Empty for an empty URL; a URL that does not
/// parse shows as `[invalid url]`, and a value already redacted as it is.
pub fn redact_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if is_redacted(url) {
        return url.to_string();
    }
    match parse_url(url) {
        Ok(p) => format!("{}://{}{REDACTED_PATH}", p.scheme, p.authority),
        Err(_) => "[invalid url]".into(),
    }
}

/// Whether `url` is a redacted value ([`redact_url`]) rather than a URL:
/// a scheme and host that check out, followed by exactly `/[redacted]`. A
/// real URL never is one, since brackets are refused outside a host.
pub fn is_redacted(url: &str) -> bool {
    url.strip_suffix(REDACTED_PATH)
        .and_then(|base| parse_url(base).ok())
        .is_some_and(|p| p.rest.is_empty())
}

/// Problems with a `webhook.url` value, for config validation. A redacted
/// value passes here (a config read back from `GET /v1/config` holds one);
/// writes resolve it with [`keep_redacted`] and saves refuse it.
pub fn url_problem(url: &str) -> Option<String> {
    if url.is_empty() || is_redacted(url) {
        return None;
    }
    parse_url(url)
        .err()
        .map(|why| format!("webhook.url is not usable: {why}"))
}

/// A config write sent `new.url`. When it is the redacted form of the saved
/// URL, keep the saved URL; another redacted value (for a different host, or
/// with no URL saved) is refused, since Mulchie cannot know what it stood
/// for.
pub fn keep_redacted(new: &mut String, saved: &str) -> Result<(), String> {
    if !is_redacted(new) {
        return Ok(());
    }
    if !saved.is_empty() && redact_url(saved) == *new {
        *new = saved.to_string();
        return Ok(());
    }
    Err(format!(
        "webhook.url is {new}, a redacted value that does not match the saved URL; send the full URL, or the value GET /v1/config returns to keep the saved one"
    ))
}

/// `text` with every occurrence of `url`, and of its path, replaced by its
/// redacted form. A last guard on error text; errors are built from fixed
/// words in the first place.
pub fn scrub(text: &str, url: &str) -> String {
    if url.is_empty() {
        return text.to_string();
    }
    let mut out = text.replace(url, &redact_url(url));
    if let Ok(p) = parse_url(url) {
        if p.rest.len() > 1 {
            out = out.replace(p.rest, REDACTED_PATH);
        }
    }
    out
}

// ----------------------------------------------------------------- payload

/// One commit of a performance issue's range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RangeCommit {
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// The commits behind a performance issue: from the last good revision to
/// the first bad one, oldest first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub commits: Vec<RangeCommit>,
    /// More commits than listed.
    pub truncated: bool,
}

fn of_entry(e: &crate::commits::CommitEntry) -> RangeCommit {
    RangeCommit {
        sha: e.head.clone(),
        at_unix: Some(e.at_unix),
        branch: e.branch.clone(),
        session_id: e.session_id.clone(),
        agent: e.agent.clone(),
    }
}

/// The commit range in a performance finding's evidence, or `None` for
/// other findings and ones without it.
pub fn commit_range(f: &Finding) -> Option<CommitRange> {
    if f.kind.category() != FindingCategory::Performance {
        return None;
    }
    let e = &f.evidence;
    let mut r = if let Some(p) = &e.perf {
        CommitRange {
            from: p.from_head.clone(),
            to: p.to_head.clone(),
            commits: p.commits.iter().map(of_entry).collect(),
            truncated: p.commits_truncated,
        }
    } else if let Some(d) = &e.devserver {
        CommitRange {
            from: d.from_head.clone(),
            to: d.to_head.clone(),
            commits: d.commits.iter().map(of_entry).collect(),
            truncated: d.commits_truncated,
        }
    } else if let Some(x) = &e.endpoint {
        CommitRange {
            from: x.from_revision.clone(),
            to: x.to_revision.clone(),
            commits: x.commits.iter().map(of_entry).collect(),
            truncated: x.commits_truncated,
        }
    } else if let Some(d) = &e.deploy {
        CommitRange {
            from: d.previous_sha.clone(),
            to: d.sha.clone(),
            commits: d
                .attribution
                .commits
                .iter()
                .map(|c| RangeCommit {
                    sha: c.sha.clone(),
                    at_unix: c.at_unix,
                    branch: c.branch.clone(),
                    session_id: c.session_id.clone(),
                    agent: c.agent.clone(),
                })
                .collect(),
            truncated: d.attribution.commits_truncated,
        }
    } else {
        return None;
    };
    if r.commits.len() > MAX_COMMITS {
        r.commits.drain(..r.commits.len() - MAX_COMMITS);
        r.truncated = true;
    }
    if r.from.is_none() && r.to.is_none() && r.commits.is_empty() {
        return None;
    }
    Some(r)
}

/// One event to send.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueEvent {
    pub event: WebhookEvent,
    /// `None` for a test event.
    pub issue: Option<Issue>,
    pub commits: Option<CommitRange>,
}

impl IssueEvent {
    pub fn new(event: WebhookEvent, issue: Issue, finding: Option<&Finding>) -> Self {
        Self {
            event,
            commits: finding.and_then(commit_range),
            issue: Some(issue),
        }
    }

    pub fn test() -> Self {
        Self {
            event: WebhookEvent::Test,
            issue: None,
            commits: None,
        }
    }
}

/// Whether `event` about `issue` goes out under `cfg`.
pub fn wanted(cfg: &crate::config::WebhookConfig, event: WebhookEvent, issue: &Issue) -> bool {
    !cfg.url.is_empty() && cfg.events.contains(&event) && issue.severity >= cfg.min_severity
}

fn redacted_text(s: &str) -> String {
    crate::redact::redact_command_line(s)
}

/// The JSON body (format `json`).
pub fn payload(ev: &IssueEvent, host: Option<&str>) -> Value {
    let mut v = json!({ "event": ev.event.name() });
    if let Some(i) = &ev.issue {
        v["issue"] = json!({
            "id": i.id,
            "kind": i.kind,
            "category": i.category,
            "severity": i.severity,
            "title": redacted_text(&i.title),
            "culprit": redacted_text(&i.culprit),
            "workspace": i.workspace,
            "occurrences": i.occurrences,
            "status": i.status,
            "regressions": i.regressions,
            "first_seen_unix": i.first_seen_unix,
            "last_seen_unix": i.last_seen_unix,
        });
    } else {
        v["message"] = json!("A test event from Mulchie. Issue events will arrive here.");
    }
    if let Some(c) = &ev.commits {
        v["commits"] = json!(c);
    }
    if let Some(h) = host {
        v["host"] = json!(h);
    }
    v
}

/// Slack's control characters, escaped as Slack asks.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The one line of a `slack` body.
pub fn slack_text(ev: &IssueEvent, host: Option<&str>) -> String {
    let on = host.map(|h| format!(" on {h}")).unwrap_or_default();
    let Some(i) = &ev.issue else {
        return slack_escape(&format!(
            "Mulchie{on}: test event. Issue events will arrive here."
        ));
    };
    let sev = serde_json::to_value(i.severity)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let ws = i
        .workspace
        .as_deref()
        .and_then(|w| w.file_name())
        .map(|n| format!(" in {}", n.to_string_lossy()))
        .unwrap_or_default();
    let commits = ev
        .commits
        .as_ref()
        .filter(|c| !c.commits.is_empty())
        .map(|c| format!(", {} commit(s) in range", c.commits.len()))
        .unwrap_or_default();
    slack_escape(&format!(
        "Mulchie{on}: {} ({sev}){ws}: {} [{}, seen {} time(s){commits}]",
        ev.event.words(),
        redacted_text(&i.title),
        redacted_text(&i.culprit),
        i.occurrences,
    ))
}

/// The body to post and its content type.
pub fn body(format: WebhookFormat, ev: &IssueEvent, host: Option<&str>) -> Vec<u8> {
    let v = match format {
        WebhookFormat::Json => payload(ev, host),
        WebhookFormat::Slack => json!({ "text": slack_text(ev, host) }),
    };
    serde_json::to_vec(&v).unwrap_or_default()
}

/// This machine's host name, for `webhook.include_host`.
pub fn host_name() -> Option<String> {
    sysinfo::System::host_name().ok().filter(|h| !h.is_empty())
}

// -------------------------------------------------------------------- http

/// Posting a body. The real one is [`Post`]; tests use a fake or a loopback
/// server. Errors are fixed words, never the URL.
pub trait WebhookHttp: Send + Sync {
    /// The reply's status, or why there was none.
    fn post(&self, url: &str, body: &[u8]) -> Result<u16, String>;
}

/// A build without the `network` feature (and this crate's own tests,
/// unless they pass a client): nothing is sent.
pub struct NoNetwork;

impl WebhookHttp for NoNetwork {
    fn post(&self, _: &str, _: &[u8]) -> Result<u16, String> {
        Err(NO_NETWORK.into())
    }
}

/// ureq with rustls and the webpki roots: certificates are verified, no
/// redirect is followed (a 3xx is an error), 10 s per post. Plain `http` is
/// allowed by the client because [`parse_url`] allows it only to loopback;
/// every post checks the URL again first.
#[cfg(feature = "network")]
pub struct Post {
    agent: ureq::Agent,
}

#[cfg(feature = "network")]
impl Post {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .https_only(false)
            .max_redirects(0)
            .user_agent(crate::updates::USER_AGENT)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

#[cfg(feature = "network")]
impl Default for Post {
    fn default() -> Self {
        Self::new()
    }
}

/// A ureq error in fixed words: its own text can hold the URL.
#[cfg(feature = "network")]
fn words(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => "connection refused".into(),
            std::io::ErrorKind::TimedOut => "timed out".into(),
            _ => "network error".into(),
        },
        ureq::Error::Tls(_) => "TLS failed".into(),
        _ => "request failed (TLS or protocol error)".into(),
    }
}

#[cfg(feature = "network")]
impl WebhookHttp for Post {
    fn post(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        parse_url(url).map_err(|w| format!("refused the URL: {w}"))?;
        let mut r = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| words(&e))?;
        // Read a little of the reply so the connection ends cleanly; it is
        // never shown.
        let _ = r.body_mut().with_config().limit(MAX_REPLY).read_to_vec();
        Ok(r.status().as_u16())
    }
}

/// The client of this build.
pub fn default_http() -> std::sync::Arc<dyn WebhookHttp> {
    #[cfg(feature = "network")]
    {
        std::sync::Arc::new(Post::new())
    }
    #[cfg(not(feature = "network"))]
    {
        std::sync::Arc::new(NoNetwork)
    }
}

/// Whether this build can send at all.
pub const fn network_built() -> bool {
    cfg!(feature = "network")
}

/// Post `body` to `url` up to `attempts` times, waiting `backoff`, then
/// twice that, between tries. A 2xx is done; a 3xx or another 4xx is not
/// tried again; a 429, a 5xx or no reply is. Errors name the redacted URL
/// and the status or reason, never the URL itself or the reply.
pub fn deliver(
    http: &dyn WebhookHttp,
    url: &str,
    body: &[u8],
    attempts: u32,
    backoff: Duration,
    sleep: &dyn Fn(Duration),
) -> Result<u16, String> {
    let shown = redact_url(url);
    if let Err(why) = parse_url(url) {
        return Err(format!("webhook.url is not usable: {why}"));
    }
    let mut wait = backoff;
    let mut last = String::new();
    for n in 0..attempts.max(1) {
        if n > 0 {
            sleep(wait);
            wait *= 2;
        }
        match http.post(url, body) {
            Ok(s) if (200..300).contains(&s) => return Ok(s),
            Ok(s) if (300..400).contains(&s) => {
                return Err(format!(
                    "{shown} answered with a redirect (HTTP {s}); Mulchie follows none"
                ))
            }
            Ok(s) if s == 429 || s >= 500 => last = format!("HTTP {s} from {shown}"),
            Ok(s) => return Err(format!("HTTP {s} from {shown}")),
            Err(e) => last = format!("posting to {shown} failed: {}", scrub(&e, url)),
        }
    }
    Err(format!("{last} (tried {} times)", attempts.max(1)))
}

#[cfg(test)]
#[path = "webhook_tests.rs"]
pub(crate) mod tests;
