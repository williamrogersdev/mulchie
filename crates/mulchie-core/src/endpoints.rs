//! Is an endpoint slower, or failing more, than it usually is? The
//! `endpoint_slower` and `endpoint_errors` findings, from the per-window
//! aggregates of [`crate::otel::Aggregator`].
//!
//! The recent figures are the last [`RECENT_MS`] (15 minutes) of one
//! (service, operation), its windows merged: p95 from the merged histogram
//! and the error rate. The baseline is the median of the p95s (and of the
//! error rates) of the 5-minute windows before that, over the retention
//! period (24 hours), counting only windows with at least
//! `otel.min_requests` requests and needing at least
//! [`crate::perf::MIN_BASELINE`] of them. The recent range needs at least
//! `otel.min_requests` requests too.
//!
//! - Slower: the recent p95 is more than `perf.slower_ratio` times the
//!   baseline and more than `otel.min_slowdown_ms` above it
//!   ([`crate::perf::is_slow`]).
//! - Failing more: the recent error rate is more than `perf.slower_ratio`
//!   times the baseline rate and more than [`MIN_ERROR_RATE_INCREASE`]
//!   (5 percentage points) above it.
//!
//! Pure: no clock, no disk, no git. The engine adds where the service runs
//! and the commits in between (`engine_otel.rs`). Nothing acts on a finding.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::commits::{self, CommitEntry};
use crate::config::{OtelConfig, PerfConfig};
use crate::consumers::Severity;
use crate::otel::{Stats, WindowSummary};
use crate::perf::{self, Thresholds};
use crate::watch::{ApiRequest, Evidence, Finding, FindingCategory, FindingKind, Suggestion};

/// The recent range compared with the baseline.
pub const RECENT_MS: u64 = 15 * 60 * 1000;
/// An error rate counts as higher only when it rose by more than this
/// (a fraction: 0.05 is 5 percentage points).
pub const MIN_ERROR_RATE_INCREASE: f64 = 0.05;
/// Commits listed in a finding's evidence, at most (the newest).
pub const MAX_EVIDENCE_COMMITS: usize = crate::timings::MAX_EVIDENCE_COMMITS;

/// When an endpoint counts as slower or failing more.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EndpointThresholds {
    pub ratio: f64,
    pub min_slowdown_ms: f64,
    pub min_requests: u64,
}

impl EndpointThresholds {
    pub fn of(perf: &PerfConfig, otel: &OtelConfig) -> Self {
        Self {
            ratio: perf.slower_ratio,
            min_slowdown_ms: otel.min_slowdown_ms as f64,
            min_requests: otel.min_requests,
        }
    }

    fn p95(&self) -> Thresholds {
        Thresholds {
            baseline_runs: usize::MAX,
            slow_runs: 1,
            ratio: self.ratio,
            min_increase: self.min_slowdown_ms,
        }
    }
}

/// An endpoint's usual figures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// The median of the windows' p95s.
    pub p95_ms: f64,
    /// The median of the windows' error rates (0 to 1).
    pub error_rate: f64,
    /// How many windows the medians are of.
    pub windows: usize,
    /// The start of the first window and the end of the last one.
    pub from_ms: u64,
    pub to_ms: u64,
    /// The VCS revision the app reported in the last baseline window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

fn rate(errors: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        errors as f64 / count as f64
    }
}

/// The error rate of `s` (0 to 1).
pub fn error_rate(s: &Stats) -> f64 {
    rate(s.errors, s.count)
}

/// The baseline from `windows` (oldest first): those starting before
/// `recent_from_ms` with enough requests. `None` with fewer than
/// [`perf::MIN_BASELINE`] of them.
pub fn baseline(
    windows: &[WindowSummary],
    recent_from_ms: u64,
    t: &EndpointThresholds,
) -> Option<Baseline> {
    let used: Vec<&WindowSummary> = windows
        .iter()
        .filter(|w| w.start_ms < recent_from_ms && w.stats.count >= t.min_requests)
        .collect();
    if used.len() < perf::MIN_BASELINE {
        return None;
    }
    let p95s: Vec<f64> = used.iter().map(|w| w.stats.p95_ms).collect();
    let rates: Vec<f64> = used
        .iter()
        .map(|w| rate(w.stats.errors, w.stats.count))
        .collect();
    let last = used[used.len() - 1];
    Some(Baseline {
        p95_ms: perf::median(&p95s)?,
        error_rate: perf::median(&rates)?,
        windows: used.len(),
        from_ms: used[0].start_ms,
        to_ms: last.end_ms,
        revision: last.resource.vcs_revision.clone(),
    })
}

/// Whether `recent` is slower than `base`.
pub fn is_slower(recent: &Stats, base: &Baseline, t: &EndpointThresholds) -> bool {
    recent.count >= t.min_requests && perf::is_slow(recent.p95_ms, base.p95_ms, &t.p95())
}

/// Whether `recent` fails more often than `base`.
pub fn has_more_errors(recent: &Stats, base: &Baseline, t: &EndpointThresholds) -> bool {
    let r = error_rate(recent);
    recent.count >= t.min_requests
        && r > base.error_rate * t.ratio
        && r - base.error_rate > MIN_ERROR_RATE_INCREASE
}

/// Where a service runs and who moved its code last, as the engine found
/// it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    /// The checkout the service runs from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit it runs: the revision the app reports, else the
    /// workspace's `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The agent session working in the workspace when `HEAD` moved to
    /// `head` ([`crate::commits::attribute`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// How the workspace was found: `pid` (the process's working directory)
    /// or `revision` (the app's VCS revision, matched against recorded
    /// commits).
    pub via: String,
}

/// The evidence of an endpoint finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointEvidence {
    /// `service.name`.
    pub service: String,
    /// The HTTP method and route, or the span name.
    pub operation: String,
    /// The recent range, in minutes.
    pub recent_minutes: u64,
    pub recent_count: u64,
    pub recent_errors: u64,
    pub recent_p95_ms: f64,
    pub recent_error_rate: f64,
    pub baseline_p95_ms: f64,
    pub baseline_error_rate: f64,
    pub baseline_windows: usize,
    /// The recent p95 over the baseline p95.
    pub ratio: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Where the service runs, and the session that moved `HEAD` last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
    /// The commit at the end of the baseline and the one running now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_revision: Option<String>,
    /// The recorded moves of `HEAD` between them, oldest first, with the
    /// sessions that were working then (at most [`MAX_EVIDENCE_COMMITS`],
    /// the newest).
    #[serde(default)]
    pub commits: Vec<CommitEntry>,
    #[serde(default)]
    pub commits_truncated: bool,
}

/// `<kind>:<16 hex digits of the service and operation>`.
pub fn finding_id(kind: FindingKind, service: &str, operation: &str) -> String {
    format!(
        "{}:{:016x}",
        kind.code(),
        crate::issues::fnv1a(format!("{service}\u{1f}{operation}").as_bytes())
    )
}

/// `/v1/otel?service=…`.
pub fn otel_route(service: &str) -> String {
    format!("/v1/otel?service={}", crate::api::query_escape(service))
}

/// A duration in milliseconds as a finding shows it: `4.2 ms`, `420 ms`,
/// `1.3 s`.
pub fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

fn pct(r: f64) -> String {
    let p = r * 100.0;
    if p > 0.0 && p < 1.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

fn short(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}

/// The recorded moves of `HEAD` in `ws` from `from` to `to`, at most
/// [`MAX_EVIDENCE_COMMITS`] (the newest), and whether there were more.
pub fn commit_range(
    log: &[CommitEntry],
    ws: Option<&Path>,
    from: Option<&str>,
    to: Option<&str>,
) -> (Vec<CommitEntry>, bool) {
    let mut moves = match (ws, from, to) {
        (Some(w), Some(a), Some(b)) => commits::between(log, w, a, b),
        _ => Vec::new(),
    };
    let truncated = moves.len() > MAX_EVIDENCE_COMMITS;
    if truncated {
        moves.drain(..moves.len() - MAX_EVIDENCE_COMMITS);
    }
    (moves, truncated)
}

/// What changed in the code, in words, for a finding's `why`.
fn changed(ev: &EndpointEvidence) -> String {
    match (&ev.from_revision, &ev.to_revision) {
        (Some(a), Some(b)) if commits::same_commit(a, b) => {
            "The code did not change in between, so the cause is elsewhere: data, a dependency, configuration or a busy machine.".to_string()
        }
        (Some(a), Some(b)) if ev.commits.is_empty() => format!(
            "The code moved from {} to {} in between; Mulchie recorded none of the moves.",
            short(a),
            short(b)
        ),
        (Some(a), Some(b)) => {
            let who: Vec<String> = ev
                .commits
                .iter()
                .filter_map(|m| m.agent.clone().or_else(|| m.session_id.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let by = if who.is_empty() {
                String::new()
            } else {
                format!(" while {} worked there", who.join(", "))
            };
            format!(
                "HEAD moved {} time(s) from {} to {}{by}: see evidence.endpoint.commits.",
                ev.commits.len() + usize::from(ev.commits_truncated),
                short(a),
                short(b)
            )
        }
        _ => "Which commit the service ran before is not known: send vcs.ref.head.revision, or run the app from a workspace Mulchie watches.".to_string(),
    }
}

/// The finding for an endpoint that is slower ([`FindingKind::EndpointSlower`])
/// or failing more ([`FindingKind::EndpointErrors`]).
pub fn finding(
    kind: FindingKind,
    ev: EndpointEvidence,
    t: &EndpointThresholds,
    first_seen: u64,
    now: u64,
) -> Finding {
    let ws = ev.attribution.as_ref().and_then(|a| a.workspace.clone());
    let (title, why, severity) = if kind == FindingKind::EndpointErrors {
        let title = format!(
            "`{}` in `{}` fails more often: {} of requests vs {} usually",
            ev.operation,
            ev.service,
            pct(ev.recent_error_rate),
            pct(ev.baseline_error_rate)
        );
        let why = format!(
            "In the last {} minutes {} of {} requests failed, against a usual rate of {} (the median of {} five-minute windows): more than {}× and more than {} points higher. {} Mulchie only reports this; it changes nothing.",
            ev.recent_minutes,
            ev.recent_errors,
            ev.recent_count,
            pct(ev.baseline_error_rate),
            ev.baseline_windows,
            t.ratio,
            (MIN_ERROR_RATE_INCREASE * 100.0).round(),
            changed(&ev)
        );
        let sev = if ev.recent_error_rate >= 0.25 {
            Severity::Medium
        } else {
            Severity::Low
        };
        (title, why, sev)
    } else {
        let title = format!(
            "`{}` in `{}` is {:.1}× slower at p95 ({} vs {})",
            ev.operation,
            ev.service,
            ev.ratio,
            fmt_ms(ev.recent_p95_ms),
            fmt_ms(ev.baseline_p95_ms)
        );
        let why = format!(
            "Over the last {} minutes ({} requests) its p95 was {}, against a usual {} (the median p95 of {} five-minute windows): more than {}× and more than {} ms slower. {} Mulchie only reports this; it changes nothing.",
            ev.recent_minutes,
            ev.recent_count,
            fmt_ms(ev.recent_p95_ms),
            fmt_ms(ev.baseline_p95_ms),
            ev.baseline_windows,
            t.ratio,
            t.min_slowdown_ms,
            changed(&ev)
        );
        let sev = if ev.ratio >= 2.0 {
            Severity::Medium
        } else {
            Severity::Low
        };
        (title, why, sev)
    };
    let detail = format!(
        "{} {}: p95 {} vs {}, errors {} vs {}, {} requests in {} minutes",
        ev.service,
        ev.operation,
        fmt_ms(ev.recent_p95_ms),
        fmt_ms(ev.baseline_p95_ms),
        pct(ev.recent_error_rate),
        pct(ev.baseline_error_rate),
        ev.recent_count,
        ev.recent_minutes
    );
    let route = otel_route(&ev.service);
    Finding {
        id: finding_id(kind, &ev.service, &ev.operation),
        kind,
        category: FindingCategory::Performance,
        severity,
        title,
        why,
        evidence: Evidence {
            name: Some(ev.service.clone()),
            workspace: ws,
            detail: Some(detail),
            endpoint: Some(Box::new(ev)),
            ..Evidence::default()
        },
        suggested: vec![Suggestion::call(
            "Show this service's endpoints",
            ApiRequest::get(route),
            true,
        )],
        first_seen_unix: first_seen.min(now),
        last_seen_unix: now,
        dismissed: false,
        dismissed_until: None,
    }
}

#[cfg(test)]
#[path = "endpoints_tests.rs"]
mod tests;
