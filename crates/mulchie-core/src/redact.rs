//! Redaction of secrets in command lines before they leave the engine (state,
//! API, journal, findings). Agents read this output; a token on a command line
//! must not end up in an agent's context.
//!
//! Redacted:
//! - `--token=VALUE`, `--api-key VALUE`, `-password VALUE`: the value after a
//!   flag whose name contains a secret word;
//! - `GITHUB_TOKEN=VALUE` and other `KEY=VALUE` arguments with a secret key;
//! - `Authorization: …` / `Bearer …` header arguments;
//! - credentials in URLs (`https://user:PASSWORD@host`);
//! - arguments that look like well-known token formats (`ghp_…`, `sk-…`,
//!   `xoxb-…`, `AKIA…`, `glpat-…`, JWTs).
//!
//! - header values given as one string (`-H "Authorization: Bearer X"`,
//!   `--header=x-api-key:V`);
//! - short secret flags of known programs (`curl -u user:pass`, `mysql -pX`,
//!   `docker login -p X`, `redis-cli -a X`);
//! - a token used as URL user info (`https://ghp_…@github.com`).
//!
//! It is a best effort: a secret passed positionally with no telling flag
//! (`tool SECRET`) is not recognised unless it has a known token shape.

pub const REDACTED: &str = "[redacted]";

/// Words that mark a flag or variable name as holding a secret.
const SECRET_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "pass",
    "passphrase",
    "apikey",
    "key",
    "auth",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "session",
    "bearer",
    "pat",
    "dsn",
];

/// Joined names that are secret even without separators.
const SECRET_JOINED: &[&str] = &[
    "apikey",
    "accesskey",
    "privatekey",
    "secretkey",
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "authkey",
];

/// Prefixes of well-known token formats.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "npm_",
    "hf_",
];

/// Whether a flag or variable name (`--api-key`, `GITHUB_TOKEN`) names a secret.
pub fn is_secret_name(name: &str) -> bool {
    let n = name.trim_start_matches('-').to_ascii_lowercase();
    if n.is_empty() {
        return false;
    }
    let joined: String = n.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if SECRET_JOINED.iter().any(|w| joined.contains(w)) {
        return true;
    }
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| SECRET_WORDS.contains(&w))
}

/// Whether a lone argument looks like a token in a well-known format.
pub fn looks_like_token(arg: &str) -> bool {
    let a = arg.trim_matches(|c| c == '"' || c == '\'');
    if a.len() >= 16 && TOKEN_PREFIXES.iter().any(|p| a.starts_with(p)) {
        return true;
    }
    // A JWT: three base64url parts, the first starting with `eyJ`.
    let parts: Vec<&str> = a.split('.').collect();
    parts.len() == 3 && a.starts_with("eyJ") && a.len() > 40
}

/// Header names whose value is a credential.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-access-token",
    "private-token",
    "x-amz-security-token",
    "cookie",
    "set-cookie",
];

/// Authorization schemes: the word after one of these is the credential.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "token", "digest", "negotiate"];

/// Short flags that take a secret, per program. `login` names a subcommand
/// that must also be present (`docker login -p`, not `docker run -p 80:80`).
struct ShortSecret {
    programs: &'static [&'static str],
    flags: &'static [&'static str],
    subcommand: Option<&'static str>,
}

const SHORT_SECRETS: &[ShortSecret] = &[
    ShortSecret {
        programs: &["curl"],
        flags: &["-u", "-U", "--user", "--proxy-user"],
        subcommand: None,
    },
    ShortSecret {
        programs: &[
            "mysql",
            "mysqldump",
            "mysqladmin",
            "mysqlimport",
            "mysqlshow",
            "mariadb",
            "mariadb-dump",
            "mariadb-admin",
        ],
        flags: &["-p"],
        subcommand: None,
    },
    ShortSecret {
        programs: &["docker", "podman", "nerdctl", "buildah", "skopeo", "helm"],
        flags: &["-p"],
        subcommand: Some("login"),
    },
    ShortSecret {
        programs: &["redis-cli", "valkey-cli", "keydb-cli"],
        flags: &["-a"],
        subcommand: None,
    },
    ShortSecret {
        programs: &[
            "mongo",
            "mongosh",
            "mongodump",
            "mongorestore",
            "mongoexport",
            "mongoimport",
        ],
        flags: &["-p"],
        subcommand: None,
    },
    ShortSecret {
        programs: &["sshpass"],
        flags: &["-p"],
        subcommand: None,
    },
];

fn program_name(arg: &str) -> &str {
    let a = arg.trim_matches(|c| c == '"' || c == '\'');
    a.rsplit('/').next().unwrap_or(a)
}

fn is_short_program(name: &str) -> bool {
    SHORT_SECRETS.iter().any(|s| s.programs.contains(&name))
}

/// Whether `arg` is a short secret flag of `program`: `Some(true)` when the
/// value is attached (`-ppass`), `Some(false)` when it is the next argument.
fn short_secret(program: &str, args: &[String], arg: &str) -> Option<bool> {
    for s in SHORT_SECRETS {
        if !s.programs.contains(&program) {
            continue;
        }
        if let Some(sub) = s.subcommand {
            if !args.iter().any(|a| a == sub) {
                continue;
            }
        }
        for f in s.flags {
            if arg == *f {
                return Some(false);
            }
            // Attached values only for single-letter flags (`-ppass`).
            if f.len() == 2 && arg.len() > 2 && arg.starts_with(f) && !arg.starts_with("--") {
                return Some(true);
            }
        }
    }
    None
}

/// A user name that is really a token (`https://ghp_…@github.com`, a long
/// random string): long, no separators, letters and digits mixed.
fn token_shaped(s: &str) -> bool {
    if looks_like_token(s) {
        return true;
    }
    s.len() >= 20
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Hide the password in `scheme://user:password@host…`, and a token used as
/// the whole user info (`https://TOKEN@host`).
fn redact_url(arg: &str) -> Option<String> {
    let i = arg.find("://")?;
    let rest = &arg[i + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let at = authority.rfind('@')?;
    let userinfo = &authority[..at];
    match userinfo.split_once(':') {
        Some((user, _)) => Some(format!(
            "{}{user}:{REDACTED}@{}",
            &arg[..i + 3],
            &rest[at + 1..]
        )),
        None if token_shaped(userinfo) => {
            Some(format!("{}{REDACTED}@{}", &arg[..i + 3], &rest[at + 1..]))
        }
        None => None,
    }
}

/// `name:` at the start of `s` when `name` is a secret header.
fn header_prefix(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    SECRET_HEADERS.iter().find_map(|h| {
        let rest = lower.strip_prefix(h)?;
        rest.starts_with(':').then_some(h.len())
    })
}

/// One argument on its own (no look at its neighbours).
fn redact_one(arg: &str) -> Option<String> {
    redact_one_at(arg, 0)
}

fn redact_one_at(arg: &str, depth: u8) -> Option<String> {
    if looks_like_token(arg) {
        return Some(REDACTED.into());
    }
    if let Some(n) = header_prefix(arg) {
        // A bare `Authorization:` hides the next argument instead.
        if !arg[n + 1..].trim().is_empty() {
            return Some(format!("{}: {REDACTED}", &arg[..n]));
        }
    }
    let lower = arg.to_ascii_lowercase();
    if let Some(scheme) = AUTH_SCHEMES
        .iter()
        .find(|s| lower.starts_with(*s) && lower[s.len()..].starts_with(' '))
    {
        return Some(format!("{} {REDACTED}", &arg[..scheme.len()]));
    }
    if let Some((k, v)) = arg.split_once('=') {
        if !v.is_empty() && !k.contains('/') && is_secret_name(k) {
            return Some(format!("{k}={REDACTED}"));
        }
        // The value after `=` gets the same checks as a lone argument:
        // `--jwt=eyJ…`, `--header=Authorization:Bearer …`, `DB=postgres://u:p@h`.
        if depth < 4 && !v.is_empty() {
            if let Some(x) = redact_one_at(v, depth + 1) {
                return Some(format!("{k}={x}"));
            }
        }
    }
    redact_url(arg)
}

/// Redact secrets in an argument vector.
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    let mut program = args.first().map(|a| program_name(a)).unwrap_or("");
    for (i, a) in args.iter().enumerate() {
        if hide_next {
            hide_next = false;
            if !a.starts_with('-') || a.len() == 1 {
                // `Authorization: Bearer X` split into words: hide the scheme's
                // credential too.
                if AUTH_SCHEMES.contains(&a.to_ascii_lowercase().as_str()) {
                    out.push(a.clone());
                    hide_next = true;
                } else {
                    out.push(REDACTED.to_string());
                }
                continue;
            }
        }
        // A wrapped program (`sudo mysql …`, `docker exec c redis-cli …`).
        if i > 0 && !a.starts_with('-') && is_short_program(program_name(a)) {
            program = program_name(a);
        }
        if i > 0 {
            match short_secret(program, args, a) {
                Some(true) => {
                    out.push(format!("{}{REDACTED}", &a[..2]));
                    continue;
                }
                Some(false) => {
                    out.push(a.clone());
                    hide_next = true;
                    continue;
                }
                None => {}
            }
        }
        if let Some(r) = redact_one(a) {
            out.push(r);
            continue;
        }
        let lower = a.to_ascii_lowercase();
        // A header name or `Bearer` on its own: the credential follows.
        if i > 0
            && (lower == "bearer"
                || lower
                    .strip_suffix(':')
                    .is_some_and(|h| SECRET_HEADERS.contains(&h)))
        {
            out.push(a.clone());
            hide_next = true;
            continue;
        }
        // `--token VALUE` / `-p VALUE` style: a flag naming a secret without
        // `=` hides the next argument. Not for argv[0].
        let is_flag = i > 0 && a.starts_with('-') && !a.contains('=') && a.len() > 2;
        if is_flag && is_secret_name(a) {
            hide_next = true;
        }
        out.push(a.clone());
    }
    out
}

/// Before splitting a command line on whitespace, hide header values that
/// contain spaces (`-H "Authorization: Bearer X"`, `x-api-key: v`). A quoted
/// header is hidden up to its closing quote; an unquoted one for one word, or
/// two when the first is an auth scheme.
fn redact_headers(cmd: &str) -> String {
    let lower = cmd.to_ascii_lowercase();
    let bytes = cmd.as_bytes();
    let mut out = String::with_capacity(cmd.len());
    let mut i = 0;
    // The quote we are inside, if any (shell-style, no escapes inside '…').
    let mut in_quote: Option<u8> = None;
    while i < cmd.len() {
        let boundary = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'"' | b'\'' | b'=' | b'\n');
        let hit = boundary
            .then(|| {
                SECRET_HEADERS
                    .iter()
                    .find(|h| lower[i..].starts_with(*h) && lower[i + h.len()..].starts_with(':'))
            })
            .flatten();
        let Some(h) = hit else {
            let c = cmd[i..].chars().next().unwrap_or(' ');
            match (in_quote, bytes[i]) {
                (None, q @ (b'"' | b'\'')) => in_quote = Some(q),
                (Some(q), b) if b == q => in_quote = None,
                _ => {}
            }
            out.push(c);
            i += c.len_utf8();
            continue;
        };
        let colon = i + h.len();
        out.push_str(&cmd[i..=colon]);
        out.push(' ');
        out.push_str(REDACTED);
        let quote = in_quote;
        let mut j = colon + 1;
        if let Some(q) = quote {
            j = cmd[j..].find(q as char).map_or(cmd.len(), |k| j + k);
        } else {
            let mut words = 0;
            loop {
                while j < cmd.len() && matches!(bytes[j], b' ' | b'\t') {
                    j += 1;
                }
                let start = j;
                while j < cmd.len() && !matches!(bytes[j], b' ' | b'\t' | b'\n') {
                    j += 1;
                }
                words += 1;
                let w = lower[start..j].to_string();
                if words >= 2 || !AUTH_SCHEMES.contains(&w.as_str()) || j >= cmd.len() {
                    break;
                }
            }
        }
        i = j;
    }
    out
}

/// Redact a command line given as one string (split on whitespace).
pub fn redact_command_line(cmd: &str) -> String {
    let cmd = redact_headers(cmd);
    let parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
    redact_args(&parts).join(" ")
}

/// A lease label for display. `mulchie run` sends `<directory>: <program>
/// [subcommand]`; anything beyond short plain words is not needed and is
/// hidden: at most 8 words of at most 64 characters, made of letters, digits
/// and `._:-+@/`, none of them token-shaped, 120 characters in all.
pub fn redact_label(label: &str) -> String {
    let cleaned = redact_command_line(label);
    let plain = |w: &str| {
        w == REDACTED
            || (w.chars().count() <= 64
                && w.chars()
                    .all(|c| c.is_alphanumeric() || "._:-+@/".contains(c))
                && !token_shaped(w.trim_end_matches(':')))
    };
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .take(8)
        .map(|w| if plain(w) { w } else { REDACTED })
        .collect();
    words.join(" ").chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &[&str]) -> Vec<String> {
        redact_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn flags_and_their_values() {
        assert_eq!(
            r(&["tool", "--token=abc123", "--verbose"]),
            ["tool", "--token=[redacted]", "--verbose"]
        );
        assert_eq!(
            r(&["tool", "--api-key", "abc123", "--port", "80"]),
            ["tool", "--api-key", "[redacted]", "--port", "80"]
        );
        assert_eq!(r(&["tool", "--password", "hunter2"])[2], REDACTED);
        assert_eq!(
            r(&["tool", "--client-secret=x"])[1],
            "--client-secret=[redacted]"
        );
        assert_eq!(r(&["tool", "--auth", "user:pw"])[2], REDACTED);
        // A secret flag followed by another flag hides nothing extra.
        assert_eq!(
            r(&["tool", "--token", "--next"]),
            ["tool", "--token", "--next"]
        );
    }

    #[test]
    fn env_style_assignments() {
        assert_eq!(
            r(&[
                "env",
                "GITHUB_TOKEN=ghx",
                "AWS_SECRET_ACCESS_KEY=y",
                "PORT=3000",
                "node"
            ]),
            [
                "env",
                "GITHUB_TOKEN=[redacted]",
                "AWS_SECRET_ACCESS_KEY=[redacted]",
                "PORT=3000",
                "node"
            ]
        );
        assert_eq!(
            r(&["x", "OPENAI_API_KEY=sk"])[1],
            "OPENAI_API_KEY=[redacted]"
        );
        assert_eq!(
            r(&["x", "DATABASE_PASSWORD=p"])[1],
            "DATABASE_PASSWORD=[redacted]"
        );
    }

    #[test]
    fn headers_urls_and_token_shapes() {
        assert_eq!(
            r(&["curl", "-H", "Authorization: Bearer abc.def"]),
            ["curl", "-H", "Authorization: [redacted]"]
        );
        assert_eq!(
            r(&["git", "clone", "https://bob:s3cret@github.com/x/y.git"]),
            ["git", "clone", "https://bob:[redacted]@github.com/x/y.git"]
        );
        assert_eq!(
            r(&["psql", "--dsn=postgres://u:pw@db:5432/x"])[1],
            "--dsn=[redacted]"
        );
        assert_eq!(
            r(&["x", "DB=postgres://u:pw@db/x"])[1],
            "DB=postgres://u:[redacted]@db/x"
        );
        assert_eq!(r(&["gh", "ghp_ABCDEFGHIJKLMNOPQRSTUVWX"])[1], REDACTED);
        assert_eq!(r(&["x", "sk-proj-abcdefghijklmnopqrstuv"])[1], REDACTED);
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk";
        assert_eq!(r(&["x", jwt])[1], REDACTED);
    }

    #[test]
    fn ordinary_command_lines_are_untouched() {
        let v = [
            "cargo",
            "build",
            "--release",
            "--manifest-path",
            "/a/Cargo.toml",
            "-j",
            "8",
            "https://example.com/path",
            "--keyboard-layout=us",
        ];
        assert_eq!(r(&v), v);
        assert_eq!(
            redact_command_line("node server.js --port 3000"),
            "node server.js --port 3000"
        );
        assert_eq!(
            redact_command_line("app --secret abc --x"),
            "app --secret [redacted] --x"
        );
    }

    /// Every case from the review (R3), the cases that already worked, and
    /// non-secrets that must stay as they are. Command-line strings.
    #[test]
    fn command_line_table() {
        let cases: &[(&str, &str)] = &[
            // Headers split by whitespace, quoted or not.
            (
                "curl -H 'Authorization: Bearer abc123' https://x",
                "curl -H 'Authorization: [redacted]' https://x",
            ),
            (
                "curl -H \"Authorization: Bearer abc123\" https://x",
                "curl -H \"Authorization: [redacted]\" https://x",
            ),
            (
                "curl -H Authorization: Bearer abc123 https://x",
                "curl -H Authorization: [redacted] https://x",
            ),
            (
                "curl -H x-api-key: v4lue https://x",
                "curl -H x-api-key: [redacted] https://x",
            ),
            (
                "curl -H 'X-Api-Key: v4lue'",
                "curl -H 'X-Api-Key: [redacted]'",
            ),
            (
                "curl --header=Authorization:Bearer.abc https://x",
                "curl --header=Authorization: [redacted] https://x",
            ),
            (
                "curl \"--header=Authorization: Bearer abc\"",
                "curl \"--header=Authorization: [redacted]\"",
            ),
            (
                "curl -H 'Cookie: sid=1; x=2' u",
                "curl -H 'Cookie: [redacted]' u",
            ),
            // Per-program short flags.
            (
                "curl -u user:pass https://x",
                "curl -u [redacted] https://x",
            ),
            ("curl -uuser:pass https://x", "curl -u[redacted] https://x"),
            (
                "curl --user user:pass https://x",
                "curl --user [redacted] https://x",
            ),
            (
                "mysql -u root -p hunter2 db",
                "mysql -u root -p [redacted] db",
            ),
            (
                "mysql -u root -phunter2 db",
                "mysql -u root -p[redacted] db",
            ),
            ("sudo mysql -phunter2", "sudo mysql -p[redacted]"),
            (
                "/usr/local/bin/mysqldump -phunter2 db",
                "/usr/local/bin/mysqldump -p[redacted] db",
            ),
            (
                "docker login -u bob -p hunter2 reg",
                "docker login -u bob -p [redacted] reg",
            ),
            ("redis-cli -a hunter2 ping", "redis-cli -a [redacted] ping"),
            (
                "docker exec c redis-cli -a hunter2",
                "docker exec c redis-cli -a [redacted]",
            ),
            ("mongosh -u a -p hunter2", "mongosh -u a -p [redacted]"),
            // URL user info.
            (
                "git clone https://ghp_ABCDEFGHIJKLMNOPQRSTUVWX@github.com/a/b",
                "git clone https://[redacted]@github.com/a/b",
            ),
            (
                "git clone https://0123456789abcdef0123456789abcdef01234567@github.com/a/b",
                "git clone https://[redacted]@github.com/a/b",
            ),
            (
                "git clone https://x-access-token:abc@github.com/a/b",
                "git clone https://x-access-token:[redacted]@github.com/a/b",
            ),
            // Token and JWT shapes after `=`.
            (
                "tool --jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk",
                "tool --jwt=[redacted]",
            ),
            (
                "tool --gh=ghp_ABCDEFGHIJKLMNOPQRSTUVWX",
                "tool --gh=[redacted]",
            ),
            // Already worked.
            ("app --secret abc --x", "app --secret [redacted] --x"),
            ("app --token=abc", "app --token=[redacted]"),
            (
                "env GITHUB_TOKEN=x node",
                "env GITHUB_TOKEN=[redacted] node",
            ),
            ("x sk-proj-abcdefghijklmnopqrstuv", "x [redacted]"),
            (
                "git clone https://bob:pw@h/x",
                "git clone https://bob:[redacted]@h/x",
            ),
            // Not secrets: must stay.
            ("pip install sk-learn-thing", "pip install sk-learn-thing"),
            ("docker run -p 8080:80 nginx", "docker run -p 8080:80 nginx"),
            ("mysql -P 3306 -h db", "mysql -P 3306 -h db"),
            (
                "curl -o out.html https://example.com/a",
                "curl -o out.html https://example.com/a",
            ),
            (
                "git clone https://git@github.com/a/b",
                "git clone https://git@github.com/a/b",
            ),
            ("gh auth token", "gh auth token"),
            ("npm token list", "npm token list"),
            ("node server.js --port 3000", "node server.js --port 3000"),
            ("grep -r authorization src", "grep -r authorization src"),
            (
                "cargo build --keyboard-layout=us",
                "cargo build --keyboard-layout=us",
            ),
            ("tar -xpf a.tar", "tar -xpf a.tar"),
        ];
        for (input, want) in cases {
            assert_eq!(redact_command_line(input), *want, "input: {input}");
        }
    }

    #[test]
    fn argv_table() {
        assert_eq!(
            r(&["curl", "-H", "x-api-key: v4lue"]),
            ["curl", "-H", "x-api-key: [redacted]"]
        );
        assert_eq!(
            r(&["curl", "--header=Authorization: Bearer abc"]),
            ["curl", "--header=Authorization: [redacted]"]
        );
        assert_eq!(r(&["curl", "-u", "a:b"]), ["curl", "-u", "[redacted]"]);
        assert_eq!(r(&["mysql", "-pX"]), ["mysql", "-p[redacted]"]);
        assert_eq!(
            r(&["docker", "login", "-p", "X"]),
            ["docker", "login", "-p", "[redacted]"]
        );
        assert_eq!(
            r(&["docker", "run", "-p", "80:80"]),
            ["docker", "run", "-p", "80:80"]
        );
        assert_eq!(
            r(&["redis-cli", "-a", "X"]),
            ["redis-cli", "-a", "[redacted]"]
        );
        assert_eq!(r(&["x", "Bearer", "abc"]), ["x", "Bearer", "[redacted]"]);
        assert_eq!(
            r(&["pip", "install", "sk-learn-thing"]),
            ["pip", "install", "sk-learn-thing"]
        );
    }

    #[test]
    fn lease_labels_keep_only_plain_words() {
        assert_eq!(redact_label("mulchie: cargo build"), "mulchie: cargo build");
        assert_eq!(
            redact_label("open-source-plan-7e9557: npm test"),
            "open-source-plan-7e9557: npm test"
        );
        assert_eq!(
            redact_label("w: curl -H 'Authorization: Bearer abc'"),
            "w: curl -H [redacted] [redacted]"
        );
        assert_eq!(
            redact_label("w: tool --token abc"),
            "w: tool --token [redacted]"
        );
        assert_eq!(
            redact_label("w: deploy 0123456789abcdef0123456789abcdef"),
            "w: deploy [redacted]"
        );
        assert_eq!(
            redact_label("w: x $(cat ~/.secret)"),
            "w: x [redacted] [redacted]"
        );
        assert!(redact_label(&"a ".repeat(100)).split(' ').count() <= 8);
        assert!(redact_label(&"é".repeat(500)).chars().count() <= 120);
        assert_eq!(
            redact_label("Mon Projet: cargo build"),
            "Mon Projet: cargo build"
        );
    }

    #[test]
    fn hostile_input_never_panics() {
        for s in [
            "",
            "authorization:",
            "'authorization:",
            "\"Authorization: Bearer",
            "Authorization: Bearer",
            "é=é",
            "-p",
            "mysql -p",
            "https://@",
            "a=b=c=d=e=f=g=h",
            "x-api-key:é",
        ] {
            let _ = redact_command_line(s);
            let _ = redact_label(s);
            let _ = redact_args(&[s.to_string(), s.to_string()]);
        }
    }
}
