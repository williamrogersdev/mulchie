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
//! It is a best effort: a secret passed positionally with no telling flag
//! (`mysql -pSECRET`, `tool SECRET`) is not recognised.

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

/// Hide the password in `scheme://user:password@host…`.
fn redact_url(arg: &str) -> Option<String> {
    let i = arg.find("://")?;
    let rest = &arg[i + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let at = authority.rfind('@')?;
    let userinfo = &authority[..at];
    let (user, _) = userinfo.split_once(':')?;
    Some(format!(
        "{}{user}:{REDACTED}@{}",
        &arg[..i + 3],
        &rest[at + 1..]
    ))
}

/// One argument on its own (no look at its neighbours).
fn redact_one(arg: &str) -> Option<String> {
    if looks_like_token(arg) {
        return Some(REDACTED.into());
    }
    let lower = arg.to_ascii_lowercase();
    if lower.starts_with("authorization:")
        || lower.starts_with("bearer ")
        || lower.starts_with("x-api-key:")
        || lower.starts_with("cookie:")
    {
        let cut = arg.find([':', ' ']).unwrap_or(arg.len());
        return Some(format!("{} {REDACTED}", arg[..cut + 1].trim_end()));
    }
    if let Some((k, v)) = arg.split_once('=') {
        if !v.is_empty() && !k.contains('/') && is_secret_name(k) {
            return Some(format!("{k}={REDACTED}"));
        }
        if let Some(u) = redact_url(v) {
            return Some(format!("{k}={u}"));
        }
    }
    redact_url(arg)
}

/// Redact secrets in an argument vector.
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for (i, a) in args.iter().enumerate() {
        if hide_next {
            hide_next = false;
            if !a.starts_with('-') || a.len() == 1 {
                out.push(REDACTED.to_string());
                continue;
            }
        }
        if let Some(r) = redact_one(a) {
            out.push(r);
            continue;
        }
        // `--token VALUE` / `-p VALUE` style: a flag naming a secret without
        // `=` hides the next argument. Not for argv[0].
        let is_flag = i > 0 && a.starts_with('-') && !a.contains('=') && a.len() > 2;
        if is_flag && is_secret_name(a) {
            hide_next = true;
        }
        // `-H 'Authorization: …'` is handled by redact_one on the value.
        out.push(a.clone());
    }
    out
}

/// Redact a command line given as one string (split on whitespace).
pub fn redact_command_line(cmd: &str) -> String {
    let parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
    redact_args(&parts).join(" ")
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
}
