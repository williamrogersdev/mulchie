//! The discovery files at the repository root stay true to the code:
//! `docs/openapi.yaml` lists exactly the API's routes, and `server.json`
//! names this version. Skipped when the files are absent, as in a crates.io
//! package.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn root_file(rel: &str) -> Option<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(p).ok()
}

/// (METHOD, path) pairs from the `paths:` section, read line by line.
fn openapi_routes(text: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut in_paths = false;
    let mut current: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            in_paths = line.trim_end() == "paths:";
            current = None;
            continue;
        }
        if !in_paths {
            continue;
        }
        if let Some(p) = line.strip_prefix("  ").filter(|l| l.starts_with('/')) {
            current = Some(p.trim_end().trim_end_matches(':').to_string());
        } else if let Some(m) = line.strip_prefix("    ").filter(|l| !l.starts_with(' ')) {
            let m = m.trim_end().trim_end_matches(':');
            if ["get", "post", "put", "patch", "delete"].contains(&m) {
                let path = current.clone().expect("a method under a path");
                out.insert((m.to_uppercase(), path));
            }
        }
    }
    out
}

#[test]
fn openapi_matches_the_route_table() {
    let Some(text) = root_file("docs/openapi.yaml") else {
        eprintln!("docs/openapi.yaml not found; skipped");
        return;
    };
    let documented = openapi_routes(&text);
    let served: BTreeSet<(String, String)> = mulchie_core::api::ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    let missing: Vec<_> = served.difference(&documented).collect();
    let extra: Vec<_> = documented.difference(&served).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "docs/openapi.yaml is out of date.\nServed but not documented: {missing:?}\nDocumented but not served: {extra:?}"
    );
    let version = format!("  version: {}", env!("CARGO_PKG_VERSION"));
    assert!(
        text.lines().any(|l| l == version),
        "docs/openapi.yaml info.version should be {}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn openapi_parser_reads_paths_only() {
    let t = "paths:\n  /a:\n    get:\n      x: 1\n    post:\n  /b:\n    get:\ncomponents:\n  /c:\n    get:\n";
    let r = openapi_routes(t);
    let want: BTreeSet<(String, String)> = [("GET", "/a"), ("POST", "/a"), ("GET", "/b")]
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(r, want);
}

#[test]
fn server_json_names_this_version() {
    let Some(text) = root_file("server.json") else {
        eprintln!("server.json not found; skipped");
        return;
    };
    let v: serde_json::Value = serde_json::from_str(&text).expect("server.json is JSON");
    let want = env!("CARGO_PKG_VERSION");
    assert_eq!(v["version"], want, "server.json version");
    for p in v["packages"].as_array().expect("packages") {
        assert_eq!(p["version"], want, "server.json package version");
        assert_eq!(p["identifier"], "mulchie");
        assert_eq!(p["transport"]["type"], "stdio");
    }
    let d = v["description"].as_str().unwrap();
    assert!(
        !d.is_empty() && d.len() <= 100,
        "registry descriptions are 1-100 characters"
    );
}
