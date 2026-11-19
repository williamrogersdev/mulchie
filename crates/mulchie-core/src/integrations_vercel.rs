//! Vercel: production deployments of one project
//! (`GET https://api.vercel.com/v7/deployments`), with the commit from
//! `meta.githubCommitSha` (or the GitLab or Bitbucket field).
//!
//! Vercel's REST API has no stable per-deployment latency metric (Speed
//! Insights leaves Vercel only through drains), so the number Mulchie reads
//! is the deployment's build duration: `ready - buildingAt`, for deployments
//! in state `READY`. It says how long a deploy took to build, not how fast
//! it serves.

use serde_json::Value;

use super::{enc, fetch_json, AuthHttp, Deploy, DeploySource, MetricValue, Provider, Secret};
use crate::config::VercelIntegration;

pub const HOST: &str = "api.vercel.com";
/// Deployments asked for per poll.
const LIMIT: usize = 40;

pub struct Vercel(pub VercelIntegration);

/// The request URL for the deployment list.
pub fn deployments_url(cfg: &VercelIntegration) -> String {
    let mut u = format!(
        "https://{HOST}/v7/deployments?projectId={}&target=production&state=READY&limit={LIMIT}",
        enc(&cfg.project)
    );
    if !cfg.team.is_empty() {
        u.push_str(&format!("&teamId={}", enc(&cfg.team)));
    }
    u
}

/// The deployments in a `/v7/deployments` reply, with build duration.
pub fn parse_deployments(v: &Value) -> Result<Vec<Deploy>, String> {
    let list = v["deployments"]
        .as_array()
        .ok_or("Vercel's reply has no deployments list")?;
    let mut out = Vec::new();
    for d in list {
        let Some(id) = d["uid"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
        else {
            continue;
        };
        let state = d["readyState"].as_str().or(d["state"].as_str());
        if state.is_some_and(|s| s != "READY") {
            continue;
        }
        if d["target"].as_str() != Some("production") {
            continue;
        }
        let created = d["created"].as_u64().or(d["createdAt"].as_u64());
        let Some(created_ms) = created else {
            continue;
        };
        let meta = &d["meta"];
        let sha = ["githubCommitSha", "gitlabCommitSha", "bitbucketCommitSha"]
            .iter()
            .find_map(|k| meta[*k].as_str())
            .filter(|s| super::valid_sha(s))
            .map(|s| s.to_ascii_lowercase());
        let mut metrics = Vec::new();
        if let (Some(b), Some(r)) = (d["buildingAt"].as_u64(), d["ready"].as_u64()) {
            if r >= b {
                metrics.push(MetricValue {
                    metric: "build_duration".into(),
                    value_ms: (r - b) as f64,
                    samples: None,
                });
            }
        }
        out.push(Deploy {
            provider: Provider::Vercel,
            id: id.to_string(),
            sha,
            at_unix: created_ms / 1000,
            metrics,
        });
    }
    Ok(out)
}

impl DeploySource for Vercel {
    fn provider(&self) -> Provider {
        Provider::Vercel
    }

    fn deploys(
        &self,
        http: &dyn AuthHttp,
        token: &Secret,
        _now: u64,
    ) -> Result<Vec<Deploy>, String> {
        let v = fetch_json(http, HOST, &deployments_url(&self.0), token, None)?;
        parse_deployments(&v)
    }

    /// The deployment list already carries what is measured (build
    /// duration), so nothing more is asked.
    fn metrics(&self, _: &dyn AuthHttp, _: &Secret, _: &mut [Deploy]) -> Result<(), String> {
        Ok(())
    }
}
