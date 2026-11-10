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

// ------------------------------------------------------------------ YAML
//
// `docs/openapi.yaml` must parse as YAML, not only list the right paths: it
// was once invalid (plain scalars holding `: `) and the path test did not
// notice. No YAML crate is a dependency, so this is a strict checker for the
// subset the file uses: block mappings and sequences, `|`/`>` block
// scalars, one-line flow collections, quoted and plain scalars. Anything it
// does not understand is an error, so it errs on the side of failing.

struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
}

fn yaml_lines(src: &str) -> Result<Vec<Line<'_>>, String> {
    let mut out = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let body = raw.trim_start_matches(' ');
        if body.starts_with('\t') {
            return Err(format!("line {}: a tab in the indentation", i + 1));
        }
        out.push(Line {
            no: i + 1,
            indent: raw.len() - body.len(),
            text: body.trim_end(),
        });
    }
    Ok(out)
}

struct Yaml<'a> {
    lines: Vec<Line<'a>>,
    i: usize,
}

fn err<T>(no: usize, msg: impl std::fmt::Display) -> Result<T, String> {
    Err(format!("line {no}: {msg}"))
}

/// Strip a trailing ` #` comment outside quotes.
fn uncomment(s: &str) -> &str {
    let (mut sq, mut dq, mut prev) = (false, false, ' ');
    for (k, c) in s.char_indices() {
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq && prev != '\\' => dq = !dq,
            '#' if !sq && !dq && prev == ' ' => return s[..k].trim_end(),
            _ => {}
        }
        prev = c;
    }
    s
}

impl<'a> Yaml<'a> {
    fn skip_blank(&mut self) {
        while self.i < self.lines.len() {
            let t = self.lines[self.i].text;
            if t.is_empty() || t.starts_with('#') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<(usize, usize, &'a str)> {
        self.skip_blank();
        self.lines.get(self.i).map(|l| (l.no, l.indent, l.text))
    }

    fn block(&mut self, indent: usize) -> Result<(), String> {
        let Some((_, ind, t)) = self.peek() else {
            return Ok(());
        };
        if ind != indent {
            return Ok(());
        }
        if t == "-" || t.starts_with("- ") {
            self.sequence(indent)
        } else {
            self.mapping(indent)
        }
    }

    /// A value after `key:` or `- ` on the same line, or below it.
    fn value(&mut self, no: usize, indent: usize, rest: &str) -> Result<(), String> {
        let rest = uncomment(rest);
        if rest.is_empty() {
            return match self.peek() {
                Some((_, ind, _)) if ind > indent => self.block(ind),
                // `key:` followed by a sequence at the same indent.
                Some((_, ind, t)) if ind == indent && (t == "-" || t.starts_with("- ")) => {
                    self.sequence(ind)
                }
                _ => Ok(()), // null
            };
        }
        if matches!(rest, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            while self.i < self.lines.len() {
                let l = &self.lines[self.i];
                if l.text.is_empty() || l.indent > indent {
                    self.i += 1;
                } else {
                    break;
                }
            }
            return Ok(());
        }
        let used = scalar_or_flow(rest, false).map_err(|e| format!("line {no}: {e}"))?;
        if used != rest.len() {
            return err(
                no,
                format!("unexpected text after a value: {:?}", &rest[used..]),
            );
        }
        Ok(())
    }

    fn mapping(&mut self, indent: usize) -> Result<(), String> {
        let mut keys = BTreeSet::new();
        while let Some((no, ind, t)) = self.peek() {
            if ind < indent {
                return Ok(());
            }
            if ind > indent {
                return err(no, "unexpected indentation");
            }
            if t == "-" || t.starts_with("- ") {
                return err(no, "a list item where a key was expected");
            }
            let (key, rest) = split_key(t).map_err(|e| format!("line {no}: {e}"))?;
            if !keys.insert(key.clone()) {
                return err(no, format!("duplicate key {key:?}"));
            }
            self.i += 1;
            self.value(no, indent, rest)?;
        }
        Ok(())
    }

    fn sequence(&mut self, indent: usize) -> Result<(), String> {
        while let Some((no, ind, t)) = self.peek() {
            if ind < indent {
                return Ok(());
            }
            if ind > indent {
                return err(no, "unexpected indentation");
            }
            if !(t == "-" || t.starts_with("- ")) {
                return Ok(());
            }
            let rest = t[1..].trim_start();
            let inner = indent + (t.len() - rest.len());
            if !rest.is_empty()
                && !rest.starts_with(['{', '[', '"', '\''])
                && split_key(rest).is_ok()
            {
                // `- key: value` starts a mapping at the item's column.
                let l = &mut self.lines[self.i];
                l.indent = inner;
                l.text = rest;
                self.mapping(inner)?;
            } else {
                self.i += 1;
                self.value(no, indent, rest)?;
            }
        }
        Ok(())
    }
}

/// `key: rest` or `key:`; the key plain or quoted.
fn split_key(t: &str) -> Result<(String, &str), String> {
    if t.starts_with(['"', '\'']) {
        let n = quoted(t)?;
        let after = &t[n..];
        let Some(rest) = after.strip_prefix(':') else {
            return Err("a quoted key without `:`".into());
        };
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return Err("`:` after a key must be followed by a space".into());
        }
        return Ok((t[..n].to_string(), rest.trim_start()));
    }
    let k = if let Some(k) = t.find(": ") {
        k
    } else if t.ends_with(':') {
        t.len() - 1
    } else {
        return Err(format!("expected `key: value`, found {t:?}"));
    };
    let key = &t[..k];
    plain_ok(key, false)?;
    Ok((key.to_string(), t[k + 1..].trim_start()))
}

/// Bytes used by a quoted scalar at the start of `s`.
fn quoted(s: &str) -> Result<usize, String> {
    let q = s.chars().next().unwrap();
    let mut it = s.char_indices().skip(1).peekable();
    while let Some((k, c)) = it.next() {
        if q == '"' && c == '\\' {
            match it.next() {
                Some((_, e)) if "0abtnvfre \"/\\N_LPxuU\t".contains(e) => continue,
                other => return Err(format!("bad escape \\{:?}", other.map(|x| x.1))),
            }
        }
        if c == q {
            if q == '\'' && it.peek().map(|x| x.1) == Some('\'') {
                it.next();
                continue;
            }
            return Ok(k + 1);
        }
    }
    Err("an unterminated quoted string".into())
}

/// A plain scalar YAML accepts as written.
fn plain_ok(s: &str, flow: bool) -> Result<(), String> {
    let first = s.chars().next().ok_or("an empty plain scalar")?;
    if "`@!&*|>%{}[],#'\"".contains(first) {
        return Err(format!(
            "a plain scalar cannot start with {first:?}: {s:?}; quote it"
        ));
    }
    if matches!(first, '-' | '?' | ':') && s[1..].starts_with(' ') {
        return Err(format!("{s:?} starts with an indicator; quote it"));
    }
    if s.contains(": ") || s.ends_with(':') {
        return Err(format!("`: ` inside a plain scalar ({s:?}); quote it"));
    }
    if s.contains(" #") {
        return Err(format!(
            "` #` inside a plain scalar starts a comment ({s:?}); quote it"
        ));
    }
    if flow && s.contains([',', '[', ']', '{', '}']) {
        return Err(format!("{s:?} holds a flow indicator; quote it"));
    }
    Ok(())
}

/// Bytes used by one scalar or flow collection at the start of `s`.
fn scalar_or_flow(s: &str, flow: bool) -> Result<usize, String> {
    let s0 = s;
    let s = s.trim_start();
    let lead = s0.len() - s.len();
    let used = match s.chars().next() {
        Some('{') | Some('[') => {
            let close = if s.starts_with('{') { '}' } else { ']' };
            let map = close == '}';
            let mut k = 1;
            let mut keys = BTreeSet::new();
            loop {
                let rest = &s[k..];
                let t = rest.trim_start();
                k += rest.len() - t.len();
                if t.starts_with(close) {
                    break k + 1;
                }
                if map {
                    let (kn, after) = if t.starts_with(['"', '\'']) {
                        let n = quoted(t)?;
                        (n, &t[n..])
                    } else {
                        let n = t
                            .find(':')
                            .ok_or_else(|| format!("a flow mapping entry without `:`: {t:?}"))?;
                        plain_ok(t[..n].trim_end(), true)?;
                        (n, &t[n..])
                    };
                    if !keys.insert(t[..kn].trim_end().to_string()) {
                        return Err(format!("duplicate key {:?}", t[..kn].trim_end()));
                    }
                    let Some(after) = after.strip_prefix(':') else {
                        return Err(format!("expected `:` after a key in {s:?}"));
                    };
                    if !after.starts_with(' ') {
                        return Err("`:` in a flow mapping must be followed by a space".into());
                    }
                    k += kn + 1;
                }
                let n = scalar_or_flow(&s[k..], true)?;
                k += n;
                let rest = &s[k..];
                let t = rest.trim_start();
                k += rest.len() - t.len();
                if let Some(t2) = t.strip_prefix(',') {
                    let _ = t2;
                    k += 1;
                } else if t.starts_with(close) {
                    break k + 1;
                } else {
                    return Err(format!("expected `,` or `{close}` in {s:?}"));
                }
            }
        }
        Some('"') | Some('\'') => quoted(s)?,
        Some(_) => {
            let end = if flow {
                s.find([',', ']', '}']).unwrap_or(s.len())
            } else {
                s.len()
            };
            let v = s[..end].trim_end();
            plain_ok(v, flow)?;
            v.len()
        }
        None => return Err("a missing value".into()),
    };
    Ok(lead + used)
}

fn yaml_check(src: &str) -> Result<(), String> {
    let mut y = Yaml {
        lines: yaml_lines(src)?,
        i: 0,
    };
    y.block(0)?;
    match y.peek() {
        None => Ok(()),
        Some((no, _, t)) => err(no, format!("not part of the document: {t:?}")),
    }
}

#[test]
fn yaml_checker_catches_what_broke_the_file_before() {
    for bad in [
        // The kind of line that made docs/openapi.yaml invalid.
        "paths:\n  /v1/lease:\n    post:\n      description: A lease needs a `pid`. Poll with: the ticket.\n",
        "a:\n  description: Send `{\"items\": [...]}` back.\n",
        "a: `code` first\n",
        "a: 1\na: 2\n",
        "a:\n   b: 1\n  c: 2\n",
        "a: { b: 1, c: x: y }\n",
        "a: { b: 1\n",
        "a: \"unterminated\n",
        "a:\n\tb: 1\n",
        "- a\nb: 1\n",
    ] {
        assert!(yaml_check(bad).is_err(), "accepted: {bad:?}");
    }
    for good in [
        "a: 1\nb:\n  - x\n  - { y: 1, z: [a, \"b\"] }\nc: |\n  text: with colon\n\n  more\nd: 'it''s'\n",
        "a:\n- 1\n- 2\nb: \"#/x\" # comment\n",
        "a:\n  - name: x\n    in: query\n  - name: y\n",
    ] {
        yaml_check(good).unwrap_or_else(|e| panic!("{e}\n{good}"));
    }
}

#[test]
fn openapi_is_valid_yaml() {
    let Some(text) = root_file("docs/openapi.yaml") else {
        eprintln!("docs/openapi.yaml not found; skipped");
        return;
    };
    if let Err(e) = yaml_check(&text) {
        panic!("docs/openapi.yaml is not valid YAML: {e}");
    }
    // A second opinion from a full YAML parser, where one is installed.
    let ruby = std::process::Command::new("ruby")
        .args(["-ryaml", "-e", "YAML.load_file(ARGV[0])"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.yaml"))
        .output();
    if let Ok(o) = ruby {
        assert!(
            o.status.success(),
            "Ruby's YAML parser rejects docs/openapi.yaml: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}
