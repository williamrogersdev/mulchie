//! `mulchie traces`: the endpoints of local apps that send OpenTelemetry
//! traces to the engine (`GET /v1/otel`). The figures live in the engine's
//! memory, so this needs the engine.

use crate::commands::{fail, print_json, s, truncate, u};
use crate::conn::{self, Conn};
use mulchie_core::api::query_escape;
use mulchie_core::endpoints::fmt_ms;
use mulchie_core::Paths;
use serde_json::Value;
use std::path::Path;

pub fn list(paths: &Paths, service: Option<&str>, minutes: u64, json_out: bool) -> i32 {
    let c = match conn::connect(paths) {
        Conn::Engine(c) => c,
        other => return fail(other.explain_required("`mulchie traces`")),
    };
    let mut q = vec![format!("minutes={minutes}")];
    if let Some(svc) = service {
        q.push(format!("service={}", query_escape(svc)));
    }
    let v = match conn::call(&c, "GET", &format!("/v1/otel?{}", q.join("&")), None) {
        Ok(v) => v,
        Err(e) => return fail(conn::describe(&e)),
    };
    if json_out {
        print_json(&v);
    } else {
        print!("{}", render(&v));
    }
    0
}

fn ms(v: &Value) -> String {
    v.as_f64().map(fmt_ms).unwrap_or_else(|| "-".into())
}

pub fn render(v: &Value) -> String {
    let mut out = String::new();
    if v["enabled"] != true {
        out.push_str(
            "The trace receiver is off. Turn it on with `mulchie config set otel.enabled true`, then point an app at it with OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318.\n",
        );
        return out;
    }
    match (v["listening"].as_bool(), v["error"].as_str()) {
        (Some(true), _) => out.push_str(&format!(
            "Receiving traces at {} (OTLP/HTTP, JSON or protobuf, uncompressed); {} spans received.\n",
            s(v, "endpoint"),
            u(v, "received_spans")
        )),
        (_, Some(e)) => out.push_str(&format!(
            "The trace receiver is on but not listening on port {}: {e}\n",
            u(v, "configured_port")
        )),
        _ => out.push_str(
            "The trace receiver is on, but this engine does not run it (only the engine that holds the lock does), or it is starting.\n",
        ),
    }
    let services = v["services"].as_array().cloned().unwrap_or_default();
    if services.is_empty() {
        out.push_str("No traces received yet.\n");
        return out;
    }
    let minutes = u(v, "minutes");
    for svc in &services {
        let a = &svc["attribution"];
        let place = a["workspace"]
            .as_str()
            .and_then(|w| Path::new(w).file_name())
            .map(|n| format!(" · {}", n.to_string_lossy()))
            .unwrap_or_default();
        let head = a["head"]
            .as_str()
            .map(|h| format!(" @ {}", h.get(..10).unwrap_or(h)))
            .unwrap_or_default();
        let by = a["agent"]
            .as_str()
            .map(|g| format!(" · {g}"))
            .unwrap_or_default();
        out.push_str(&format!("\n{}{place}{head}{by}\n", s(svc, "name")));
        let ops = svc["operations"].as_array().cloned().unwrap_or_default();
        if ops.is_empty() {
            out.push_str(&format!("  no requests in the last {minutes} minutes\n"));
            continue;
        }
        out.push_str(&format!(
            "  {:<40} {:>7} {:>6} {:>9} {:>9} {:>9}  usual p95\n",
            "operation", "count", "errors", "p50", "p95", "p99"
        ));
        for op in &ops {
            let flag = match (op["slower"] == true, op["more_errors"] == true) {
                (true, true) => "  slower, more errors",
                (true, false) => "  slower",
                (false, true) => "  more errors",
                _ => "",
            };
            out.push_str(&format!(
                "  {:<40} {:>7} {:>6} {:>9} {:>9} {:>9}  {}{flag}\n",
                truncate(s(op, "operation"), 40),
                u(op, "count"),
                u(op, "errors"),
                ms(&op["p50_ms"]),
                ms(&op["p95_ms"]),
                ms(&op["p99_ms"]),
                ms(&op["baseline"]["p95_ms"]),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_off_on_and_services() {
        assert!(render(&json!({ "enabled": false })).contains("otel.enabled true"));
        let v = json!({ "enabled": true, "listening": false, "error": "address in use",
                        "configured_port": 4318, "services": [] });
        assert!(render(&v).contains("not listening on port 4318: address in use"));
        let v = json!({ "enabled": true, "listening": true, "endpoint": "http://127.0.0.1:4318",
            "received_spans": 70, "minutes": 15, "services": [
            { "name": "shop-web",
              "attribution": { "workspace": "/w/shop", "head": "0123456789abcdef", "agent": "claude", "via": "pid" },
              "operations": [
                { "operation": "GET /api/orders", "count": 40, "errors": 0, "p50_ms": 400.0,
                  "p95_ms": 420.0, "p99_ms": 420.0, "baseline": { "p95_ms": 100.0 }, "slower": true },
                { "operation": "GET /health", "count": 1, "errors": 0, "p50_ms": 2.0,
                  "p95_ms": 2.0, "p99_ms": 2.0, "baseline": null }
              ] },
            { "name": "worker", "operations": [] }
        ]});
        let r = render(&v);
        assert!(r.contains("http://127.0.0.1:4318"), "{r}");
        assert!(r.contains("shop-web · shop @ 0123456789 · claude"), "{r}");
        assert!(r.contains("420 ms") && r.contains("100 ms  slower"), "{r}");
        assert!(r.contains("no requests in the last 15 minutes"), "{r}");
    }

    #[test]
    fn without_an_engine_it_says_so() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        assert_eq!(list(&paths, None, 15, true), 1);
    }
}
