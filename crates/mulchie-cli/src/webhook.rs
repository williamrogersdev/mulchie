//! `mulchie webhook`: the issue webhook's status, and a test event
//! ([`mulchie_core::webhook`]). The URL is only ever shown redacted.

use crate::commands::{fail, print_json, s, u};
use crate::conn::{self, Conn};
use mulchie_core::webhook::{network_built, redact_url};
use mulchie_core::{Config, Paths};
use serde_json::{json, Value};

/// The status from the config file, when no engine runs: nothing is sent.
fn local_status(paths: &Paths) -> Value {
    let cfg = Config::load_from(paths).config.webhook;
    json!({
        "engine": false,
        "network": network_built(),
        "configured": !cfg.url.is_empty(),
        "url": redact_url(&cfg.url),
        "format": cfg.format,
        "events": cfg.events,
        "min_severity": cfg.min_severity,
        "include_host": cfg.include_host,
        "sending": false,
    })
}

pub fn status(paths: &Paths, json_out: bool) -> i32 {
    let v = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "GET", "/v1/webhook", None) {
            Ok(v) => v,
            Err(e) => return fail(conn::describe(&e)),
        },
        _ => local_status(paths),
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    if v["network"] == false {
        println!("This build of Mulchie has no network support: the webhook is off.");
    }
    if v["configured"] != true {
        println!(
            "Off: webhook.url is not set, so nothing is sent. Set it with `mulchie config set webhook.url <url>`."
        );
        return 0;
    }
    let events: Vec<&str> = v["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    println!("url        {}", s(&v, "url"));
    println!(
        "sends      {} at {} or above, as {}{}",
        events.join(", "),
        s(&v, "min_severity"),
        s(&v, "format"),
        if v["include_host"] == true {
            ", with the host name"
        } else {
            ""
        }
    );
    if v["engine"] == false {
        println!("(No engine is running: nothing is sent until one starts.)");
        return 0;
    }
    if v["sending"] != true {
        println!("(This engine does not send; the one that holds the engine lock does.)");
    }
    println!(
        "sent       {}  failed {}  dropped {}  queued {}",
        u(&v, "sent"),
        u(&v, "failed"),
        u(&v, "dropped"),
        u(&v, "queued")
    );
    if let Some(e) = v["last_error"].as_str() {
        println!("last error {e}");
    }
    0
}

pub fn test(paths: &Paths, json_out: bool) -> i32 {
    let Conn::Engine(c) = conn::connect(paths) else {
        return fail(conn::connect(paths).explain_required("`mulchie webhook test`"));
    };
    match conn::call(&c, "POST", "/v1/webhook/test", None) {
        Ok(v) => {
            if json_out {
                print_json(&v);
            } else {
                println!(
                    "Sent a test event; the webhook answered HTTP {}.",
                    u(&v, "status")
                );
            }
            0
        }
        Err(e) => fail(conn::describe(&e)),
    }
}
