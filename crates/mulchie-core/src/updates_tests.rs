//! Update checks against fakes: no test reaches the network or runs a real
//! version command.

use super::*;
use crate::config::UpdateCheck;
use std::sync::Mutex;

/// A fake HTTP client: GET answers by URL, POST by a closure; every call is
/// recorded.
#[derive(Default)]
pub(crate) struct FakeHttp {
    pub gets: HashMap<String, HttpReply>,
    #[allow(clippy::type_complexity)]
    pub post: Option<Box<dyn Fn(&Value) -> HttpReply + Send + Sync>>,
    pub calls: Mutex<Vec<String>>,
}

pub(crate) fn reply(status: u16, v: Value) -> HttpReply {
    HttpReply {
        status,
        body: serde_json::to_vec(&v).unwrap(),
    }
}

impl Http for FakeHttp {
    fn get(&self, url: &str, max: usize) -> Result<HttpReply, String> {
        self.calls.lock().unwrap().push(format!("GET {url}"));
        let r = self.gets.get(url).cloned().ok_or("connection refused")?;
        if r.body.len() > max {
            return Err("too large".into());
        }
        Ok(r)
    }
    fn post_json(&self, url: &str, body: &[u8], _max: usize) -> Result<HttpReply, String> {
        self.calls.lock().unwrap().push(format!("POST {url}"));
        let v: Value = serde_json::from_slice(body).unwrap();
        self.post
            .as_ref()
            .map(|f| f(&v))
            .ok_or("connection refused".into())
    }
}

/// A fake runner: executables by name, outputs by program name.
#[derive(Default)]
pub(crate) struct FakeRunner {
    pub tools: HashMap<String, (bool, String)>,
    /// Where `find` says a tool is (default `/fake/bin/<name>`).
    pub paths: HashMap<String, PathBuf>,
    pub runs: Mutex<Vec<String>>,
}

impl Runner for FakeRunner {
    fn find(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = self.paths.get(name) {
            return Some(p.clone());
        }
        self.tools
            .contains_key(name)
            .then(|| PathBuf::from(format!("/fake/bin/{name}")))
    }
    fn run(
        &self,
        prog: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        _: Duration,
        _: usize,
    ) -> Option<(bool, String)> {
        let name = prog.file_name()?.to_string_lossy().to_string();
        self.runs
            .lock()
            .unwrap()
            .push(format!("{name} {} {env:?}", args.join(" ")));
        self.tools.get(&name).cloned()
    }
}

#[test]
fn semver_comparison_never_claims_outdated_without_semver() {
    assert_eq!(newer("1.2.4", "1.2.3"), Some(true));
    assert_eq!(newer("v1.10.0", "1.9.9"), Some(true));
    assert_eq!(newer("1.2.3", "1.2.3"), Some(false));
    assert_eq!(newer("1.2.3", "1.2.4"), Some(false));
    assert_eq!(
        newer("1.0.0", "1.0.0-beta.2"),
        Some(true),
        "release after pre-release"
    );
    assert_eq!(newer("1.0.0-beta.11", "1.0.0-beta.2"), Some(true));
    assert_eq!(
        newer("1.0.0+build.2", "1.0.0+build.1"),
        Some(false),
        "build metadata ignored"
    );
    for bad in ["latest", "1.2", "2024.10.1-x y", "", "1.2.3.4", "nightly"] {
        assert_eq!(newer(bad, "1.0.0"), None, "{bad}");
        assert_eq!(newer("9.9.9", bad), None, "{bad}");
    }
    assert_eq!(
        version_in("2.0.14 (Claude Code)").unwrap().to_string(),
        "2.0.14"
    );
    assert_eq!(
        version_in("codex-cli 0.46.0\n").unwrap().to_string(),
        "0.46.0"
    );
    assert_eq!(
        version_in("gemini v0.9.0-preview.1").unwrap().to_string(),
        "0.9.0-preview.1"
    );
    assert_eq!(version_in("no version here 1.2"), None);
}

#[test]
fn mulchie_update_offer_follows_the_release_candidate_rules() {
    let offered = mulchie_update_offered;
    // A stable install is never offered a pre-release, even a newer one.
    assert_eq!(offered("0.4.0-rc.1", "0.3.0", false), Some(false));
    assert_eq!(offered("0.3.0-rc.1", "0.2.9", false), Some(false));
    assert_eq!(offered("0.4.0", "0.3.0", true), Some(false), "API flag");
    assert_eq!(offered("0.4.0", "0.3.0", false), Some(true));
    // An rc install is offered the final release and a later rc, not an older one.
    assert_eq!(offered("0.3.0", "0.3.0-rc.1", false), Some(true));
    assert_eq!(offered("0.3.0-rc.2", "0.3.0-rc.1", true), Some(true));
    assert_eq!(offered("0.3.0-rc.10", "0.3.0-rc.2", false), Some(true));
    assert_eq!(offered("0.3.0-rc.1", "0.3.0-rc.1", false), Some(false));
    assert_eq!(offered("0.2.9", "0.3.0-rc.1", false), Some(false));
    assert_eq!(offered("latest", "0.3.0", false), None);
}

#[test]
fn cargo_lock_keeps_registry_packages() {
    let t = r#"
version = 4

[[package]]
name = "mine"
version = "0.1.0"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"

[[package]]
name = "fromgit"
version = "0.2.0"
source = "git+https://example.com/x#abc"

[[package]]
name = "sparse"
version = "2.0.0"
source = "sparse+https://index.crates.io/"
"#;
    let p = parse_cargo_lock(t).unwrap();
    let names: Vec<(&str, &str)> = p
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(names, vec![("serde", "1.0.200"), ("sparse", "2.0.0")]);
    assert!(p.iter().all(|p| p.ecosystem == "crates.io"));
    assert!(parse_cargo_lock("[[package]]\nname = 1").is_err());
    assert!(parse_cargo_lock("").unwrap().is_empty());
}

#[test]
fn package_lock_v1_v2_v3() {
    let v1 = json!({
        "lockfileVersion": 1,
        "dependencies": {
            "a": { "version": "1.0.0", "dependencies": { "b": { "version": "2.0.0" } } },
            "linked": { "version": "file:../x" }
        }
    });
    let p = parse_package_lock(&v1.to_string()).unwrap();
    let mut n: Vec<(String, String)> = p
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    n.sort();
    assert_eq!(
        n,
        vec![("a".into(), "1.0.0".into()), ("b".into(), "2.0.0".into())]
    );

    let v3 = json!({
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "app", "dependencies": { "lodash": "^4" }, "devDependencies": { "@scope/t": "1" } },
            "node_modules/lodash": { "version": "4.17.20" },
            "node_modules/@scope/t": { "version": "1.0.0", "dev": true },
            "node_modules/x/node_modules/lodash": { "version": "3.10.1" },
            "node_modules/local": { "resolved": "../local", "link": true },
            "packages/inner": { "version": "0.0.1" }
        },
        "dependencies": { "ignored-v1-section": { "version": "9.9.9" } }
    });
    let p = parse_package_lock(&v3.to_string()).unwrap();
    let find = |n: &str, v: &str| p.iter().find(|p| p.name == n && p.version == v).cloned();
    assert!(find("lodash", "4.17.20").unwrap().direct);
    assert!(!find("lodash", "3.10.1").unwrap().direct, "nested copy");
    assert!(find("@scope/t", "1.0.0").unwrap().direct);
    assert_eq!(p.len(), 3, "{p:?}");
    assert!(parse_package_lock("{}").is_err());
    assert!(parse_package_lock("[").is_err());
}

#[test]
fn go_mod_requires_without_replaced_modules() {
    let t = r#"
module example.com/app

go 1.22

require github.com/a/b v1.2.3

require (
    golang.org/x/net v0.17.0 // indirect
    github.com/c/d v2.0.0+incompatible
    github.com/replaced/e v1.0.0
    github.com/bad/f latest
)

replace github.com/replaced/e => ../e
"#;
    let p = parse_go_mod(t);
    let n: Vec<(&str, &str, bool)> = p
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str(), p.direct))
        .collect();
    assert_eq!(
        n,
        vec![
            ("github.com/a/b", "1.2.3", true),
            ("golang.org/x/net", "0.17.0", false),
            ("github.com/c/d", "2.0.0", true),
        ]
    );
}

fn pkgs(n: usize) -> Vec<Pkg> {
    (0..n)
        .map(|i| Pkg {
            ecosystem: "npm".into(),
            name: format!("p{i}"),
            version: "1.0.0".into(),
            direct: false,
        })
        .collect()
}

/// Batches of at most OSV_BATCH, and each query's next_page_token sent back
/// on that query only.
#[test]
fn osv_batches_and_follows_page_tokens() {
    let seen: std::sync::Arc<Mutex<Vec<usize>>> = Default::default();
    let s2 = seen.clone();
    let http = FakeHttp {
        post: Some(Box::new(move |v: &Value| {
            let qs = v["queries"].as_array().unwrap();
            s2.lock().unwrap().push(qs.len());
            let results: Vec<Value> = qs
                .iter()
                .map(|q| {
                    let name = q["package"]["name"].as_str().unwrap();
                    match (name, q.get("page_token").and_then(Value::as_str)) {
                        ("p1", None) => json!({"vulns": [{"id": "GHSA-1", "modified": "x"}], "next_page_token": "t1"}),
                        ("p1", Some("t1")) => json!({"vulns": [{"id": "GHSA-2"}, {"id": "../etc"}]}),
                        (_, Some(_)) => panic!("token sent on a query that had none"),
                        _ => json!({}),
                    }
                })
                .collect();
            reply(200, json!({ "results": results }))
        })),
        ..FakeHttp::default()
    };
    let p = pkgs(OSV_BATCH + 5);
    let ids = osv_query(&http, &p).unwrap();
    assert_eq!(
        ids[1],
        vec!["GHSA-1".to_string(), "GHSA-2".to_string()],
        "invalid ids dropped"
    );
    assert!(ids[0].is_empty());
    assert_eq!(*seen.lock().unwrap(), vec![OSV_BATCH, 1, 5]);
}

#[test]
fn osv_malformed_replies_are_errors_not_panics() {
    let cases: Vec<HttpReply> = vec![
        reply(500, json!({})),
        HttpReply {
            status: 200,
            body: b"not json".to_vec(),
        },
        reply(200, json!({"results": "x"})),
        reply(200, json!({"results": []})),
        reply(200, json!({"nothing": 1})),
    ];
    for c in cases {
        let c2 = c.clone();
        let http = FakeHttp {
            post: Some(Box::new(move |_| c2.clone())),
            ..FakeHttp::default()
        };
        assert!(osv_query(&http, &pkgs(2)).is_err(), "{c:?}");
    }
    // Endless pagination stops.
    let http = FakeHttp {
        post: Some(Box::new(|v: &Value| {
            let n = v["queries"].as_array().unwrap().len();
            reply(
                200,
                json!({"results": vec![json!({"vulns": [], "next_page_token": "again"}); n]}),
            )
        })),
        ..FakeHttp::default()
    };
    assert!(osv_query(&http, &pkgs(1)).unwrap_err().contains("pages"));
    // Odd shapes inside results are skipped.
    let http = FakeHttp {
        post: Some(Box::new(|_| {
            reply(
                200,
                json!({"results": [{"vulns": [1, {"id": 2}, {"x": "y"}]}, null]}),
            )
        })),
        ..FakeHttp::default()
    };
    assert_eq!(
        osv_query(&http, &pkgs(2)).unwrap(),
        vec![Vec::<String>::new(), Vec::new()]
    );
}

#[test]
fn advisory_details_and_fixed_versions() {
    let v = json!({
        "id": "GHSA-xxxx-yyyy-zzzz",
        "summary": "Prototype pollution\nmore",
        "aliases": ["CVE-2024-1", "bad id!"],
        "database_specific": { "severity": "HIGH" },
        "affected": [{
            "package": { "ecosystem": "npm", "name": "lodash" },
            "ranges": [{ "type": "SEMVER", "events": [
                { "introduced": "0" }, { "fixed": "4.17.21" },
                { "introduced": "5.0.0" }, { "fixed": "5.0.3" }
            ]}]
        }, { "package": "broken" }]
    });
    let d = parse_vuln(&v).unwrap();
    assert_eq!(d.summary, "Prototype pollution");
    assert_eq!(d.severity, Severity::High);
    assert_eq!(d.aliases, vec!["CVE-2024-1".to_string()]);
    let p = |ver: &str| Pkg {
        ecosystem: "npm".into(),
        name: "lodash".into(),
        version: ver.into(),
        direct: true,
    };
    assert_eq!(fixed_for(&d, &p("4.17.20")).as_deref(), Some("4.17.21"));
    assert_eq!(fixed_for(&d, &p("5.0.1")).as_deref(), Some("5.0.3"));
    assert_eq!(fixed_for(&d, &p("6.0.0")), None);
    assert!(parse_vuln(&json!({"id": "../x"})).is_err());
    assert!(parse_vuln(&json!([])).is_err());
    let bare = parse_vuln(&json!({"id": "OSV-1"})).unwrap();
    assert_eq!(bare.severity, Severity::Medium);
    assert_eq!(
        advisory_command(&p("4.17.20"), Some("4.17.21"), Path::new("/w")),
        "npm install lodash@4.17.21 (in /w)"
    );
}

fn input<'a>(cfg: &'a UpdatesConfig, ws: &'a [PathBuf], home: &'a Path) -> CheckInput<'a> {
    CheckInput {
        cfg,
        home,
        exe: Some(Path::new(
            "/opt/homebrew/Cellar/mulchie-cli/0.3.0/bin/mulchie",
        )),
        version: "0.3.0",
        workspaces: ws,
        brew_cache: None,
        avoid_dirs: ws,
        now: 1_000,
    }
}

/// `check: off`: no request, no command.
#[test]
fn off_means_no_network_and_no_commands() {
    let cfg = UpdatesConfig {
        check: UpdateCheck::Off,
        advisories: true,
        ..UpdatesConfig::default()
    };
    let http = FakeHttp::default();
    let run = FakeRunner {
        tools: HashMap::from([("claude".into(), (true, "1.0.0".into()))]),
        ..FakeRunner::default()
    };
    let r = check(&http, &run, &input(&cfg, &[], Path::new("/h")), None);
    assert!(http.calls.lock().unwrap().is_empty());
    assert!(run.runs.lock().unwrap().is_empty());
    assert!(findings(&r).is_empty());
}

#[test]
fn a_full_check_reports_outdated_tools_and_advisories() {
    let t = tempfile::tempdir().unwrap();
    let ws = t.path().join("app");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("package-lock.json"),
        json!({"lockfileVersion": 3, "packages": {
            "": {"dependencies": {"lodash": "^4"}},
            "node_modules/lodash": {"version": "4.17.20"}
        }})
        .to_string(),
    )
    .unwrap();
    // Installs read by their metadata (N10): an npm package, a cask, and
    // one with no version anywhere.
    let pkg = t.path().join("lib/node_modules/@anthropic-ai/claude-code");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        json!({"name": "@anthropic-ai/claude-code", "version": "2.0.14"}).to_string(),
    )
    .unwrap();
    std::fs::write(pkg.join("cli.js"), "").unwrap();
    std::fs::create_dir_all(t.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(pkg.join("cli.js"), t.path().join("bin/claude")).unwrap();
    let cask = t.path().join("Caskroom/codex/0.46.0");
    std::fs::create_dir_all(&cask).unwrap();
    std::fs::write(cask.join("codex"), "").unwrap();
    let cursor = t.path().join("cursor/versions/2025.10.01-abc");
    std::fs::create_dir_all(&cursor).unwrap();
    std::fs::write(cursor.join("cursor-agent"), "").unwrap();
    let hb = t.path().join("hb");
    std::fs::create_dir_all(hb.join("bin")).unwrap();
    std::fs::create_dir_all(hb.join("Library/Taps/homebrew/homebrew-core")).unwrap();
    std::fs::write(hb.join("bin/brew"), "").unwrap();
    let mut http = FakeHttp::default();
    http.gets.insert(
        MULCHIE_RELEASE_URL.into(),
        reply(200, json!({"tag_name": "v0.4.0"})),
    );
    http.gets.insert(
        "https://registry.npmjs.org/-/package/@anthropic-ai%2fclaude-code/dist-tags".into(),
        reply(200, json!({"latest": "2.1.0"})),
    );
    http.gets.insert(
        "https://registry.npmjs.org/-/package/@openai%2fcodex/dist-tags".into(),
        reply(200, json!({"latest": "nightly-build"})),
    );
    http.gets.insert(
        format!("{OSV_VULN_URL}GHSA-1"),
        reply(200, json!({"id": "GHSA-1", "summary": "bad", "affected": [{"package": {"ecosystem": "npm", "name": "lodash"}, "ranges": [{"events": [{"fixed": "4.17.21"}]}]}]})),
    );
    http.post = Some(Box::new(|v: &Value| {
        let n = v["queries"].as_array().unwrap().len();
        let mut res = vec![json!({}); n];
        res[0] = json!({"vulns": [{"id": "GHSA-1"}]});
        reply(200, json!({"results": res}))
    }));
    let run = FakeRunner {
        tools: HashMap::from([
            ("claude".into(), (true, "2.0.14 (Claude Code)".into())),
            ("codex".into(), (true, "codex-cli 0.46.0".into())),
            ("cursor-agent".into(), (true, "2025.10.01-abc".into())),
            ("brew".into(), (false, json!({"formulae": [
                {"name": "git", "installed_versions": ["2.44.0"], "current_version": "2.45.0", "pinned": false},
                {"name": "pinned", "installed_versions": ["1"], "current_version": "2", "pinned": true}
            ], "casks": []}).to_string())),
        ]),
        paths: HashMap::from([
            ("claude".into(), t.path().join("bin/claude")),
            ("codex".into(), cask.join("codex")),
            ("cursor-agent".into(), cursor.join("cursor-agent")),
            ("brew".into(), hb.join("bin/brew")),
        ]),
        ..FakeRunner::default()
    };
    let cfg = UpdatesConfig {
        advisories: true,
        ..UpdatesConfig::default()
    };
    let wss = [ws.clone()];
    let inp = input(&cfg, &wss, t.path());
    let r = check(&http, &run, &inp, None);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let m = r.mulchie.as_ref().unwrap();
    assert!(m.outdated);
    assert_eq!(
        m.command.as_deref(),
        Some("brew upgrade williamrogersdev/tap/mulchie-cli")
    );
    let claude = r.agents.iter().find(|a| a.name == "claude").unwrap();
    assert_eq!(claude.installed.as_deref(), Some("2.0.14"));
    assert!(claude
        .version_from
        .as_deref()
        .unwrap()
        .starts_with("package.json at"));
    assert!(claude.outdated);
    assert_eq!(claude.command.as_deref(), Some("claude update"));
    let codex = r.agents.iter().find(|a| a.name == "codex").unwrap();
    assert_eq!(
        codex.installed.as_deref(),
        Some("0.46.0"),
        "from the cask path"
    );
    assert!(!codex.outdated, "a non-semver latest makes no claim");
    let cursor = r.agents.iter().find(|a| a.name == "cursor-agent").unwrap();
    assert_eq!(cursor.latest, None);
    assert_eq!(cursor.installed, None, "not semver: unknown");
    assert!(cursor
        .version_from
        .as_deref()
        .unwrap()
        .contains("--version is not run"));
    assert!(!cursor.outdated);
    // No agent tool was run: only brew.
    let runs = run.runs.lock().unwrap().clone();
    assert!(runs.iter().all(|x| x.starts_with("brew ")), "{runs:?}");
    assert_eq!(
        r.homebrew_path.as_deref(),
        Some(hb.join("bin/brew").as_path())
    );
    assert_eq!(r.homebrew.as_ref().unwrap().len(), 1, "pinned left out");
    assert!(run.runs.lock().unwrap().iter().any(
        |x| x.contains("HOMEBREW_NO_AUTO_UPDATE") && x.contains("HOMEBREW_NO_INSTALL_FROM_API")
    ));
    let f = findings(&r);
    let kinds: Vec<FindingKind> = f.iter().map(|f| f.kind).collect();
    assert!(kinds.contains(&FindingKind::MulchieUpdate));
    assert!(kinds.contains(&FindingKind::AgentUpdate));
    assert!(kinds.contains(&FindingKind::HomebrewOutdated));
    let adv = f.iter().find(|f| f.kind == FindingKind::Advisory).unwrap();
    assert!(
        adv.title.contains("lodash 4.17.20 (fixed in 4.17.21)"),
        "{}",
        adv.title
    );
    assert!(adv.suggested[0]
        .label
        .contains("npm install lodash@4.17.21"));
    assert!(f.iter().all(|f| f.category == FindingCategory::Update));
    // A3: every update finding carries its packages as data.
    let item = |k: FindingKind, name: &str| {
        f.iter()
            .filter(|f| f.kind == k)
            .flat_map(|f| f.evidence.updates.clone())
            .find(|u| u.package == name)
            .unwrap_or_else(|| panic!("{name}: {f:?}"))
    };
    let u = item(FindingKind::AgentUpdate, "claude");
    assert_eq!(u.installed.as_deref(), Some("2.0.14"));
    assert_eq!(u.latest, claude.latest);
    assert_eq!(u.source, claude.source);
    assert_eq!(u.command.as_deref(), Some("claude update"));
    let u = item(FindingKind::MulchieUpdate, "mulchie");
    assert_eq!(
        u.command.as_deref(),
        Some("brew upgrade williamrogersdev/tap/mulchie-cli")
    );
    let brew = &r.homebrew.as_ref().unwrap()[0];
    let u = item(FindingKind::HomebrewOutdated, &brew.name);
    assert_eq!(u.installed.as_deref(), Some(brew.installed.as_str()));
    assert_eq!(u.latest.as_deref(), Some(brew.current.as_str()));
    assert_eq!(u.source, "homebrew");
    assert!(u.command.unwrap().starts_with("brew upgrade "));
    let u = item(FindingKind::Advisory, "lodash");
    assert_eq!(
        (
            u.installed.as_deref(),
            u.latest.as_deref(),
            u.source.as_str()
        ),
        (Some("4.17.20"), Some("4.17.21"), "osv.dev")
    );
    assert!(u.command.unwrap().contains("npm install lodash@4.17.21"));
    let v = serde_json::to_value(&adv.evidence).unwrap();
    assert!(v["updates"][0]["package"].is_string(), "{v}");
    // Every request carried no secret and went to the known hosts.
    for c in http.calls.lock().unwrap().iter() {
        assert!(
            c.contains("api.github.com")
                || c.contains("registry.npmjs.org")
                || c.contains("api.osv.dev"),
            "{c}"
        );
    }

    // The network fails: previous results stay, errors say why.
    let dead = FakeHttp::default();
    let r2 = check(&dead, &run, &inp, Some(&r));
    assert_eq!(r2.mulchie, r.mulchie);
    assert_eq!(r2.advisories, r.advisories);
    assert!(r2.errors.iter().any(|e| e.starts_with("mulchie:")));
}

#[test]
fn homebrew_needs_local_data() {
    let run = FakeRunner {
        tools: HashMap::from([("brew".into(), (true, "{\"formulae\": []}".into()))]),
        ..FakeRunner::default()
    };
    let mut errs = Vec::new();
    assert_eq!(
        homebrew(&run, &[], &mut errs),
        BrewOutcome::Skipped("no_core_tap")
    );
    assert!(
        run.runs.lock().unwrap().is_empty(),
        "brew not run without its local core tap"
    );
    assert!(parse_brew_outdated("{}").is_err());
    assert!(parse_brew_outdated("garbage").is_err());
}

fn brew_setup(t: &Path, output: &str) -> FakeRunner {
    let hb = t.join("hb");
    std::fs::create_dir_all(hb.join("bin")).unwrap();
    std::fs::create_dir_all(hb.join("Library/Taps/homebrew/homebrew-core")).unwrap();
    std::fs::write(hb.join("bin/brew"), "").unwrap();
    FakeRunner {
        tools: HashMap::from([("brew".into(), (false, output.to_string()))]),
        paths: HashMap::from([("brew".into(), hb.join("bin/brew"))]),
        ..FakeRunner::default()
    }
}

fn previous_report() -> Report {
    Report {
        homebrew: Some(vec![BrewOutdated {
            name: "git".into(),
            installed: "2.44.0".into(),
            current: "2.45.0".into(),
            cask: false,
        }]),
        advisories: Some(vec![]),
        packages_checked: 3,
        ..Report::default()
    }
}

/// A skipped Homebrew check drops the previous result; one that ran and
/// failed keeps it.
#[test]
fn skipped_homebrew_drops_the_previous_result_and_a_failure_keeps_it() {
    let t = tempfile::tempdir().unwrap();
    let prev = previous_report();
    let http = FakeHttp::default();
    let base = UpdatesConfig {
        mulchie: false,
        agents: false,
        advisories: false,
        ..UpdatesConfig::default()
    };
    let go = |cfg: &UpdatesConfig, run: &FakeRunner| {
        check(&http, run, &input(cfg, &[], Path::new("/h")), Some(&prev))
    };
    // Turned off.
    let off = UpdatesConfig {
        homebrew: false,
        ..base.clone()
    };
    let r = go(&off, &brew_setup(t.path(), "{}"));
    assert!(r.homebrew.is_none());
    assert_eq!(r.homebrew_skipped.as_deref(), Some("off"));
    assert!(findings(&r).is_empty());
    // No brew binary.
    let r = go(&base, &FakeRunner::default());
    assert!(r.homebrew.is_none());
    assert_eq!(r.homebrew_skipped.as_deref(), Some("no_brew"));
    assert!(findings(&r).is_empty());
    // brew without a local core tap.
    let run = FakeRunner {
        tools: HashMap::from([("brew".into(), (true, "{}".into()))]),
        ..FakeRunner::default()
    };
    let r = go(&base, &run);
    assert!(r.homebrew.is_none());
    assert_eq!(r.homebrew_skipped.as_deref(), Some("no_core_tap"));
    // Ran and failed: previous kept, error recorded, not marked skipped.
    let r = go(&base, &brew_setup(t.path(), "garbage"));
    assert_eq!(r.homebrew, prev.homebrew);
    assert!(r.homebrew_skipped.is_none());
    assert!(r.errors.iter().any(|e| e.starts_with("homebrew:")));
    assert!(findings(&r)
        .iter()
        .any(|f| f.kind == FindingKind::HomebrewOutdated));
    // Ran and succeeded: replaced.
    let r = go(
        &base,
        &brew_setup(t.path(), "{\"formulae\": [], \"casks\": []}"),
    );
    assert_eq!(r.homebrew, Some(vec![]));
}

/// Advisories turned off drop the previous result; a failed lookup keeps it.
#[test]
fn advisories_off_drops_the_previous_result_and_a_failure_keeps_it() {
    let t = tempfile::tempdir().unwrap();
    let ws = t.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"node_modules/lodash":{"version":"4.17.20"}}}"#,
    )
    .unwrap();
    let wss = [ws];
    let prev = previous_report();
    let http = FakeHttp::default(); // every request is refused
    let base = UpdatesConfig {
        mulchie: false,
        agents: false,
        homebrew: false,
        ..UpdatesConfig::default()
    };
    let off = UpdatesConfig {
        advisories: false,
        ..base.clone()
    };
    let run = FakeRunner::default();
    let r = check(
        &http,
        &run,
        &input(&off, &wss, Path::new("/h")),
        Some(&prev),
    );
    assert!(r.advisories.is_none());
    assert_eq!(r.packages_checked, 0);
    let on = UpdatesConfig {
        advisories: true,
        ..base
    };
    let r = check(&http, &run, &input(&on, &wss, Path::new("/h")), Some(&prev));
    assert_eq!(r.advisories, prev.advisories);
    assert_eq!(r.packages_checked, 3);
    assert!(r.errors.iter().any(|e| e.starts_with("advisories:")));
}

#[test]
fn backoff_doubles_up_to_the_interval() {
    let day = 86_400;
    assert_eq!(next_check(0, day, 0), day);
    assert_eq!(next_check(0, day, 1), 900);
    assert_eq!(next_check(0, day, 2), 1_800);
    assert_eq!(next_check(0, day, 30), day);
}

#[test]
fn cache_round_trips_and_tolerates_garbage() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(load_cache(t.path()), Cache::default());
    let c = Cache {
        report: Some(Report {
            checked_unix: 5,
            ..Report::default()
        }),
        next_unix: 9,
        failures: 1,
    };
    save_cache(t.path(), &c).unwrap();
    assert_eq!(load_cache(t.path()), c);
    std::fs::write(cache_path(t.path()), "{{{").unwrap();
    assert_eq!(load_cache(t.path()), Cache::default());
}

/// Hostile lockfiles and replies never panic.
#[test]
fn hostile_input_never_panics() {
    let junk = [
        "",
        "\0\0\0",
        "[[package]]",
        "{\"packages\": {\"node_modules/\": {\"version\": 5}}}",
        "{\"dependencies\": {\"a\": {\"dependencies\": 3}}}",
        "require (\nrequire\n)\nreplace =>\n",
        "require x v\nrequire y vv1.2.3\n",
        &"[".repeat(10_000),
    ];
    for j in junk {
        let _ = parse_cargo_lock(j);
        let _ = parse_package_lock(j);
        let _ = parse_go_mod(j);
        let _ = parse_brew_outdated(j);
        let _ = version_in(j);
        if let Ok(v) = serde_json::from_str::<Value>(j) {
            let _ = parse_vuln(&v);
        }
    }
}

/// N10: where executables are looked for, what a probe runs with, and the
/// installed version read from metadata only.
#[test]
fn probes_come_from_safe_places_and_versions_from_metadata() {
    let home = Path::new("/Users/u");
    let ws = PathBuf::from("/Users/u/code/app");
    let path = std::ffi::OsString::from(
        ".:node_modules/.bin:/Users/u/code/app/node_modules/.bin:/opt/x/bin::/usr/bin",
    );
    let dirs = search_dirs(Some(&path), home, std::slice::from_ref(&ws));
    assert!(dirs.iter().all(|d| d.is_absolute()), "{dirs:?}");
    assert!(!dirs.iter().any(|d| d.starts_with(&ws)), "{dirs:?}");
    assert_eq!(dirs[0], PathBuf::from("/opt/x/bin"));
    assert!(dirs.contains(&PathBuf::from("/Users/u/.local/bin")));
    assert_eq!(
        dirs.iter()
            .filter(|d| d.as_path() == Path::new("/usr/bin"))
            .count(),
        1
    );
    // The default find_outside refuses a tool inside a workspace.
    let run = FakeRunner {
        tools: HashMap::from([("claude".into(), (true, String::new()))]),
        paths: HashMap::from([("claude".into(), ws.join("node_modules/.bin/claude"))]),
        ..FakeRunner::default()
    };
    assert_eq!(run.find_outside("claude", std::slice::from_ref(&ws)), None);
    // A probe's PATH: its own directory, then system directories only.
    assert_eq!(
        probe_path(Path::new("/nonexistent/opt/bin/brew")),
        "/nonexistent/opt/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );
    // Versions: package.json of the right package, or a version directory;
    // a node version directory is not the tool's version.
    let t = tempfile::tempdir().unwrap();
    let nvm = t
        .path()
        .join("nvm/versions/node/v20.11.0/lib/node_modules/@openai/codex");
    std::fs::create_dir_all(nvm.join("bin")).unwrap();
    std::fs::write(nvm.join("bin/codex.js"), "").unwrap();
    std::fs::write(
        nvm.join("package.json"),
        json!({"name": "@openai/codex", "version": "0.50.1"}).to_string(),
    )
    .unwrap();
    let (v, from) = installed_from_metadata(&nvm.join("bin/codex.js"), Some("@openai/codex"));
    assert_eq!(v.as_deref(), Some("0.50.1"), "{from}");
    // Another package's package.json is not read as this one's.
    let (v, _) = installed_from_metadata(&nvm.join("bin/codex.js"), Some("@google/gemini-cli"));
    assert_eq!(v, None, "the node version directory is not a tool version");
    let native = t.path().join("share/claude/versions/2.0.20");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    std::fs::write(&native, "").unwrap();
    let (v, from) = installed_from_metadata(&native, Some("@anthropic-ai/claude-code"));
    assert_eq!(v.as_deref(), Some("2.0.20"));
    assert!(from.starts_with("install path"));
    // brew is not run from a workspace either.
    let run = FakeRunner {
        tools: HashMap::from([("brew".into(), (true, "{}".into()))]),
        paths: HashMap::from([("brew".into(), ws.join("bin/brew"))]),
        ..FakeRunner::default()
    };
    let mut errs = Vec::new();
    assert_eq!(
        homebrew(&run, std::slice::from_ref(&ws), &mut errs),
        BrewOutcome::Skipped("no_brew")
    );
    assert!(run.runs.lock().unwrap().is_empty());
}
