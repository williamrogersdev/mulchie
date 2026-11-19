//! The issue webhook ([`crate::webhook`]) on its own thread
//! (`mulchie-webhook`), started only by the engine that holds the lock and
//! only in a build with network support. Issue events are queued (at most
//! [`webhook::QUEUE`]; more are dropped and counted) and posted one at a
//! time. The URL is read from the config when an event is sent, so clearing
//! it stops what is queued. The status never holds the URL, only its
//! redacted form.

use super::*;
use crate::issues::Issue;
use crate::webhook::{self, IssueEvent, WebhookEvent, WebhookHttp};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};

/// How often the worker looks at the shutdown flag while idle.
const WEBHOOK_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
pub(crate) struct WebhookStats {
    sent: u64,
    failed: u64,
    dropped: u64,
    last_sent_unix: Option<u64>,
    last_event: Option<WebhookEvent>,
    last_error: Option<String>,
    last_error_unix: Option<u64>,
}

pub(crate) struct Webhook {
    http: Arc<dyn WebhookHttp>,
    /// The worker's queue, once it runs.
    tx: Mutex<Option<SyncSender<IssueEvent>>>,
    queued: AtomicU64,
    stats: Mutex<WebhookStats>,
    /// The first wait between tries (doubled for the next).
    backoff: Duration,
}

impl Webhook {
    pub(crate) fn new(http: Arc<dyn WebhookHttp>, backoff: Duration) -> Self {
        Self {
            http,
            tx: Mutex::new(None),
            queued: AtomicU64::new(0),
            stats: Mutex::new(WebhookStats::default()),
            backoff,
        }
    }
}

/// `GET /v1/webhook`. Never holds the URL itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookStatus {
    /// This build can send (the `network` feature).
    pub network: bool,
    /// `webhook.url` is set.
    pub configured: bool,
    /// Scheme, host and port of `webhook.url`, the rest `[redacted]`; empty
    /// when none is set.
    pub url: String,
    pub format: webhook::WebhookFormat,
    pub events: Vec<WebhookEvent>,
    pub min_severity: crate::consumers::Severity,
    pub include_host: bool,
    /// This engine sends: it holds the lock and was built with network
    /// support.
    pub sending: bool,
    /// Events waiting to be sent.
    pub queued: u64,
    pub sent: u64,
    pub failed: u64,
    /// Events dropped because the queue was full.
    pub dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<WebhookEvent>,
    /// Why the last send failed, with the URL redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_unix: Option<u64>,
    /// What is sent, in words.
    pub sends: String,
}

pub const SENDS: &str = "Nothing unless webhook.url is set. Then, for each issue event named in webhook.events (new_issue, regressed, resolved_by_user) at or above webhook.min_severity: one POST to that URL with the event name, the issue's id, kind, category, severity, title, culprit, workspace, occurrences, status, regressions and first and last seen times, the commit range with sessions and agents for performance issues, and the host name only with webhook.include_host. With format slack, one line of the same as {\"text\": ...}. No redirects followed, 10 s timeout, at most 3 tries.";

/// The real client; in this crate's tests none (a test that sends passes a
/// fake or a client of its own).
pub(crate) fn webhook_http() -> Arc<dyn WebhookHttp> {
    if cfg!(test) {
        Arc::new(webhook::NoNetwork)
    } else {
        webhook::default_http()
    }
}

impl Engine {
    /// Use `http` for the webhook, waiting `backoff` (then twice that)
    /// between tries (tests use a fake and no wait).
    pub fn with_webhook_http(mut self, http: Arc<dyn WebhookHttp>, backoff: Duration) -> Self {
        self.webhook = Arc::new(Webhook::new(http, backoff));
        self
    }

    /// On the lock holder with network support: start the worker.
    pub(crate) fn start_webhook(&self, threads: &mut Vec<JoinHandle<()>>) {
        if !self.automation_enabled.load(Ordering::SeqCst) || !webhook::network_built() {
            return;
        }
        let (tx, rx) = sync_channel::<IssueEvent>(webhook::QUEUE);
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-webhook".into())
            .spawn(move || me.webhook_loop(rx))
        {
            *lock(&self.webhook.tx) = Some(tx);
            threads.push(h);
        }
    }

    fn webhook_loop(&self, rx: Receiver<IssueEvent>) {
        while !self.is_shut_down() {
            match rx.recv_timeout(WEBHOOK_POLL) {
                Ok(ev) => {
                    self.webhook.queued.fetch_sub(1, Ordering::SeqCst);
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        self.webhook_send(&ev, webhook::ATTEMPTS)
                    }));
                    if r.is_err() {
                        self.log(
                            "error",
                            "webhook send failed unexpectedly; later events are still sent".into(),
                            0,
                            None,
                            "webhook",
                        );
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        *lock(&self.webhook.tx) = None;
    }

    /// Queue `ev` for the worker. Not running (another engine holds the
    /// lock, or no network support): nothing happens. Full: dropped and
    /// counted.
    fn webhook_enqueue(&self, ev: IssueEvent) {
        let g = lock(&self.webhook.tx);
        let Some(tx) = g.as_ref() else {
            return;
        };
        self.webhook.queued.fetch_add(1, Ordering::SeqCst);
        let full = match tx.try_send(ev) {
            Ok(()) => false,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => true,
        };
        // One lock at a time: never `stats` while holding `tx`.
        drop(g);
        if full {
            self.webhook.queued.fetch_sub(1, Ordering::SeqCst);
            lock(&self.webhook.stats).dropped += 1;
        }
    }

    /// Queue `event` about each of `issues` the config asks for. The
    /// current findings give performance issues their commit range.
    pub(crate) fn webhook_issues(
        &self,
        event: WebhookEvent,
        issues: &[Issue],
        findings: &[Finding],
    ) {
        if issues.is_empty() || lock(&self.webhook.tx).is_none() {
            return;
        }
        let cfg = self.config().webhook;
        for i in issues {
            if !webhook::wanted(&cfg, event, i) {
                continue;
            }
            let f = findings.iter().find(|f| f.id == i.finding_id);
            self.webhook_enqueue(IssueEvent::new(event, i.clone(), f));
        }
    }

    /// Post `ev` now, on the calling thread, with up to `attempts` tries,
    /// and record the outcome. The URL is read from the config here.
    fn webhook_send(&self, ev: &IssueEvent, attempts: u32) -> Result<u16, String> {
        let cfg = self.config().webhook;
        if cfg.url.is_empty() || webhook::is_redacted(&cfg.url) {
            return Err("webhook.url is not set".into());
        }
        let host = if cfg.include_host {
            webhook::host_name()
        } else {
            None
        };
        let body = webhook::body(cfg.format, ev, host.as_deref());
        let backoff = self.webhook.backoff;
        let me = self.clone();
        let r = webhook::deliver(
            self.webhook.http.as_ref(),
            &cfg.url,
            &body,
            attempts,
            backoff,
            &move |d| me.nap(d),
        );
        let now = now_unix();
        let failed_before = {
            let mut s = lock(&self.webhook.stats);
            let failing = s.last_error.is_some();
            match &r {
                Ok(_) => {
                    s.sent += 1;
                    s.last_sent_unix = Some(now);
                    s.last_event = Some(ev.event);
                    s.last_error = None;
                    s.last_error_unix = None;
                }
                Err(e) => {
                    s.failed += 1;
                    s.last_error = Some(webhook::scrub(e, &cfg.url));
                    s.last_error_unix = Some(now);
                }
            }
            failing
        };
        // One journal line when sending starts failing, not one per event.
        if let Err(e) = &r {
            if !failed_before {
                self.log(
                    "webhook",
                    format!("Webhook failed: {}", webhook::scrub(e, &cfg.url)),
                    0,
                    None,
                    "webhook",
                );
            }
        }
        r.map_err(|e| webhook::scrub(&e, &cfg.url))
    }

    /// `POST /v1/webhook/test`: post one test event now (one try), from the
    /// engine that sends. Refused with no URL, no network support, or on an
    /// engine that does not hold the lock.
    pub fn webhook_test(&self) -> Result<u16, (u16, &'static str, String)> {
        if !webhook::network_built() {
            return Err((409, "no_network", webhook::NO_NETWORK.into()));
        }
        if self.config().webhook.url.is_empty() {
            return Err((
                409,
                "webhook_off",
                "webhook.url is not set; set it with `mulchie config set webhook.url <url>`".into(),
            ));
        }
        if lock(&self.webhook.tx).is_none() {
            return Err((
                409,
                "not_lock_holder",
                "this engine does not send webhooks; the engine that holds the engine lock does"
                    .into(),
            ));
        }
        self.webhook_send(&IssueEvent::test(), 1)
            .map_err(|e| (502, "webhook_failed", e))
    }

    /// `GET /v1/webhook`.
    pub fn webhook_status(&self) -> WebhookStatus {
        let cfg = self.config().webhook;
        // Not under the stats lock: enqueueing takes `tx`, then `stats`.
        let sending = lock(&self.webhook.tx).is_some();
        let s = lock(&self.webhook.stats);
        WebhookStatus {
            network: webhook::network_built(),
            configured: !cfg.url.is_empty(),
            url: webhook::redact_url(&cfg.url),
            format: cfg.format,
            events: cfg.events.clone(),
            min_severity: cfg.min_severity,
            include_host: cfg.include_host,
            sending,
            queued: self.webhook.queued.load(Ordering::SeqCst),
            sent: s.sent,
            failed: s.failed,
            dropped: s.dropped,
            last_sent_unix: s.last_sent_unix,
            last_event: s.last_event,
            last_error: s.last_error.clone(),
            last_error_unix: s.last_error_unix,
            sends: SENDS.into(),
        }
    }

    /// Start the worker whatever the lock and the build (tests).
    #[cfg(test)]
    pub(crate) fn webhook_start_for_test(&self) -> JoinHandle<()> {
        let (tx, rx) = sync_channel::<IssueEvent>(webhook::QUEUE);
        *lock(&self.webhook.tx) = Some(tx);
        let me = self.clone();
        std::thread::spawn(move || me.webhook_loop(rx))
    }
}

#[cfg(test)]
#[path = "engine_webhook_tests.rs"]
mod tests;
