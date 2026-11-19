//! Sentry: releases of one project
//! (`GET /api/0/organizations/{org}/releases/`) and the p95 duration of
//! transactions per release, from the events API on the spans dataset
//! (`GET /api/0/organizations/{org}/events/?dataset=spans` with
//! `field=release&field=p95(span.duration)&field=count()` and
//! `query=is_transaction:true`, over the last 30 days).
//!
//! A release's commit is `lastCommit.id` when Sentry has commits for it,
//! else the version itself when it is a SHA (or ends in `@<sha>`). A
//! release with fewer than [`MIN_SAMPLES`] transactions gets no p95.

use serde_json::Value;

use super::{
    enc, fetch_json, parse_time, sha_in, AuthHttp, Deploy, DeploySource, MetricValue, Provider,
    Secret,
};
use crate::config::SentryIntegration;

/// Fewer transactions than this in a release: its p95 is noise.
pub const MIN_SAMPLES: u64 = 20;
const P95: &str = "p95(span.duration)";

pub struct Sentry(pub SentryIntegration);

pub fn releases_url(cfg: &SentryIntegration) -> String {
    let mut u = format!(
        "https://{}/api/0/organizations/{}/releases/?project={}&per_page=50",
        cfg.host,
        enc(&cfg.org),
        enc(&cfg.project)
    );
    if !cfg.environment.is_empty() {
        u.push_str(&format!("&environment={}", enc(&cfg.environment)));
    }
    u
}

pub fn events_url(cfg: &SentryIntegration) -> String {
    let mut u = format!(
        "https://{}/api/0/organizations/{}/events/?dataset=spans&field=release&field={}&field={}&query={}&project={}&statsPeriod=30d&per_page=100&sort={}",
        cfg.host,
        enc(&cfg.org),
        enc(P95),
        enc("count()"),
        enc("is_transaction:true"),
        enc(&cfg.project),
        enc("-count()"),
    );
    if !cfg.environment.is_empty() {
        u.push_str(&format!("&environment={}", enc(&cfg.environment)));
    }
    u
}

/// The releases in a release-list reply.
pub fn parse_releases(v: &Value) -> Result<Vec<Deploy>, String> {
    let list = v
        .as_array()
        .ok_or("Sentry's reply is not a list of releases")?;
    let mut out = Vec::new();
    for r in list {
        let Some(version) = r["version"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 200)
        else {
            continue;
        };
        let at = r["dateReleased"]
            .as_str()
            .and_then(parse_time)
            .or_else(|| r["dateCreated"].as_str().and_then(parse_time));
        let Some(at) = at else {
            continue;
        };
        let sha = r["lastCommit"]["id"]
            .as_str()
            .filter(|s| super::valid_sha(s))
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| sha_in(version));
        out.push(Deploy {
            provider: Provider::Sentry,
            id: version.to_string(),
            sha,
            at_unix: at,
            metrics: Vec::new(),
        });
    }
    Ok(out)
}

/// `(release, p95 ms, count)` rows of an events reply.
pub fn parse_p95(v: &Value) -> Result<Vec<(String, f64, u64)>, String> {
    let rows = v["data"]
        .as_array()
        .ok_or("Sentry's events reply has no data")?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let rel = r["release"].as_str()?.to_string();
            let p95 = r[P95].as_f64()?;
            let n = r["count()"].as_u64().unwrap_or(0);
            Some((rel, p95, n))
        })
        .collect())
}

impl DeploySource for Sentry {
    fn provider(&self) -> Provider {
        Provider::Sentry
    }

    fn deploys(
        &self,
        http: &dyn AuthHttp,
        token: &Secret,
        _now: u64,
    ) -> Result<Vec<Deploy>, String> {
        let v = fetch_json(http, &self.0.host, &releases_url(&self.0), token, None)?;
        parse_releases(&v)
    }

    fn metrics(
        &self,
        http: &dyn AuthHttp,
        token: &Secret,
        deploys: &mut [Deploy],
    ) -> Result<(), String> {
        if deploys.is_empty() {
            return Ok(());
        }
        let v = fetch_json(http, &self.0.host, &events_url(&self.0), token, None)?;
        for (rel, p95, n) in parse_p95(&v)? {
            if n < MIN_SAMPLES || !p95.is_finite() {
                continue;
            }
            if let Some(d) = deploys.iter_mut().find(|d| d.id == rel) {
                d.metrics.push(MetricValue {
                    metric: "p95".into(),
                    value_ms: p95,
                    samples: Some(n),
                });
            }
        }
        Ok(())
    }
}
