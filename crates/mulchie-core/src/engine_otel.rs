//! The OpenTelemetry trace receiver ([`crate::otel_server`]) and its
//! endpoint findings ([`crate::endpoints`]), through the engine.
//!
//! Only the engine that holds the lock runs the receiver, and only while
//! `otel.enabled` is on. The `mulchie-perf` worker starts and stops it when
//! the setting or `otel.port` changes (a config save wakes the worker), and
//! remembers why it could not start (a posture finding,
//! `otel_unavailable`); it tries a taken port again at most once a minute.
//! Turning it off drops what was aggregated.
//!
//! Once a minute the worker finds where each service runs (its
//! `process.pid`, when that process belongs to this user, through the
//! process's working directory to the checkout; else its
//! `vcs.ref.head.revision` matched against recorded commits), reads that
//! checkout's `HEAD` (recording a move in `commits.jsonl` as the perf worker
//! does), and stores the `endpoint_slower` and `endpoint_errors` findings
//! next to `build_slower`. The receiver thread only feeds the aggregator;
//! `GET /v1/otel` reads it and the cached attribution, with no git and no
//! process table. Nothing here signals, deletes or changes anything else.

use super::*;
use crate::commits;
use crate::endpoints::{self, Attribution, Baseline, EndpointEvidence, EndpointThresholds};
use crate::otel::{self, Aggregator, Drops, OperationSummary, Resource, Stats};
use crate::otel_server::{self, Server, Sink};
use crate::watch::FindingKind;
use std::net::SocketAddr;

/// How often endpoints are assessed and services attributed.
const EVAL_MS: u64 = 60_000;
/// A port that could not be bound is tried again after this long.
const RETRY_SECS: u64 = 60;
/// Services attributed in one assessment, at most.
const MAX_ATTRIBUTED: usize = 32;

pub(crate) struct Otel {
    agg: Arc<Mutex<Aggregator>>,
    inner: Mutex<OtelInner>,
}

impl Default for Otel {
    fn default() -> Self {
        Self {
            agg: Arc::new(Mutex::new(Aggregator::default())),
            inner: Mutex::new(OtelInner::default()),
        }
    }
}

#[derive(Default)]
struct OtelInner {
    server: Option<Server>,
    /// The configured port the running server was started for (0: any).
    started_for: Option<u16>,
    /// The configured port that could not be bound, and why.
    error: Option<(u16, String)>,
    error_at: u64,
    /// Where each service runs, from the last assessment.
    attrib: HashMap<String, Attribution>,
    last_eval_ms: Option<u64>,
    /// The endpoint findings of the last assessment.
    findings: Vec<Finding>,
    /// Something was aggregated since the receiver was last turned off.
    used: bool,
}

/// The receiver in a few words, in `GET /v1/state`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OtelBrief {
    /// `otel.enabled`.
    pub enabled: bool,
    /// This engine's receiver is listening.
    pub listening: bool,
    /// The port it listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Why it is not listening while enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Services that sent traces in the last 24 hours.
    pub services: usize,
    /// Endpoints slower, or failing more, than usual.
    pub endpoints_slower: usize,
    pub endpoints_failing: usize,
}

/// One operation of a service, in `GET /v1/otel`.
#[derive(Debug, Clone, Serialize)]
pub struct OperationReport {
    pub operation: String,
    /// Over the requested range.
    #[serde(flatten)]
    pub stats: Stats,
    pub error_rate: f64,
    /// The usual figures, from the windows before the range; `None` without
    /// enough busy windows.
    pub baseline: Option<Baseline>,
    /// Slower, or failing more, than the baseline.
    pub slower: bool,
    pub more_errors: bool,
    /// As last seen in the range.
    pub resource: Resource,
}

/// One service, in `GET /v1/otel`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceReport {
    pub name: String,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub resource: Resource,
    /// Where it runs, from the last assessment (once a minute); `None` when
    /// not found.
    pub attribution: Option<Attribution>,
    /// Operations with requests in the range, slowest p95 first.
    pub operations: Vec<OperationReport>,
}

/// `GET /v1/otel`.
#[derive(Debug, Clone, Serialize)]
pub struct OtelReport {
    /// `otel.enabled`.
    pub enabled: bool,
    /// This engine's receiver is listening.
    pub listening: bool,
    /// The port it listens on (with `otel.port` 0, the one picked).
    pub port: Option<u16>,
    /// `otel.port`.
    pub configured_port: u16,
    /// The URL to export to (`OTEL_EXPORTER_OTLP_ENDPOINT`).
    pub endpoint: Option<String>,
    /// Why it is not listening while enabled.
    pub error: Option<String>,
    /// The range the operations' figures cover.
    pub minutes: u64,
    pub thresholds: EndpointThresholds,
    pub received_spans: u64,
    pub recorded_spans: u64,
    /// Spans that are neither server, consumer nor root: not aggregated.
    pub ignored_spans: u64,
    pub drops: Drops,
    pub services: Vec<ServiceReport>,
}

/// Each operation with requests since `from_ms`, its baseline (windows
/// before `from_ms`) and whether it is slower or failing more.
fn assess(
    agg: &Aggregator,
    from_ms: u64,
    t: &EndpointThresholds,
) -> Vec<(OperationSummary, Option<Baseline>, bool, bool)> {
    agg.operations(from_ms, u64::MAX)
        .into_iter()
        .map(|op| {
            let w = agg.windows(&op.service, &op.operation);
            let base = endpoints::baseline(&w, from_ms, t);
            let (slower, errors) = match &base {
                Some(b) => (
                    endpoints::is_slower(&op.stats, b, t),
                    endpoints::has_more_errors(&op.stats, b, t),
                ),
                None => (false, false),
            };
            (op, base, slower, errors)
        })
        .collect()
}

impl Engine {
    fn otel_thresholds(cfg: &Config) -> EndpointThresholds {
        EndpointThresholds::of(&cfg.perf, &cfg.otel)
    }

    /// Start, restart or stop the receiver to match the config. Only the
    /// engine that holds the lock runs it.
    pub(crate) fn otel_reconcile(&self, now: u64) {
        let cfg = self.config().otel;
        let want = cfg.enabled && self.keeps_perf();
        let mut g = lock(&self.otel.inner);
        if !want {
            if let Some(s) = g.server.take() {
                s.stop();
                self.log("otel", "trace receiver stopped".into(), 0, None, "otel");
            }
            g.started_for = None;
            g.error = None;
            g.attrib.clear();
            g.findings.clear();
            g.last_eval_ms = None;
            if g.used {
                g.used = false;
                *lock(&self.otel.agg) = Aggregator::default();
            }
            return;
        }
        if g.server.is_some() && g.started_for == Some(cfg.port) {
            return;
        }
        if let Some(s) = g.server.take() {
            s.stop();
        }
        if let Some((p, _)) = &g.error {
            if *p == cfg.port && now >= g.error_at && now < g.error_at + RETRY_SECS {
                return;
            }
        }
        let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
        let sink: Arc<dyn Sink> = self.otel.agg.clone();
        match otel_server::serve(addr, sink, otel_server::Limits::default()) {
            Ok(s) => {
                self.log(
                    "otel",
                    format!("trace receiver listening on {}", s.local_addr()),
                    0,
                    None,
                    "otel",
                );
                g.server = Some(s);
                g.started_for = Some(cfg.port);
                g.error = None;
                g.used = true;
            }
            Err(e) => {
                if g.error.as_ref().map(|x| x.0) != Some(cfg.port) {
                    self.log(
                        "error",
                        format!("trace receiver could not listen on {addr}: {e}"),
                        0,
                        None,
                        "otel",
                    );
                }
                g.error = Some((cfg.port, e.to_string()));
                g.error_at = now;
            }
        }
    }

    /// Stop the receiver (shutdown).
    pub(crate) fn otel_stop(&self) {
        let s = lock(&self.otel.inner).server.take();
        if let Some(s) = s {
            s.stop();
        }
    }

    /// The bound port while listening, and the configured port with the
    /// error while it could not start: for the posture check.
    pub(crate) fn otel_posture(&self) -> (Option<u16>, Option<(u16, String)>) {
        let g = lock(&self.otel.inner);
        (g.server.as_ref().map(Server::port), g.error.clone())
    }

    /// Where the service with `resource` runs, and who moved its code
    /// last. Reads the process table and runs `git rev-parse`: the worker
    /// only.
    fn attribute_service(&self, resource: &Resource, now: u64) -> Option<Attribution> {
        let by_pid = resource
            .pid
            .filter(|p| *p > 1 && *p <= i64::from(u32::MAX))
            .and_then(|p| self.procs.get(p as u32))
            .filter(|p| p.uid == Some(self.procs.current_uid()))
            .and_then(|p| p.cwd)
            .and_then(|cwd| perf_glue::checkout_root(&cwd))
            .map(|ws| (ws, "pid"));
        let found = by_pid.or_else(|| {
            let rev = resource.vcs_revision.as_deref()?;
            self.with_perf(|p| commits::workspace_of(p.commits(), rev))
                .map(|ws| (ws, "revision"))
        });
        let (ws, via) = found?;
        let head = self.check_head(&ws, None, "otel", now);
        let sha = resource
            .vcs_revision
            .clone()
            .or_else(|| head.as_ref().map(|h| h.sha.clone()));
        let who = sha.as_deref().and_then(|s| self.attribute(&ws, s));
        Some(Attribution {
            workspace: Some(ws),
            branch: head.and_then(|h| h.branch),
            head: sha,
            session_id: who.as_ref().and_then(|c| c.session_id.clone()),
            agent: who.and_then(|c| c.agent),
            via: via.into(),
        })
    }

    /// Assess every endpoint once a minute: attribute the services and
    /// build the endpoint findings. `Some(findings)` when it ran.
    pub(crate) fn otel_tick(&self, now: u64) -> Option<Vec<Finding>> {
        let cfg = self.config();
        let now_ms = now.saturating_mul(1000);
        {
            let mut g = lock(&self.otel.inner);
            g.server.as_ref()?;
            let due = g
                .last_eval_ms
                .is_none_or(|l| now_ms >= l + EVAL_MS || now_ms < l);
            if !due {
                return None;
            }
            g.last_eval_ms = Some(now_ms);
        }
        let t = Self::otel_thresholds(&cfg);
        let from = now_ms.saturating_sub(endpoints::RECENT_MS);
        let (assessed, services) = {
            let mut a = lock(&self.otel.agg);
            a.prune(now_ms);
            (assess(&a, from, &t), a.services())
        };
        let mut attrib = HashMap::new();
        let mut recent: Vec<_> = services;
        recent.sort_by_key(|s| std::cmp::Reverse(s.last_seen_ms));
        for s in recent.iter().take(MAX_ATTRIBUTED) {
            if let Some(a) = self.attribute_service(&s.resource, now) {
                attrib.insert(s.name.clone(), a);
            }
        }
        let mut out = Vec::new();
        for (op, base, slower, errors) in assessed {
            let Some(base) = base else { continue };
            if !slower && !errors {
                continue;
            }
            let a = attrib.get(&op.service).cloned();
            let ws = a.as_ref().and_then(|a| a.workspace.clone());
            let to_rev = op
                .resource
                .vcs_revision
                .clone()
                .or_else(|| a.as_ref().and_then(|a| a.head.clone()));
            let (from_rev, moves, cut) = self.with_perf(|p| {
                let from_rev = base.revision.clone().or_else(|| {
                    ws.as_deref()
                        .and_then(|w| commits::head_at(p.commits(), w, base.to_ms / 1000))
                });
                let (moves, cut) = endpoints::commit_range(
                    p.commits(),
                    ws.as_deref(),
                    from_rev.as_deref(),
                    to_rev.as_deref(),
                );
                (from_rev, moves, cut)
            });
            let ev = EndpointEvidence {
                service: op.service.clone(),
                operation: op.operation.clone(),
                recent_minutes: endpoints::RECENT_MS / 60_000,
                recent_count: op.stats.count,
                recent_errors: op.stats.errors,
                recent_p95_ms: op.stats.p95_ms,
                recent_error_rate: endpoints::error_rate(&op.stats),
                baseline_p95_ms: base.p95_ms,
                baseline_error_rate: base.error_rate,
                baseline_windows: base.windows,
                ratio: op.stats.p95_ms / base.p95_ms.max(f64::MIN_POSITIVE),
                service_version: op.resource.service_version.clone(),
                environment: op.resource.deployment_environment.clone(),
                attribution: a,
                from_revision: from_rev,
                to_revision: to_rev,
                commits: moves,
                commits_truncated: cut,
            };
            let first = from / 1000;
            if slower {
                out.push(endpoints::finding(
                    FindingKind::EndpointSlower,
                    ev.clone(),
                    &t,
                    first,
                    now,
                ));
            }
            if errors {
                out.push(endpoints::finding(
                    FindingKind::EndpointErrors,
                    ev,
                    &t,
                    first,
                    now,
                ));
            }
        }
        let mut g = lock(&self.otel.inner);
        g.attrib = attrib;
        g.findings = out.clone();
        Some(out)
    }

    /// The endpoint findings of the last assessment.
    pub(crate) fn otel_findings(&self) -> Vec<Finding> {
        lock(&self.otel.inner).findings.clone()
    }

    /// Workspaces the services run from, for the periodic `HEAD` check.
    pub(crate) fn otel_workspaces(&self) -> Vec<PathBuf> {
        lock(&self.otel.inner)
            .attrib
            .values()
            .filter_map(|a| a.workspace.clone())
            .collect()
    }

    /// `state.otel`.
    pub(crate) fn otel_brief(&self, cfg: &Config) -> OtelBrief {
        let (port, error, slower, failing) = {
            let g = lock(&self.otel.inner);
            let count = |k: FindingKind| g.findings.iter().filter(|f| f.kind == k).count();
            (
                g.server.as_ref().map(Server::port),
                g.error.as_ref().map(|e| e.1.clone()),
                count(FindingKind::EndpointSlower),
                count(FindingKind::EndpointErrors),
            )
        };
        let services = if port.is_some() {
            lock(&self.otel.agg).status().services
        } else {
            0
        };
        OtelBrief {
            enabled: cfg.otel.enabled,
            listening: port.is_some(),
            port,
            error,
            services,
            endpoints_slower: slower,
            endpoints_failing: failing,
        }
    }

    /// `GET /v1/otel`: the receiver's status and each service's operations
    /// over the last `minutes`, with their baselines and attribution. Only
    /// `service` when given. No git and no process table here.
    pub fn otel_report(&self, service: Option<&str>, minutes: u64) -> OtelReport {
        let cfg = self.config();
        let t = Self::otel_thresholds(&cfg);
        let now_ms = otel::now_ms();
        let from = now_ms.saturating_sub(minutes.saturating_mul(60_000));
        let (port, error, attrib) = {
            let g = lock(&self.otel.inner);
            (
                g.server.as_ref().map(Server::port),
                g.error.as_ref().map(|e| e.1.clone()),
                g.attrib.clone(),
            )
        };
        let (status, services, assessed) = {
            let a = lock(&self.otel.agg);
            (a.status(), a.services(), assess(&a, from, &t))
        };
        let mut out: Vec<ServiceReport> = services
            .into_iter()
            .filter(|s| service.is_none_or(|w| s.name == w))
            .map(|s| ServiceReport {
                attribution: attrib.get(&s.name).cloned(),
                name: s.name,
                first_seen_ms: s.first_seen_ms,
                last_seen_ms: s.last_seen_ms,
                resource: s.resource,
                operations: Vec::new(),
            })
            .collect();
        for (op, base, slower, more_errors) in assessed {
            let Some(svc) = out.iter_mut().find(|s| s.name == op.service) else {
                continue;
            };
            svc.operations.push(OperationReport {
                error_rate: endpoints::error_rate(&op.stats),
                operation: op.operation,
                stats: op.stats,
                baseline: base,
                slower,
                more_errors,
                resource: op.resource,
            });
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.last_seen_ms));
        OtelReport {
            enabled: cfg.otel.enabled,
            listening: port.is_some(),
            endpoint: port.map(|p| format!("http://127.0.0.1:{p}")),
            port,
            configured_port: cfg.otel.port,
            error,
            minutes,
            thresholds: t,
            received_spans: status.received_spans,
            recorded_spans: status.recorded_spans,
            ignored_spans: status.ignored_spans,
            drops: status.drops,
            services: out,
        }
    }

    /// Feed a decoded batch straight to the aggregator, at `now_ms`
    /// (tests).
    #[cfg(test)]
    pub(crate) fn otel_ingest(&self, batch: &otel::TraceBatch, now_ms: u64) {
        lock(&self.otel.agg).ingest(batch, now_ms);
    }
}

#[cfg(test)]
#[path = "engine_otel_tests.rs"]
mod tests;
