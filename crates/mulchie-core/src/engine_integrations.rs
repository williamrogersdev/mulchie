//! Integrations on their own thread (`mulchie-integrations`), started only
//! by the engine that holds the lock. Each enabled provider is polled no
//! sooner than a minute after start, then every `integrations.interval_mins`
//! (doubling after each failed poll, up to a day); `POST
//! /v1/integrations/check` asks for a poll now. Deploys and each provider's
//! status (never its token) are cached in `<config dir>/integrations.json`.
//! The `production_slower` findings are rebuilt after a poll, when the
//! settings change and when new commits were recorded. Nothing here acts on
//! a finding.

use super::*;
use crate::config::IntegrationsConfig;
use crate::integrations::{self, AuthHttp, Cache, Deploy, Provider, ProviderCache};
use crate::watch::{FindingCategory, FindingKind};

/// How many commits are recorded and the newest one.
type CommitMark = (usize, Option<(u64, String, PathBuf)>);

/// The worker wakes at least this often.
const INTEGRATIONS_POLL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct IntegrationState {
    cache: Option<Cache>,
    checking: bool,
    manual: bool,
    started_unix: u64,
    /// What the stored findings were built from: settings, polls, commits.
    published: Option<(IntegrationsConfig, u64, CommitMark)>,
    /// Bumped after every poll.
    polls: u64,
}

pub(crate) struct Integrations {
    http: Arc<dyn AuthHttp>,
    pub(crate) state: Mutex<IntegrationState>,
    pub(crate) wake: Condvar,
}

impl Integrations {
    pub(crate) fn new(http: Arc<dyn AuthHttp>) -> Self {
        Self {
            http,
            state: Mutex::new(IntegrationState {
                started_unix: now_unix(),
                ..IntegrationState::default()
            }),
            wake: Condvar::new(),
        }
    }
}

/// One provider in `GET /v1/integrations`. Never holds a token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderStatus {
    pub provider: Provider,
    pub enabled: bool,
    /// The API host requests go to.
    pub host: String,
    /// The project identifiers from the config, in words.
    pub project: String,
    /// A token file is in place (its contents are never shown).
    pub token: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix: Option<u64>,
    /// Why the last poll failed, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_poll_unix: Option<u64>,
    pub deploys: usize,
}

/// `GET /v1/integrations`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntegrationsStatus {
    /// This build can reach providers (the `network` feature).
    pub network: bool,
    pub interval_mins: u64,
    pub checking: bool,
    pub providers: Vec<ProviderStatus>,
    /// Open `production_slower` findings.
    pub slower: usize,
    /// What goes over the network, in words.
    pub sends: String,
}

/// One deploy in `GET /v1/deploys`, with what it contained.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployView {
    #[serde(flatten)]
    pub deploy: Deploy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_deploy_id: Option<String>,
    #[serde(flatten)]
    pub attribution: integrations::Attribution,
}

/// `GET /v1/deploys`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployList {
    pub deploys: Vec<DeployView>,
}

pub const SENDS: &str = "Only for providers you turned on: authenticated read requests over HTTPS to that provider's own API host (api.vercel.com, your Sentry host, your PostHog host), with no redirects followed, carrying the token and the project identifiers from the config. Nothing about this machine is sent.";

fn project_words(p: Provider, c: &IntegrationsConfig) -> String {
    match p {
        Provider::Vercel if c.vercel.team.is_empty() => c.vercel.project.clone(),
        Provider::Vercel => format!("{} (team {})", c.vercel.project, c.vercel.team),
        Provider::Sentry => format!("{}/{}", c.sentry.org, c.sentry.project),
        Provider::Posthog => format!("project {}", c.posthog.project),
    }
}

impl Engine {
    /// Use `http` for integrations (tests use a fake).
    pub fn with_integration_http(mut self, http: Arc<dyn AuthHttp>) -> Self {
        self.integrations = Arc::new(Integrations::new(http));
        self
    }

    fn with_int<R>(&self, f: impl FnOnce(&mut IntegrationState) -> R) -> R {
        let mut g = lock(&self.integrations.state);
        if g.cache.is_none() {
            g.cache = Some(integrations::load_cache(&self.paths));
        }
        f(&mut g)
    }

    /// On the lock holder: start the worker.
    pub(crate) fn start_integrations(&self, threads: &mut Vec<JoinHandle<()>>) {
        if !self.automation_enabled.load(Ordering::SeqCst) {
            return;
        }
        self.with_int(|s| s.started_unix = now_unix());
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-integrations".into())
            .spawn(move || {
                while !me.is_shut_down() {
                    let r = catch_unwind(AssertUnwindSafe(|| me.integrations_tick(now_unix())));
                    if r.is_err() {
                        me.log(
                            "error",
                            "integrations poll failed; retrying later".into(),
                            0,
                            None,
                            "integrations",
                        );
                        lock(&me.integrations.state).checking = false;
                    }
                    let st = lock(&me.integrations.state);
                    if !st.manual && !me.is_shut_down() {
                        let _ = me.integrations.wake.wait_timeout(st, INTEGRATIONS_POLL);
                    }
                }
            })
        {
            threads.push(h);
        }
    }

    /// One worker step at `now`: poll the providers that are due, then
    /// rebuild the findings when anything they depend on changed.
    pub(crate) fn integrations_tick(&self, now: u64) {
        let cfg = self.config().integrations;
        let (manual, started) = self.with_int(|s| {
            let m = s.manual;
            s.manual = false;
            (m, s.started_unix)
        });
        for p in Provider::ALL {
            if !p.enabled(&cfg) {
                continue;
            }
            let due = manual
                || (now >= started + integrations::FIRST_POLL_DELAY_SECS
                    && self.with_int(|s| {
                        now >= s
                            .cache
                            .as_ref()
                            .and_then(|c| c.providers.get(&p))
                            .map(|c| c.next_unix)
                            .unwrap_or(0)
                    }));
            if due {
                self.poll_provider(p, &cfg, now);
            }
        }
        self.publish_integration_findings(&cfg, now);
    }

    /// Poll one provider now (on the calling thread) and cache the result.
    pub(crate) fn poll_provider(&self, p: Provider, cfg: &IntegrationsConfig, now: u64) {
        self.with_int(|s| s.checking = true);
        let result = match integrations::read_token(&self.paths, p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                "no token for {0}: run `mulchie integrations add {0}`",
                p.name()
            )),
            Err(e) => Err(format!("the {} token was not used: {e}", p.name())),
            Ok(token) => {
                let src = p.source(cfg);
                integrations::poll(&*src, &*self.integrations.http, &token, now)
                    .map_err(|e| integrations::scrub(&e, &token))
            }
        };
        let interval = cfg.interval_mins.saturating_mul(60);
        let cache = self.with_int(|s| {
            s.checking = false;
            s.polls += 1;
            let c = s.cache.get_or_insert_with(Cache::default);
            let pc: &mut ProviderCache = c.providers.entry(p).or_default();
            pc.last_attempt_unix = Some(now);
            match &result {
                Ok(d) => {
                    pc.last_success_unix = Some(now);
                    pc.last_error = None;
                    pc.failures = 0;
                    pc.deploys = d.clone();
                }
                Err(e) => {
                    pc.last_error = Some(e.clone());
                    pc.failures = pc.failures.saturating_add(1);
                }
            }
            pc.next_unix = integrations::next_poll(now, interval, pc.failures);
            c.clone()
        });
        if let Err(e) = &result {
            self.log(
                "error",
                format!("{} poll failed: {e}", p.title()),
                0,
                None,
                "integrations",
            );
        }
        if let Err(e) = integrations::save_cache(&self.paths, &cache) {
            self.log(
                "error",
                format!("Could not save integration results: {e}"),
                0,
                None,
                "integrations",
            );
        }
    }

    /// Deploys of the enabled providers, newest first.
    fn enabled_deploys(&self, cfg: &IntegrationsConfig) -> Vec<(Provider, Vec<Deploy>)> {
        self.with_int(|s| {
            Provider::ALL
                .into_iter()
                .filter(|p| p.enabled(cfg))
                .map(|p| {
                    let d = s
                        .cache
                        .as_ref()
                        .and_then(|c| c.providers.get(&p))
                        .map(|c| c.deploys.clone())
                        .unwrap_or_default();
                    (p, d)
                })
                .collect()
        })
    }

    /// Store the `production_slower` findings again when the settings, a
    /// poll or the commit log changed since they were built.
    pub(crate) fn publish_integration_findings(&self, cfg: &IntegrationsConfig, now: u64) {
        // The log stops growing at its cap, so its newest entry tells a new
        // commit apart too.
        let commits = self.with_commit_log(|l| {
            (
                l.len(),
                l.last()
                    .map(|e| (e.at_unix, e.head.clone(), e.workspace.clone())),
            )
        });
        let polls = self.with_int(|s| s.polls);
        let key = (cfg.clone(), polls, commits);
        let fresh = self.with_int(|s| s.published.as_ref() == Some(&key));
        if fresh {
            return;
        }
        let deploys = self.enabled_deploys(cfg);
        let mut cfg_real = cfg.clone();
        for w in [
            &mut cfg_real.vercel.workspace,
            &mut cfg_real.sentry.workspace,
            &mut cfg_real.posthog.workspace,
        ] {
            if let Some(p) = w.as_mut() {
                *p = Engine::real_workspace(p);
            }
        }
        let found =
            self.with_commit_log(|log| integrations::findings(&deploys, log, &cfg_real, now));
        self.replace_findings_of(
            FindingCategory::Performance,
            &[FindingKind::ProductionSlower],
            found,
            now,
        );
        self.with_int(|s| s.published = Some(key));
    }

    /// Ask the worker to poll every enabled provider now. Refused when no
    /// provider is on, in a build without network support, and within a
    /// minute of the last poll.
    pub fn request_integrations_check(&self) -> Result<(), (&'static str, String)> {
        let cfg = self.config().integrations;
        if !cfg.any_enabled() {
            return Err((
                "integrations_off",
                "no integration is turned on (integrations.<provider>.enabled); nothing is checked"
                    .into(),
            ));
        }
        if !integrations::network_built() {
            return Err((
                "no_network",
                format!("{}; nothing is checked", integrations::NO_NETWORK),
            ));
        }
        let now = now_unix();
        let last = self.with_int(|s| {
            s.cache
                .as_ref()
                .map(|c| {
                    c.providers
                        .values()
                        .filter_map(|p| p.last_attempt_unix)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        });
        if now < last + integrations::MIN_MANUAL_GAP_SECS && last <= now {
            return Err((
                "too_soon",
                format!(
                    "the last poll was {}s ago; ask again in a minute",
                    now - last
                ),
            ));
        }
        lock(&self.integrations.state).manual = true;
        self.integrations.wake.notify_all();
        Ok(())
    }

    /// `GET /v1/integrations`.
    pub fn integrations_status(&self) -> IntegrationsStatus {
        let cfg = self.config().integrations;
        let slower = lock(&self.state)
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::ProductionSlower && !f.dismissed)
            .count();
        let (checking, started, cache) = self.with_int(|s| {
            (
                s.checking,
                s.started_unix,
                s.cache.clone().unwrap_or_default(),
            )
        });
        let providers = Provider::ALL
            .into_iter()
            .map(|p| {
                let c = cache.providers.get(&p).cloned().unwrap_or_default();
                let enabled = p.enabled(&cfg);
                ProviderStatus {
                    provider: p,
                    enabled,
                    host: p.host(&cfg),
                    project: project_words(p, &cfg),
                    token: integrations::token_present(&self.paths, p),
                    last_attempt_unix: c.last_attempt_unix,
                    last_success_unix: c.last_success_unix,
                    last_error: c.last_error.clone(),
                    failures: c.failures,
                    next_poll_unix: enabled.then(|| {
                        c.next_unix
                            .max(started + integrations::FIRST_POLL_DELAY_SECS)
                    }),
                    deploys: c.deploys.len(),
                }
            })
            .collect();
        IntegrationsStatus {
            network: integrations::network_built(),
            interval_mins: cfg.interval_mins,
            checking,
            providers,
            slower,
            sends: SENDS.into(),
        }
    }

    /// `GET /v1/deploys`: the cached deploys of enabled providers (of
    /// `only` when given), newest first, each with the commits in it.
    pub fn deploys(&self, only: Option<Provider>) -> DeployList {
        let cfg = self.config().integrations;
        let lists: Vec<(Provider, Vec<Deploy>)> = self
            .enabled_deploys(&cfg)
            .into_iter()
            .filter(|(p, _)| only.is_none_or(|o| o == *p))
            .collect();
        let deploys = self.with_commit_log(|log| {
            let mut out = Vec::new();
            for (p, list) in &lists {
                let ws = p.workspace(&cfg).map(|w| Engine::real_workspace(&w));
                for (i, d) in list.iter().enumerate() {
                    let prev = list.get(i + 1);
                    out.push(DeployView {
                        deploy: d.clone(),
                        previous_deploy_id: prev.map(|d| d.id.clone()),
                        attribution: integrations::attribute(
                            log,
                            ws.as_deref(),
                            prev.and_then(|d| d.sha.as_deref()),
                            d.sha.as_deref(),
                        ),
                    });
                }
            }
            out
        });
        DeployList { deploys }
    }
}

#[cfg(test)]
#[path = "engine_integrations_tests.rs"]
mod tests;
