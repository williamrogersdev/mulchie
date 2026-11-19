//! PostHog: web vitals per release of one project, through the query API
//! (`POST https://<us|eu>.posthog.com/api/projects/{id}/query/` with a
//! HogQL query, which reads and changes nothing; the personal API key needs
//! only the Query Read scope).
//!
//! A release is a value of the event property `release_property` (default
//! `$app_version`) on `$web_vitals` events in the last 30 days; its time is
//! when it was first seen. Web SDKs do not set `$app_version` on their own,
//! so the app has to register the property, ideally with the commit SHA.
//! The numbers are the 75th percentile of `$web_vitals_LCP_value` and
//! `$web_vitals_INP_value` (milliseconds), from releases with at least
//! [`MIN_SAMPLES`] measurements.

use serde_json::{json, Value};

use super::{fetch_json, sha_in, AuthHttp, Deploy, DeploySource, MetricValue, Provider, Secret};
use crate::config::{posthog_property_ok, PostHogIntegration};

/// Fewer measurements than this in a release: its p75 is noise.
pub const MIN_SAMPLES: u64 = 20;

pub struct PostHog(pub PostHogIntegration);

pub fn query_url(cfg: &PostHogIntegration) -> String {
    format!("https://{}/api/projects/{}/query/", cfg.host, cfg.project)
}

/// `properties.<name>`; `None` for a name that is not a plain identifier
/// (it is placed into HogQL as it is).
fn prop(cfg: &PostHogIntegration) -> Option<String> {
    posthog_property_ok(&cfg.release_property)
        .then(|| format!("properties.{}", cfg.release_property))
}

pub fn releases_query(cfg: &PostHogIntegration) -> Option<String> {
    let p = prop(cfg)?;
    Some(format!(
        "SELECT toString({p}) AS release, toUnixTimestamp(min(timestamp)) AS first_seen \
FROM events WHERE event = '$web_vitals' AND timestamp > now() - INTERVAL 30 DAY \
AND notEmpty(toString({p})) GROUP BY release ORDER BY first_seen DESC LIMIT 50"
    ))
}

pub fn vitals_query(cfg: &PostHogIntegration) -> Option<String> {
    let p = prop(cfg)?;
    Some(format!(
        "SELECT toString({p}) AS release, \
quantile(0.75)(toFloat(properties.$web_vitals_LCP_value)) AS lcp, \
countIf(isNotNull(properties.$web_vitals_LCP_value)) AS lcp_n, \
quantile(0.75)(toFloat(properties.$web_vitals_INP_value)) AS inp, \
countIf(isNotNull(properties.$web_vitals_INP_value)) AS inp_n \
FROM events WHERE event = '$web_vitals' AND timestamp > now() - INTERVAL 30 DAY \
AND notEmpty(toString({p})) GROUP BY release LIMIT 200"
    ))
}

fn body(query: &str, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "query": { "kind": "HogQLQuery", "query": query },
        "name": name,
    }))
    .unwrap_or_default()
}

fn rows(v: &Value) -> Result<&Vec<Value>, String> {
    v["results"]
        .as_array()
        .ok_or_else(|| "PostHog's reply has no results".to_string())
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Releases from the first query's rows: `[release, first_seen]`.
pub fn parse_releases(v: &Value) -> Result<Vec<Deploy>, String> {
    let mut out = Vec::new();
    for r in rows(v)? {
        let Some(rel) = r[0].as_str().filter(|s| !s.is_empty() && s.len() <= 200) else {
            continue;
        };
        let Some(at) = num(&r[1]).filter(|t| *t >= 0.0) else {
            continue;
        };
        out.push(Deploy {
            provider: Provider::Posthog,
            id: rel.to_string(),
            sha: sha_in(rel),
            at_unix: at as u64,
            metrics: Vec::new(),
        });
    }
    Ok(out)
}

/// Fill LCP and INP from the second query's rows:
/// `[release, lcp, lcp_n, inp, inp_n]`.
pub fn apply_vitals(v: &Value, deploys: &mut [Deploy]) -> Result<(), String> {
    for r in rows(v)? {
        let Some(rel) = r[0].as_str() else {
            continue;
        };
        let Some(d) = deploys.iter_mut().find(|d| d.id == rel) else {
            continue;
        };
        for (metric, vi, ni) in [("lcp_p75", 1, 2), ("inp_p75", 3, 4)] {
            let n = num(&r[ni]).unwrap_or(0.0) as u64;
            if let Some(x) = num(&r[vi]).filter(|x| x.is_finite() && *x >= 0.0) {
                if n >= MIN_SAMPLES {
                    d.metrics.push(MetricValue {
                        metric: metric.into(),
                        value_ms: x,
                        samples: Some(n),
                    });
                }
            }
        }
    }
    Ok(())
}

impl DeploySource for PostHog {
    fn provider(&self) -> Provider {
        Provider::Posthog
    }

    fn deploys(
        &self,
        http: &dyn AuthHttp,
        token: &Secret,
        _now: u64,
    ) -> Result<Vec<Deploy>, String> {
        let q = releases_query(&self.0)
            .ok_or("integrations.posthog.release_property is not a plain property name")?;
        let v = fetch_json(
            http,
            &self.0.host,
            &query_url(&self.0),
            token,
            Some(&body(&q, "mulchie: releases")),
        )?;
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
        let q = vitals_query(&self.0)
            .ok_or("integrations.posthog.release_property is not a plain property name")?;
        let v = fetch_json(
            http,
            &self.0.host,
            &query_url(&self.0),
            token,
            Some(&body(&q, "mulchie: web vitals per release")),
        )?;
        apply_vitals(&v, deploys)
    }
}
