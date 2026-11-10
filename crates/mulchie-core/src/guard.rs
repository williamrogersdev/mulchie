//! Guard rules: a pure check of one shell command an agent is about to run.
//!
//! This is a **tripwire for honest agents, not a sandbox**. It reads the
//! command text the agent's own hook hands over and matches a few risky
//! shapes: deleting or moving outside the workspace, piping a download into a
//! shell, `sudo`, force-pushing a protected branch, disabling Mulchie, reading
//! credential stores, and writing shell startup files or launch agents. An
//! agent that wants to evade it can (a script file, an alias, base64, a
//! variable holding the path); nothing here stops a process that does not ask.
//!
//! The parser is conservative: when it cannot tell what a word means (an
//! unknown `$VAR`, a command substitution used as a path, an unbalanced
//! quote), the rule does **not** match. A false "block" stalls honest work;
//! a missed match is what the tripwire design accepts.
//!
//! Handled shell syntax: `;`, `&&`, `||`, `|`, `&`, newlines, single and
//! double quotes, backslash escapes, `$(…)` and backticks (parsed as nested
//! scripts), `<(…)` / `>(…)`, `sh -c "…"` (and bash, zsh, dash, ksh, fish),
//! `VAR=x` prefixes, `env`, `command`, `exec`, `nohup`, `time`, `nice`,
//! `timeout`, `sudo`/`doas` (flagged, then the command they run is checked),
//! redirections (`>`, `>>`, `<`, `2>`, `&>`, `>|`, here-docs skipped), `~` and
//! `$HOME`, comments.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::config::{GuardConfig, RuleMode};

/// Commands longer than this are checked on their first this-many bytes.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// Nested scripts (`$(…)`, `sh -c`) deeper than this are not looked into.
const MAX_DEPTH: usize = 4;
/// At most this many simple commands per check.
const MAX_COMMANDS: usize = 2_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum GuardRule {
    RecursiveDelete,
    PipeToShell,
    Sudo,
    ForcePush,
    DisableMulchie,
    CredentialRead,
    StartupWrite,
    /// The command was longer than [`MAX_COMMAND_BYTES`] and only its start
    /// was checked. Always a warning; not configurable (not in `ALL`).
    TooLong,
}

impl GuardRule {
    pub const ALL: [GuardRule; 7] = [
        GuardRule::RecursiveDelete,
        GuardRule::PipeToShell,
        GuardRule::Sudo,
        GuardRule::ForcePush,
        GuardRule::DisableMulchie,
        GuardRule::CredentialRead,
        GuardRule::StartupWrite,
    ];

    pub fn code(self) -> &'static str {
        match self {
            GuardRule::RecursiveDelete => "recursive_delete",
            GuardRule::PipeToShell => "pipe_to_shell",
            GuardRule::Sudo => "sudo",
            GuardRule::ForcePush => "force_push",
            GuardRule::DisableMulchie => "disable_mulchie",
            GuardRule::CredentialRead => "credential_read",
            GuardRule::StartupWrite => "startup_write",
            GuardRule::TooLong => "too_long",
        }
    }

    fn mode(self, cfg: &GuardConfig) -> RuleMode {
        let r = &cfg.rules;
        match self {
            GuardRule::RecursiveDelete => r.recursive_delete,
            GuardRule::PipeToShell => r.pipe_to_shell,
            GuardRule::Sudo => r.sudo,
            GuardRule::ForcePush => r.force_push,
            GuardRule::DisableMulchie => r.disable_mulchie,
            GuardRule::CredentialRead => r.credential_read,
            GuardRule::StartupWrite => r.startup_write,
            GuardRule::TooLong => RuleMode::Warn,
        }
    }
}

/// The answer for the agent's client.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    #[default]
    Allow,
    Warn,
    Block,
}

/// One rule that matched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleMatch {
    pub rule: GuardRule,
    /// The rule's configured mode (never `off`: those are not reported).
    pub mode: RuleMode,
    /// One sentence about what matched.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Verdict {
    pub decision: Decision,
    /// The deciding rule (the strictest match), when one matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<GuardRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Every rule that matched, strictest first.
    pub matches: Vec<RuleMatch>,
}

/// What a check needs besides the command.
#[derive(Debug, Clone)]
pub struct GuardInput<'a> {
    pub command: &'a str,
    /// The working directory the command runs in.
    pub cwd: &'a Path,
    /// The session's workspace (git checkout), when known.
    pub workspace: Option<&'a Path>,
    pub home: &'a Path,
    /// Mulchie's config directory.
    pub config_dir: &'a Path,
    /// The branch checked out in the workspace, when known.
    pub current_branch: Option<&'a str>,
    /// Pids of the running Mulchie engine (for `kill <pid>`).
    pub mulchie_pids: &'a [u32],
}

/// Check one command. Pure: reads nothing but its input.
pub fn check(input: &GuardInput, cfg: &GuardConfig) -> Verdict {
    if !cfg.enabled {
        return Verdict::default();
    }
    let text = truncate_utf8(input.command, MAX_COMMAND_BYTES);
    let script = parse_script(text, input.home, 0);
    let mut found: Vec<(GuardRule, String)> = Vec::new();
    if input.command.len() > MAX_COMMAND_BYTES {
        found.push((
            GuardRule::TooLong,
            format!(
                "the command is longer than {} KiB; only its first {} KiB were checked",
                MAX_COMMAND_BYTES / 1024,
                MAX_COMMAND_BYTES / 1024
            ),
        ));
    }
    let mut ctx = Ctx {
        input,
        cfg,
        found: &mut found,
        cwd: Some(normalize(input.cwd)),
        downloaded: Vec::new(),
    };
    ctx.script(&script, 0);
    let mut matches: Vec<RuleMatch> = Vec::new();
    for (rule, reason) in found {
        let mode = rule.mode(cfg);
        if mode == RuleMode::Off || matches.iter().any(|m| m.rule == rule) {
            continue;
        }
        matches.push(RuleMatch { rule, mode, reason });
    }
    matches.sort_by(|a, b| b.mode.cmp(&a.mode).then(a.rule.cmp(&b.rule)));
    let Some(top) = matches.first() else {
        return Verdict::default();
    };
    Verdict {
        decision: match top.mode {
            RuleMode::Block => Decision::Block,
            _ => Decision::Warn,
        },
        rule: Some(top.rule),
        reason: Some(top.reason.clone()),
        matches,
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ------------------------------------------------------------------- lexer

/// One shell word after quote removal.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    /// The text with quotes removed, `~` and `$HOME` expanded. Substitutions
    /// appear as `$(…)`.
    pub text: String,
    /// Contains something whose value is unknown here: another variable, a
    /// command substitution, a glob outside quotes.
    pub dynamic: bool,
    /// Has an unquoted glob character (`*`, `?`, `[`).
    pub glob: bool,
    /// Nested scripts: `$(…)`, backticks, `<(…)`, `>(…)`.
    pub subs: Vec<Script>,
    /// The word is a process substitution `<(…)` / `>(…)`.
    pub procsub: bool,
}

/// A redirection: `op` is `>`, `>>`, `<`, `>|`, `&>`, `&>>`, `<<<`.
#[derive(Debug, Clone, PartialEq)]
pub struct Redir {
    pub op: String,
    pub target: Word,
}

/// A simple command and where it sits in its pipeline.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cmd {
    pub words: Vec<Word>,
    pub redirs: Vec<Redir>,
    /// Pipelines are numbered from 0 within a script.
    pub pipeline: usize,
    /// Position within the pipeline (0 = first).
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Script {
    pub cmds: Vec<Cmd>,
}

struct Lexer<'a> {
    chars: Vec<char>,
    i: usize,
    home: &'a Path,
    depth: usize,
}

/// Parse a script. Never fails: unbalanced input ends where the text ends.
pub fn parse_script(text: &str, home: &Path, depth: usize) -> Script {
    let mut lx = Lexer {
        chars: text.chars().collect(),
        i: 0,
        home,
        depth,
    };
    lx.script()
}

fn is_meta(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}

impl Lexer<'_> {
    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    fn sub(&self, inner: &str) -> Script {
        if self.depth + 1 > MAX_DEPTH {
            return Script::default();
        }
        parse_script(inner, self.home, self.depth + 1)
    }

    fn script(&mut self) -> Script {
        let mut s = Script::default();
        let mut cur = Cmd::default();
        let mut pipeline = 0usize;
        let mut pos = 0usize;
        let mut heredocs: Vec<(String, bool)> = Vec::new();
        let finish = |s: &mut Script, cur: &mut Cmd, pipeline: usize, pos: usize| {
            if !cur.words.is_empty() || !cur.redirs.is_empty() {
                let mut c = std::mem::take(cur);
                c.pipeline = pipeline;
                c.pos = pos;
                if s.cmds.len() < MAX_COMMANDS {
                    s.cmds.push(c);
                }
            }
        };
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' => self.i += 1,
                '\\' if self.peek(1) == Some('\n') => self.i += 2,
                '\n' => {
                    self.i += 1;
                    finish(&mut s, &mut cur, pipeline, pos);
                    pipeline += 1;
                    pos = 0;
                    // Here-document bodies are data, not commands.
                    for (delim, strip) in std::mem::take(&mut heredocs) {
                        self.skip_heredoc(&delim, strip);
                    }
                }
                '#' => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                ';' | '(' | ')' | '{' | '}' if c != '{' && c != '}' || cur.words.is_empty() => {
                    if (c == '{' || c == '}')
                        && self.peek(1).is_some_and(|n| !is_meta(n) && n != '\n')
                    {
                        // `{foo}` is a word, not a group.
                        let w = self.word();
                        cur.words.push(w);
                        continue;
                    }
                    self.i += 1;
                    if c == ';' && self.peek(0) == Some(';') {
                        self.i += 1;
                    }
                    finish(&mut s, &mut cur, pipeline, pos);
                    pipeline += 1;
                    pos = 0;
                }
                '&' if self.peek(1) == Some('&') => {
                    self.i += 2;
                    finish(&mut s, &mut cur, pipeline, pos);
                    pipeline += 1;
                    pos = 0;
                }
                '&' if self.peek(1) == Some('>') => {
                    self.i += 2;
                    let op = if self.peek(0) == Some('>') {
                        self.i += 1;
                        "&>>"
                    } else {
                        "&>"
                    };
                    self.skip_blanks();
                    let t = self.word();
                    cur.redirs.push(Redir {
                        op: op.into(),
                        target: t,
                    });
                }
                '&' => {
                    self.i += 1;
                    finish(&mut s, &mut cur, pipeline, pos);
                    pipeline += 1;
                    pos = 0;
                }
                '|' if self.peek(1) == Some('|') => {
                    self.i += 2;
                    finish(&mut s, &mut cur, pipeline, pos);
                    pipeline += 1;
                    pos = 0;
                }
                '|' => {
                    self.i += 1;
                    if self.peek(0) == Some('&') {
                        self.i += 1;
                    }
                    finish(&mut s, &mut cur, pipeline, pos);
                    pos += 1;
                }
                '<' | '>' if self.peek(1) == Some('(') => {
                    let w = self.word();
                    cur.words.push(w);
                }
                '<' | '>' => {
                    if let Some(r) = self.redir(&mut heredocs) {
                        cur.redirs.push(r);
                    }
                }
                _ => {
                    // `2>file`, `2>&1`: a leading fd number belongs to the redirection.
                    if c.is_ascii_digit() {
                        let mut k = 0;
                        while self.peek(k).is_some_and(|d| d.is_ascii_digit()) {
                            k += 1;
                        }
                        if matches!(self.peek(k), Some('>' | '<')) && self.peek(k + 1) != Some('(')
                        {
                            self.i += k;
                            if let Some(r) = self.redir(&mut heredocs) {
                                cur.redirs.push(r);
                            }
                            continue;
                        }
                    }
                    let w = self.word();
                    cur.words.push(w);
                }
            }
        }
        finish(&mut s, &mut cur, pipeline, pos);
        s
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.i += 1;
        }
    }

    fn skip_heredoc(&mut self, delim: &str, strip_tabs: bool) {
        while self.i < self.chars.len() {
            let start = self.i;
            while self.peek(0).is_some_and(|c| c != '\n') {
                self.i += 1;
            }
            let line: String = self.chars[start..self.i].iter().collect();
            if self.peek(0) == Some('\n') {
                self.i += 1;
            }
            let l = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if l == delim {
                return;
            }
        }
    }

    /// At `<` or `>`: one redirection. File-descriptor duplications (`>&2`)
    /// give `None`.
    fn redir(&mut self, heredocs: &mut Vec<(String, bool)>) -> Option<Redir> {
        let c = self.peek(0)?;
        self.i += 1;
        let mut op = c.to_string();
        if c == '<' && self.peek(0) == Some('<') {
            self.i += 1;
            if self.peek(0) == Some('<') {
                self.i += 1;
                self.skip_blanks();
                let w = self.word();
                return Some(Redir {
                    op: "<<<".into(),
                    target: w,
                });
            }
            let strip = self.peek(0) == Some('-');
            if strip {
                self.i += 1;
            }
            self.skip_blanks();
            let w = self.word();
            heredocs.push((w.text, strip));
            return None;
        }
        if c == '>' && matches!(self.peek(0), Some('>' | '|')) {
            op.push(self.peek(0)?);
            self.i += 1;
        }
        if self.peek(0) == Some('&') {
            // `>&2`, `<&0`, `>&-`: a descriptor, not a file.
            self.i += 1;
            self.skip_blanks();
            let _ = self.word();
            return None;
        }
        self.skip_blanks();
        let target = self.word();
        Some(Redir { op, target })
    }

    /// Text up to the matching `)` (the opening one is already consumed).
    fn balanced(&mut self) -> String {
        let start = self.i;
        let mut depth = 1usize;
        while let Some(c) = self.peek(0) {
            match c {
                '\\' => self.i += 1,
                '\'' => {
                    self.i += 1;
                    while self.peek(0).is_some_and(|c| c != '\'') {
                        self.i += 1;
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        if d == '\\' {
                            self.i += 1;
                        } else if d == '"' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let inner: String = self.chars[start..self.i].iter().collect();
                        self.i += 1;
                        return inner;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        self.chars[start.min(self.chars.len())..].iter().collect()
    }

    fn backtick(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek(0) {
            self.i += 1;
            match c {
                '\\' => {
                    if let Some(n) = self.peek(0) {
                        out.push(n);
                        self.i += 1;
                    }
                }
                '`' => return out,
                _ => out.push(c),
            }
        }
        out
    }

    /// `$…` at the cursor (the `$` not yet consumed), appended to `w`.
    fn dollar(&mut self, w: &mut Word) {
        self.i += 1;
        match self.peek(0) {
            Some('(') if self.peek(1) == Some('(') => {
                // Arithmetic: unknown value, nothing to run.
                self.i += 2;
                let _ = self.balanced();
                if self.peek(0) == Some(')') {
                    self.i += 1;
                }
                w.text.push_str("$((…))");
                w.dynamic = true;
            }
            Some('(') => {
                self.i += 1;
                let inner = self.balanced();
                w.subs.push(self.sub(&inner));
                w.text.push_str("$(…)");
                w.dynamic = true;
            }
            Some('{') => {
                self.i += 1;
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '}') {
                    self.i += 1;
                }
                let name: String = self.chars[start..self.i].iter().collect();
                self.i += 1;
                if name == "HOME" {
                    w.text.push_str(&self.home.to_string_lossy());
                } else {
                    w.text.push_str(&format!("${{{name}}}"));
                    w.dynamic = true;
                }
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let start = self.i;
                while self
                    .peek(0)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    self.i += 1;
                }
                let name: String = self.chars[start..self.i].iter().collect();
                if name == "HOME" {
                    w.text.push_str(&self.home.to_string_lossy());
                } else {
                    w.text.push('$');
                    w.text.push_str(&name);
                    w.dynamic = true;
                }
            }
            Some(c) if c.is_ascii_digit() || "@*#?$!-".contains(c) => {
                self.i += 1;
                w.text.push('$');
                w.text.push(c);
                w.dynamic = true;
            }
            _ => w.text.push('$'),
        }
    }

    fn word(&mut self) -> Word {
        let mut w = Word::default();
        let start = self.i;
        while let Some(c) = self.peek(0) {
            match c {
                '<' | '>' if self.peek(1) == Some('(') && self.i == start => {
                    self.i += 2;
                    let inner = self.balanced();
                    w.subs.push(self.sub(&inner));
                    w.text.push_str(if c == '<' { "<(…)" } else { ">(…)" });
                    w.procsub = true;
                    w.dynamic = true;
                }
                c if is_meta(c) => break,
                '\\' => {
                    self.i += 1;
                    if let Some(n) = self.peek(0) {
                        if n != '\n' {
                            w.text.push(n);
                        }
                        self.i += 1;
                    }
                }
                '\'' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        self.i += 1;
                        if d == '\'' {
                            break;
                        }
                        w.text.push(d);
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        match d {
                            '"' => {
                                self.i += 1;
                                break;
                            }
                            '\\' => {
                                self.i += 1;
                                if let Some(n) = self.peek(0) {
                                    if !"\"\\$`\n".contains(n) {
                                        w.text.push('\\');
                                    }
                                    if n != '\n' {
                                        w.text.push(n);
                                    }
                                    self.i += 1;
                                }
                            }
                            '$' => self.dollar(&mut w),
                            '`' => {
                                self.i += 1;
                                let inner = self.backtick();
                                w.subs.push(self.sub(&inner));
                                w.text.push_str("$(…)");
                                w.dynamic = true;
                            }
                            _ => {
                                w.text.push(d);
                                self.i += 1;
                            }
                        }
                    }
                }
                '$' => self.dollar(&mut w),
                '`' => {
                    self.i += 1;
                    let inner = self.backtick();
                    w.subs.push(self.sub(&inner));
                    w.text.push_str("$(…)");
                    w.dynamic = true;
                }
                '~' if self.i == start && self.peek(1).is_none_or(|n| n == '/' || is_meta(n)) => {
                    self.i += 1;
                    w.text.push_str(&self.home.to_string_lossy());
                }
                '*' | '?' | '[' => {
                    w.glob = true;
                    w.text.push(c);
                    self.i += 1;
                }
                _ => {
                    w.text.push(c);
                    self.i += 1;
                }
            }
        }
        if self.i == start {
            // A lone metacharacter we could not use: step over it.
            self.i += 1;
        }
        w
    }
}

// ------------------------------------------------------------------- rules

struct Ctx<'a, 'b> {
    input: &'a GuardInput<'a>,
    cfg: &'a GuardConfig,
    found: &'b mut Vec<(GuardRule, String)>,
    /// The working directory as `cd` commands earlier in the line leave it;
    /// `None` after a `cd` to somewhere unknown (relative paths then match
    /// nothing).
    cwd: Option<PathBuf>,
    /// Files an earlier command in the line downloaded (`curl -o f`).
    downloaded: Vec<PathBuf>,
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "mksh", "ash"];
const INTERPRETERS: &[&str] = &[
    "python", "python3", "python2", "perl", "ruby", "node", "bun", "deno", "php",
];
const DOWNLOADERS: &[&str] = &["curl", "wget", "fetch", "http", "https", "xh", "aria2c"];

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// A command with wrappers and assignments removed: (program, args).
#[derive(Debug, Clone)]
struct Simple<'a> {
    prog: String,
    /// The program word as written (`./install.sh`).
    word: &'a Word,
    args: Vec<&'a Word>,
    sudo: bool,
}

fn opt_takes_value(prog: &str, opt: &str) -> bool {
    match prog {
        "sudo" => [
            "-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U", "-T", "-R",
        ]
        .contains(&opt),
        "doas" => ["-u", "-C"].contains(&opt),
        "env" => ["-u", "-C", "-S", "--unset", "--chdir"].contains(&opt),
        "nice" => ["-n", "--adjustment"].contains(&opt),
        "timeout" => ["-s", "-k", "--signal", "--kill-after"].contains(&opt),
        "time" => ["-o", "-f"].contains(&opt),
        _ => false,
    }
}

/// Strip `VAR=x`, `env`, `command`, `exec`, `nohup`, `time`, `nice`,
/// `timeout`, `sudo`, `doas` from the front.
fn simplify(words: &[Word]) -> Option<Simple<'_>> {
    let mut i = 0;
    let mut sudo = false;
    loop {
        let w = words.get(i)?;
        let t = w.text.as_str();
        if !w.dynamic && is_assignment(t) {
            i += 1;
            continue;
        }
        let b = base(t);
        match b {
            "env" | "command" | "builtin" | "exec" | "nohup" | "time" | "nice" | "timeout"
            | "sudo" | "doas" | "pkexec" | "run0" | "caffeinate" | "stdbuf" => {
                if matches!(b, "sudo" | "doas" | "pkexec" | "run0") {
                    sudo = true;
                }
                i += 1;
                while let Some(o) = words.get(i) {
                    let ot = o.text.as_str();
                    if ot == "--" {
                        i += 1;
                        break;
                    }
                    if b == "env" && is_assignment(ot) {
                        i += 1;
                        continue;
                    }
                    if ot.starts_with('-') && ot.len() > 1 {
                        i += if opt_takes_value(b, ot) { 2 } else { 1 };
                        continue;
                    }
                    if b == "timeout" {
                        // The duration.
                        i += 1;
                    }
                    break;
                }
                if b == "timeout" && words.get(i).is_some_and(|w| w.text.starts_with('-')) {
                    i += 1;
                }
            }
            _ => {
                return Some(Simple {
                    prog: b.to_string(),
                    word: w,
                    args: words[i + 1..].iter().collect(),
                    sudo,
                });
            }
        }
    }
}

fn is_assignment(t: &str) -> bool {
    let Some((name, _)) = t.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Lexically normalized absolute path (no filesystem access).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

fn is_option(w: &Word) -> bool {
    w.text.starts_with('-') && w.text.len() > 1 && !w.dynamic
}

const TEMP_DIRS: &[&str] = &[
    "/tmp",
    "/private/tmp",
    "/var/tmp",
    "/private/var/tmp",
    "/var/folders",
    "/private/var/folders",
    "/dev/shm",
];

impl Ctx<'_, '_> {
    fn hit(&mut self, rule: GuardRule, reason: String) {
        self.found.push((rule, reason));
    }

    fn home(&self) -> &Path {
        self.input.home
    }

    /// The absolute path a word names, when it can be known.
    fn path_of(&self, w: &Word) -> Option<PathBuf> {
        if w.dynamic || w.glob || w.text.is_empty() {
            return None;
        }
        self.try_resolve(&w.text)
    }

    fn try_resolve(&self, t: &str) -> Option<PathBuf> {
        let p = Path::new(t);
        if p.is_absolute() {
            Some(normalize(p))
        } else {
            self.cwd.as_ref().map(|c| normalize(&c.join(p)))
        }
    }

    /// Like [`Ctx::try_resolve`]; an unknown working directory gives a path
    /// no rule matches.
    fn resolve(&self, t: &str) -> PathBuf {
        self.try_resolve(t)
            .unwrap_or_else(|| PathBuf::from("/nonexistent-unknown-cwd/x/y"))
    }

    fn script(&mut self, s: &Script, depth: usize) {
        for c in &s.cmds {
            self.cmd(c, s, depth);
            for w in c.words.iter().chain(c.redirs.iter().map(|r| &r.target)) {
                for sub in &w.subs {
                    self.script(sub, depth + 1);
                }
            }
        }
    }

    fn cmd(&mut self, c: &Cmd, s: &Script, depth: usize) {
        // Redirections apply whatever the command is.
        for r in &c.redirs {
            if r.op == "<" {
                if let Some(p) = self.path_of(&r.target) {
                    self.credential(&p, "input redirection");
                }
            } else if r.op.starts_with('>') || r.op.starts_with("&>") {
                if let Some(p) = self.path_of(&r.target) {
                    self.write_target(&p, &format!("redirection {}", r.op));
                }
            }
        }
        let Some(sc) = simplify(&c.words) else {
            return;
        };
        if sc.prog == "cd" || sc.prog == "pushd" {
            let target = sc.args.iter().find(|w| !is_option(w));
            self.cwd = match target {
                None => Some(normalize(self.home())),
                Some(w) if w.dynamic || w.glob || w.text == "-" => None,
                Some(w) => self.try_resolve(&w.text),
            };
            return;
        }
        if sc.sudo {
            self.hit(
                GuardRule::Sudo,
                "runs a command with elevated privileges (sudo/doas)".into(),
            );
        }
        let prog = sc.prog.as_str();
        let args = &sc.args;
        self.runs_download(&sc);
        if DOWNLOADERS.contains(&prog) {
            self.note_download(prog, args, c);
        }
        match prog {
            "rm" => self.rm(args),
            "rsync" => self.rsync(args),
            "xargs" => self.xargs(args, c, s),
            "mv" => self.mv(args),
            "find" => self.find(args),
            "git" => self.git(args),
            "kill" | "pkill" | "killall" => self.kill(prog, args),
            "launchctl" => self.launchctl(args),
            "systemctl" => self.systemctl(args),
            "mulchie" => self.mulchie_cmd(args),
            "brew" => {
                let a: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
                let stops = (a.first() == Some(&"services")
                    && a.get(1).is_some_and(|s| ["stop", "kill"].contains(s)))
                    || a.first()
                        .is_some_and(|s| ["uninstall", "remove", "rm"].contains(s));
                if stops && a.iter().any(|x| x.to_ascii_lowercase().contains("mulchie")) {
                    self.hit(
                        GuardRule::DisableMulchie,
                        "stops or uninstalls Mulchie through Homebrew".into(),
                    );
                }
            }
            "security" => self.security(args),
            "secret-tool" => {
                if args.first().is_some_and(|w| w.text == "lookup") {
                    self.hit(
                        GuardRule::CredentialRead,
                        "reads a secret from the system keyring (secret-tool lookup)".into(),
                    );
                }
            }
            "crontab" => {
                let installs = args.iter().any(|w| !is_option(w) || w.text == "-");
                let lists = args
                    .iter()
                    .any(|w| ["-l", "-r", "-e"].contains(&w.text.as_str()));
                if installs && !lists {
                    self.hit(
                        GuardRule::StartupWrite,
                        "replaces the crontab (runs commands on a schedule)".into(),
                    );
                }
            }
            _ => {}
        }
        if SHELLS.contains(&prog) || ["source", ".", "eval"].contains(&prog) {
            self.shell(prog, args, c, s, depth);
        }
        if INTERPRETERS.contains(&prog) {
            self.interpreter_stdin(prog, args, c, s);
        }
        self.reads(prog, args);
        self.writes(prog, args);
        self.mulchie_files(prog, args);
    }

    // ------------------------------------------------------ delete / move

    fn danger(&self, p: &Path) -> Option<String> {
        let home = normalize(self.home());
        if p == Path::new("/") {
            return Some("the root directory".into());
        }
        if p == home {
            return Some("your home directory".into());
        }
        if home.starts_with(p) {
            return Some(format!("{}, which holds your home directory", p.display()));
        }
        if p.components().count() <= 2 {
            return Some(format!("the top-level directory {}", p.display()));
        }
        if TEMP_DIRS.iter().any(|t| p.starts_with(t)) {
            return None;
        }
        let (ws, what) = self.boundary();
        if p == ws {
            return Some(format!("the whole {what} {}", ws.display()));
        }
        if !p.starts_with(&ws) {
            return Some(format!(
                "{}, outside the {what} {}",
                p.display(),
                ws.display()
            ));
        }
        None
    }

    /// What deletes must stay inside: the session's workspace, or with none
    /// known the working directory the command was given.
    fn boundary(&self) -> (PathBuf, &'static str) {
        match self.input.workspace {
            Some(ws) => (normalize(ws), "session's workspace"),
            None => (normalize(self.input.cwd), "working directory"),
        }
    }

    /// Like [`Ctx::danger`], but the boundary itself is fine: for tools that
    /// delete inside a target (`rsync --delete`, `git clean`).
    fn danger_outside(&self, p: &Path) -> Option<String> {
        if p.starts_with(self.boundary().0) && p != Path::new("/") {
            return None;
        }
        self.danger(p)
    }

    /// A target word: its path, or for `dir/*` the directory when that is
    /// dangerous on its own (the contents of home or `/`).
    fn target_danger(&self, w: &Word) -> Option<String> {
        if w.dynamic || w.text.is_empty() {
            return None;
        }
        if !w.glob {
            return self.danger(&self.try_resolve(&w.text)?);
        }
        // `~/*`, `/*`: everything in a dangerous directory.
        let t = w.text.as_str();
        let dir = t.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        if dir.contains(['*', '?', '[']) {
            return None;
        }
        let d = self.try_resolve(if dir.is_empty() { "/" } else { dir })?;
        let home = normalize(self.home());
        if d == Path::new("/") || d == home || home.starts_with(&d) {
            return Some(format!("everything in {}", d.display()));
        }
        if TEMP_DIRS.iter().any(|x| d.starts_with(x)) {
            return None;
        }
        let (ws, what) = self.boundary();
        (!d.starts_with(&ws)).then(|| {
            format!(
                "files in {}, outside the {what} {}",
                d.display(),
                ws.display()
            )
        })
    }

    fn rm(&mut self, args: &[&Word]) {
        let mut recursive = false;
        let mut targets = Vec::new();
        let mut opts_done = false;
        for w in args {
            if !opts_done && w.text == "--" {
                opts_done = true;
            } else if !opts_done && is_option(w) {
                let t = &w.text;
                if t == "--recursive" || (!t.starts_with("--") && t[1..].contains(['r', 'R'])) {
                    recursive = true;
                }
            } else {
                targets.push(*w);
            }
        }
        for w in &targets {
            if let Some(p) = self.path_of(w) {
                if self.is_mulchie_path(&p) {
                    self.hit(
                        GuardRule::DisableMulchie,
                        format!("deletes Mulchie's files ({})", p.display()),
                    );
                }
            }
        }
        if !recursive {
            return;
        }
        for w in targets {
            if let Some(why) = self.target_danger(w) {
                self.hit(
                    GuardRule::RecursiveDelete,
                    format!("recursively deletes {why}"),
                );
                return;
            }
        }
    }

    fn mv(&mut self, args: &[&Word]) {
        let mut pos: Vec<&Word> = Vec::new();
        let mut to_dir = false;
        let mut opts_done = false;
        let mut skip = false;
        for w in args {
            if skip {
                skip = false;
                to_dir = true;
                continue;
            }
            if !opts_done && w.text == "--" {
                opts_done = true;
            } else if !opts_done && is_option(w) {
                if w.text == "-t" {
                    skip = true;
                } else if w.text.starts_with("--target-directory") {
                    to_dir = true;
                }
            } else {
                pos.push(w);
            }
        }
        let sources: &[&Word] = if to_dir {
            &pos
        } else if pos.len() >= 2 {
            &pos[..pos.len() - 1]
        } else {
            &[]
        };
        for w in sources {
            if let Some(p) = self.path_of(w) {
                if self.is_mulchie_path(&p) {
                    self.hit(
                        GuardRule::DisableMulchie,
                        format!("moves Mulchie's files ({})", p.display()),
                    );
                }
            }
            if let Some(why) = self.target_danger(w) {
                self.hit(GuardRule::RecursiveDelete, format!("moves {why}"));
                return;
            }
        }
    }

    fn find(&mut self, args: &[&Word]) {
        let deletes = args.iter().any(|w| w.text == "-delete")
            || args.windows(2).any(|p| {
                (p[0].text == "-exec" || p[0].text == "-execdir") && base(&p[1].text) == "rm"
            });
        if !deletes {
            return;
        }
        for w in args
            .iter()
            .take_while(|w| !w.text.starts_with('-') && !w.text.starts_with('(') && w.text != "!")
        {
            let inside = self.path_of(w).is_some_and(|p| {
                self.input
                    .workspace
                    .is_some_and(|ws| p.starts_with(normalize(ws)))
            });
            if inside {
                continue;
            }
            if let Some(why) = self.target_danger(w) {
                self.hit(
                    GuardRule::RecursiveDelete,
                    format!("deletes files under {why}"),
                );
                return;
            }
        }
    }

    // -------------------------------------------------------------- git

    fn protected(&self, name: &str) -> bool {
        let n = name
            .trim_start_matches('+')
            .trim_start_matches("refs/heads/");
        self.cfg.protected_branches.iter().any(|b| b == n)
    }

    fn git(&mut self, args: &[&Word]) {
        // Global options before the subcommand.
        let mut i = 0;
        let mut dir: Option<Option<PathBuf>> = None;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if ["-C", "-c", "--git-dir", "--work-tree", "--namespace"].contains(&t) {
                if t == "-C" || t == "--work-tree" {
                    dir = Some(args.get(i + 1).and_then(|d| self.path_of(d)));
                }
                i += 2;
            } else if t.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
        let Some(sub) = args.get(i).map(|w| w.text.as_str()) else {
            return;
        };
        let rest: Vec<&Word> = args[i + 1..].to_vec();
        let cur = self.input.current_branch;
        if sub == "clean" {
            let dir = match dir {
                Some(d) => d,
                None => self.cwd.clone(),
            };
            self.git_clean(dir, &rest);
            return;
        }
        match sub {
            "push" => {
                let mut force = false;
                let mut delete = false;
                let mut all = false;
                let mut pos: Vec<&Word> = Vec::new();
                let mut skip = false;
                for w in &rest {
                    if skip {
                        skip = false;
                        continue;
                    }
                    let t = w.text.as_str();
                    if t.starts_with("--force") || t == "-f" {
                        force = true;
                    } else if t == "-d" || t == "--delete" {
                        delete = true;
                    } else if t == "--all" || t == "--mirror" || t == "--branches" {
                        all = true;
                    } else if ["-o", "--push-option", "--repo", "--receive-pack", "--exec"]
                        .contains(&t)
                    {
                        skip = true;
                    } else if t.starts_with('-') {
                        if !t.starts_with("--")
                            && t.strip_prefix('-').is_some_and(|r| r.contains('f'))
                        {
                            force = true;
                        }
                    } else {
                        pos.push(w);
                    }
                }
                let refspecs: Vec<&str> = pos.iter().skip(1).map(|w| w.text.as_str()).collect();
                if refspecs.iter().any(|r| r.starts_with('+')) {
                    force = true;
                }
                if refspecs.iter().any(|r| r.starts_with(':')) {
                    delete = true;
                }
                if !force && !delete {
                    return;
                }
                if pos.iter().any(|w| w.dynamic) {
                    return;
                }
                let dests: Vec<String> = refspecs
                    .iter()
                    .map(|r| {
                        let r = r.trim_start_matches('+');
                        match r.split_once(':') {
                            Some((_, d)) => d.to_string(),
                            None if r == "HEAD" => cur.unwrap_or("").to_string(),
                            None => r.to_string(),
                        }
                    })
                    .collect();
                let hit = if refspecs.is_empty() {
                    all || cur.is_some_and(|c| self.protected(c))
                } else {
                    dests.iter().find(|d| self.protected(d)).is_some()
                };
                if hit {
                    let what = if delete { "deletes" } else { "force-pushes" };
                    let name = dests
                        .iter()
                        .find(|d| self.protected(d))
                        .cloned()
                        .or(cur.map(String::from))
                        .unwrap_or_else(|| "every branch".into());
                    self.hit(
                        GuardRule::ForcePush,
                        format!("{what} the protected branch {name}"),
                    );
                }
            }
            "filter-branch" | "filter-repo" => {
                let names_protected = rest.iter().any(|w| self.protected(&w.text))
                    || rest.iter().any(|w| w.text == "--all");
                if names_protected || cur.is_some_and(|c| self.protected(c)) {
                    self.hit(
                        GuardRule::ForcePush,
                        format!("rewrites the history of a protected branch (git {sub})"),
                    );
                }
            }
            "branch" => {
                let forced = rest.iter().any(|w| {
                    let t = w.text.as_str();
                    ["-D", "-M", "-C", "--force", "-f"].contains(&t)
                        || (t == "-d" && rest.iter().any(|x| x.text == "--force"))
                });
                if let Some(n) = rest
                    .iter()
                    .filter(|w| !w.text.starts_with('-'))
                    .find(|w| self.protected(&w.text))
                {
                    if forced {
                        self.hit(
                            GuardRule::ForcePush,
                            format!("deletes or force-moves the protected branch {}", n.text),
                        );
                    }
                }
            }
            "update-ref" => {
                if let Some(n) = rest
                    .iter()
                    .find(|w| w.text.starts_with("refs/heads/") && self.protected(&w.text))
                {
                    self.hit(
                        GuardRule::ForcePush,
                        format!("rewrites the protected branch ref {}", n.text),
                    );
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ mulchie

    fn is_mulchie_path(&self, p: &Path) -> bool {
        p.starts_with(normalize(self.input.config_dir))
            || p.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s == "Mulchie.app"
            })
    }

    fn kill(&mut self, prog: &str, args: &[&Word]) {
        let names_mulchie = |w: &Word| w.text.to_ascii_lowercase().contains("mulchie");
        if prog != "kill" && args.iter().any(|w| names_mulchie(w)) {
            self.hit(GuardRule::DisableMulchie, format!("stops Mulchie ({prog})"));
            return;
        }
        for w in args {
            if let Ok(pid) = w.text.parse::<u32>() {
                if self.input.mulchie_pids.contains(&pid) {
                    self.hit(
                        GuardRule::DisableMulchie,
                        format!("signals the Mulchie engine (pid {pid})"),
                    );
                    return;
                }
            }
            // `kill $(pgrep mulchie)`
            let finds_mulchie = w.subs.iter().flat_map(|s| s.cmds.iter()).any(|c| {
                simplify(&c.words).is_some_and(|sc| {
                    ["pgrep", "pidof", "lsof", "ps"].contains(&sc.prog.as_str())
                        && sc.args.iter().any(|a| names_mulchie(a))
                })
            });
            if finds_mulchie {
                self.hit(
                    GuardRule::DisableMulchie,
                    "signals the Mulchie engine".into(),
                );
                return;
            }
        }
    }

    fn launchctl(&mut self, args: &[&Word]) {
        let Some(sub) = args.first().map(|w| w.text.as_str()) else {
            return;
        };
        let mentions = args
            .iter()
            .any(|w| w.text.to_ascii_lowercase().contains("mulchie"));
        if mentions && ["unload", "bootout", "remove", "disable", "kill", "stop"].contains(&sub) {
            self.hit(
                GuardRule::DisableMulchie,
                format!("stops Mulchie's launch agent (launchctl {sub})"),
            );
        }
        if ["load", "bootstrap", "enable", "submit"].contains(&sub) && !mentions {
            self.hit(
                GuardRule::StartupWrite,
                format!("loads a launch agent or daemon (launchctl {sub})"),
            );
        }
    }

    fn systemctl(&mut self, args: &[&Word]) {
        let words: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
        let verb = words.iter().find(|w| !w.starts_with('-')).copied();
        let mentions = words
            .iter()
            .any(|w| w.to_ascii_lowercase().contains("mulchie"));
        if mentions && verb.is_some_and(|v| ["stop", "disable", "mask", "kill"].contains(&v)) {
            self.hit(
                GuardRule::DisableMulchie,
                "stops Mulchie's systemd unit".into(),
            );
        }
        if !mentions && verb.is_some_and(|v| ["enable", "link"].contains(&v)) {
            self.hit(
                GuardRule::StartupWrite,
                "enables a systemd unit to start automatically".into(),
            );
        }
    }

    fn mulchie_cmd(&mut self, args: &[&Word]) {
        let a: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
        if a.first() == Some(&"service")
            && a.get(1)
                .is_some_and(|s| ["uninstall", "stop", "disable"].contains(s))
        {
            self.hit(
                GuardRule::DisableMulchie,
                format!("turns off Mulchie's service (mulchie service {})", a[1]),
            );
        }
        if a.first() == Some(&"hooks") && a.get(1) == Some(&"uninstall") {
            self.hit(
                GuardRule::DisableMulchie,
                "removes Mulchie's agent hooks".into(),
            );
        }
    }

    fn mulchie_files(&mut self, prog: &str, args: &[&Word]) {
        if ![
            "chmod", "chown", "truncate", "shred", "unlink", "tee", "sed", "perl", "cp", "ln",
        ]
        .contains(&prog)
        {
            return;
        }
        if (prog == "sed" || prog == "perl") && !args.iter().any(|w| w.text.starts_with("-i")) {
            return;
        }
        let targets: Vec<&&Word> = if prog == "cp" || prog == "ln" {
            args.iter().rfind(|w| !is_option(w)).into_iter().collect()
        } else {
            args.iter().filter(|w| !is_option(w)).collect()
        };
        for w in targets {
            if let Some(p) = self.path_of(w) {
                if self.is_mulchie_path(&p) {
                    self.hit(
                        GuardRule::DisableMulchie,
                        format!("changes Mulchie's files ({})", p.display()),
                    );
                    return;
                }
            }
        }
    }

    // ------------------------------------------------- other deleting tools

    /// `git clean -f` with `-d`, `-x` or `-X` deletes untracked (and ignored)
    /// files: refused outside the boundary.
    fn git_clean(&mut self, dir: Option<PathBuf>, rest: &[&Word]) {
        let mut force = false;
        let mut wide = false;
        let mut specs = Vec::new();
        let mut opts_done = false;
        for w in rest {
            let t = w.text.as_str();
            if !opts_done && t == "--" {
                opts_done = true;
            } else if !opts_done && t == "--force" {
                force = true;
            } else if !opts_done && t.starts_with("--") {
            } else if !opts_done && is_option(w) {
                force |= t.contains('f');
                wide |= t.contains(['d', 'x', 'X']);
            } else {
                specs.push(*w);
            }
        }
        if !force || !wide {
            return;
        }
        let Some(dir) = dir else { return };
        let targets: Vec<PathBuf> = if specs.is_empty() {
            vec![dir]
        } else {
            specs
                .iter()
                .filter(|w| !w.dynamic && !w.glob)
                .map(|w| normalize(&dir.join(&w.text)))
                .collect()
        };
        for p in targets {
            if let Some(why) = self.danger_outside(&p) {
                self.hit(
                    GuardRule::RecursiveDelete,
                    format!("git clean deletes untracked and ignored files in {why}"),
                );
                return;
            }
        }
    }

    /// `rsync --delete` (and `--del`, `--delete-*`) removes files in the
    /// destination that the source lacks: refused outside the boundary.
    fn rsync(&mut self, args: &[&Word]) {
        const TAKES: &[&str] = &[
            "-e",
            "--rsh",
            "--exclude",
            "--include",
            "--filter",
            "-f",
            "--files-from",
            "--password-file",
            "--port",
            "--chmod",
            "--chown",
            "-T",
            "--temp-dir",
            "--log-file",
            "--partial-dir",
            "--backup-dir",
            "--suffix",
            "--timeout",
            "--bwlimit",
            "--max-size",
            "--min-size",
            "--compare-dest",
            "--copy-dest",
            "--link-dest",
            "-B",
            "--block-size",
        ];
        let mut delete = false;
        let mut pos: Vec<&Word> = Vec::new();
        let mut skip = false;
        for w in args {
            if skip {
                skip = false;
                continue;
            }
            let t = w.text.as_str();
            if t == "--del" || t.starts_with("--delete") {
                delete = true;
            } else if TAKES.contains(&t) {
                skip = true;
            } else if !is_option(w) {
                pos.push(w);
            }
        }
        if !delete || pos.len() < 2 {
            return;
        }
        let dest = pos[pos.len() - 1];
        // `host:path` is another machine.
        if dest.text.split('/').next().is_some_and(|h| h.contains(':')) {
            return;
        }
        if let Some(why) = self.path_of(dest).and_then(|p| self.danger_outside(&p)) {
            self.hit(
                GuardRule::RecursiveDelete,
                format!("rsync --delete removes files in {why}"),
            );
        }
    }

    /// `… | xargs rm -r`: the paths come from the input. Allowed when every
    /// command feeding it is a `find` rooted inside the boundary.
    fn xargs(&mut self, args: &[&Word], c: &Cmd, s: &Script) {
        const TAKES: &[&str] = &[
            "-I",
            "-n",
            "-P",
            "-L",
            "-d",
            "-E",
            "-s",
            "-a",
            "--max-args",
            "--max-procs",
            "--delimiter",
            "--arg-file",
            "--max-lines",
            "--eof",
            "--max-chars",
        ];
        let mut i = 0;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if t == "--" {
                i += 1;
                break;
            }
            if !is_option(w) {
                break;
            }
            i += if TAKES.contains(&t) { 2 } else { 1 };
        }
        let rest = &args[i.min(args.len())..];
        let Some(first) = rest.first() else { return };
        if base(&first.text) != "rm" {
            return;
        }
        let tail = &rest[1..];
        self.rm(tail);
        let recursive = tail.iter().any(|w| {
            is_option(w)
                && (w.text == "--recursive"
                    || (!w.text.starts_with("--") && w.text[1..].contains(['r', 'R'])))
        });
        if !recursive {
            return;
        }
        let (bound, _) = self.boundary();
        let feeders: Vec<&Cmd> = s
            .cmds
            .iter()
            .filter(|o| o.pipeline == c.pipeline && o.pos < c.pos)
            .collect();
        let safe = !feeders.is_empty()
            && feeders.iter().all(|o| {
                simplify(&o.words).is_some_and(|sc| {
                    if sc.prog != "find" {
                        return false;
                    }
                    let roots: Vec<&&Word> = sc
                        .args
                        .iter()
                        .take_while(|w| !w.text.starts_with(['-', '(', '!']))
                        .collect();
                    if roots.is_empty() {
                        return self.cwd.as_ref().is_some_and(|d| d.starts_with(&bound));
                    }
                    roots
                        .iter()
                        .all(|w| self.path_of(w).is_some_and(|p| p.starts_with(&bound)))
                })
            });
        if !safe {
            self.hit(
                GuardRule::RecursiveDelete,
                "recursively deletes paths read from its input (xargs rm -r); which ones cannot be seen here".into(),
            );
        }
    }

    // ------------------------------------------------------- pipe to shell

    /// Remember where a downloader writes: `-o f`, `-O f`, `--output f`,
    /// `--output-document=f`, a short cluster ending in `o`/`O`, or `> f`.
    fn note_download(&mut self, prog: &str, args: &[&Word], c: &Cmd) {
        let flag = if prog == "wget" { 'O' } else { 'o' };
        let mut out: Vec<PathBuf> = Vec::new();
        for (i, w) in args.iter().enumerate() {
            let t = w.text.as_str();
            let next = || args.get(i + 1).and_then(|n| self.path_of(n));
            if ["--output", "--output-document", "-o", "-O"].contains(&t) {
                if t == "-o" || t == "-O" {
                    if t.ends_with(flag) {
                        out.extend(next());
                    }
                } else {
                    out.extend(next());
                }
            } else if let Some(v) = t
                .strip_prefix("--output=")
                .or_else(|| t.strip_prefix("--output-document="))
            {
                if !w.dynamic {
                    out.extend(self.try_resolve(v));
                }
            } else if !t.starts_with("--")
                && t.len() > 2
                && t.starts_with('-')
                && t.ends_with(flag)
                && t[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                out.extend(next());
            }
        }
        for r in &c.redirs {
            if r.op.starts_with('>') {
                out.extend(self.path_of(&r.target));
            }
        }
        self.downloaded.extend(out);
    }

    /// Running a file an earlier command downloaded: `sh f`, `python3 f`,
    /// `source f`, `./f`.
    fn runs_download(&mut self, sc: &Simple) {
        if self.downloaded.is_empty() {
            return;
        }
        let prog = sc.prog.as_str();
        let run: Option<PathBuf> = if SHELLS.contains(&prog)
            || INTERPRETERS.contains(&prog)
            || prog == "source"
            || prog == "."
        {
            sc.args
                .iter()
                .find(|w| !is_option(w))
                .and_then(|w| self.path_of(w))
        } else if sc.word.text.contains('/') {
            self.path_of(sc.word)
        } else {
            None
        };
        if let Some(p) = run.filter(|p| self.downloaded.contains(p)) {
            self.hit(
                GuardRule::PipeToShell,
                format!("runs {}, which an earlier command downloaded", p.display()),
            );
        }
    }

    fn has_download(&self, scripts: &[Script]) -> bool {
        scripts.iter().flat_map(|s| s.cmds.iter()).any(|c| {
            simplify(&c.words).is_some_and(|sc| DOWNLOADERS.contains(&sc.prog.as_str()))
                || c.words.iter().any(|w| self.has_download(&w.subs))
        })
    }

    /// The commands before `c` in its pipeline download something.
    fn piped_from_download(&self, c: &Cmd, s: &Script) -> bool {
        c.pos > 0
            && s.cmds.iter().any(|o| {
                o.pipeline == c.pipeline
                    && o.pos < c.pos
                    && (simplify(&o.words)
                        .is_some_and(|sc| DOWNLOADERS.contains(&sc.prog.as_str()))
                        || o.words.iter().any(|w| self.has_download(&w.subs)))
            })
    }

    fn shell(&mut self, prog: &str, args: &[&Word], c: &Cmd, s: &Script, depth: usize) {
        // `bash -c "$(curl …)"`, `source <(curl …)`, `eval "$(wget -O- …)"`.
        if args.iter().any(|w| self.has_download(&w.subs)) {
            self.hit(
                GuardRule::PipeToShell,
                format!("runs downloaded code in {prog}"),
            );
            return;
        }
        if prog == "source" || prog == "." || prog == "eval" {
            return;
        }
        // `sh -c 'script'`: check the script like a command line.
        let mut i = 0;
        let mut script_arg = None;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if t.starts_with('-') && !t.starts_with("--") && t.contains('c') {
                script_arg = args.get(i + 1).copied();
                break;
            }
            if !t.starts_with('-') {
                break;
            }
            i += 1;
        }
        if let Some(w) = script_arg {
            if depth < MAX_DEPTH && !w.dynamic {
                let inner = parse_script(&w.text, self.home(), depth + 1);
                self.script(&inner, depth + 1);
            }
            return;
        }
        // Reads its program from stdin: no script file argument.
        let reads_stdin = args.iter().all(|w| is_option(w) || w.text == "-");
        if reads_stdin && self.piped_from_download(c, s) {
            self.hit(
                GuardRule::PipeToShell,
                format!("pipes a download straight into {prog}"),
            );
        }
    }

    fn interpreter_stdin(&mut self, prog: &str, args: &[&Word], c: &Cmd, s: &Script) {
        let reads_stdin = args.iter().all(|w| {
            (is_option(w) && !["-c", "-e", "-m"].contains(&w.text.as_str())) || w.text == "-"
        });
        if reads_stdin && self.piped_from_download(c, s) {
            self.hit(
                GuardRule::PipeToShell,
                format!("pipes a download straight into {prog}"),
            );
        }
    }

    // --------------------------------------------------------- credentials

    fn credential_why(&self, p: &Path) -> Option<&'static str> {
        let h = normalize(self.home());
        let rel = p.strip_prefix(&h).ok()?;
        let s = rel.to_string_lossy();
        let name = p.file_name()?.to_string_lossy().to_string();
        if let Ok(r) = rel.strip_prefix(".ssh") {
            let deep = r.components().count() >= 1;
            let private = (name.starts_with("id_") && !name.ends_with(".pub"))
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || name == "identity";
            return (deep && private).then_some("an SSH private key");
        }
        let exact: &[(&str, &str)] = &[
            (".aws/credentials", "AWS credentials"),
            (
                ".config/gcloud/application_default_credentials.json",
                "Google Cloud credentials",
            ),
            (".config/gcloud/credentials.db", "Google Cloud credentials"),
            (
                ".config/gcloud/access_tokens.db",
                "Google Cloud credentials",
            ),
            (".azure/accessTokens.json", "Azure credentials"),
            (".azure/msal_token_cache.json", "Azure credentials"),
            (".azure/msal_token_cache.bin", "Azure credentials"),
            (".kube/config", "Kubernetes credentials"),
            (".docker/config.json", "Docker registry credentials"),
            (".netrc", "saved network passwords (.netrc)"),
            (".git-credentials", "saved git credentials"),
            (".pypirc", "PyPI credentials"),
            (".npmrc", "npm tokens"),
            (".config/gh/hosts.yml", "GitHub CLI tokens"),
        ];
        if let Some((_, why)) = exact.iter().find(|(e, _)| s == *e) {
            return Some(why);
        }
        let dirs: &[(&str, &str)] = &[
            (".aws/sso/cache", "AWS SSO tokens"),
            (
                ".config/gcloud/legacy_credentials",
                "Google Cloud credentials",
            ),
            (".gnupg/private-keys-v1.d", "GnuPG private keys"),
            (".password-store", "the password store"),
            ("Library/Keychains", "a keychain file"),
        ];
        dirs.iter()
            .find(|(d, _)| rel.starts_with(d) && rel != Path::new(d))
            .map(|(_, why)| *why)
    }

    fn credential(&mut self, p: &Path, how: &str) {
        if let Some(why) = self.credential_why(p) {
            self.hit(
                GuardRule::CredentialRead,
                format!("reads {why} ({}) via {how}", p.display()),
            );
        }
    }

    fn reads(&mut self, prog: &str, args: &[&Word]) {
        const READERS: &[&str] = &[
            "cat",
            "less",
            "more",
            "head",
            "tail",
            "bat",
            "view",
            "cp",
            "scp",
            "rsync",
            "base64",
            "xxd",
            "od",
            "hexdump",
            "strings",
            "grep",
            "rg",
            "egrep",
            "fgrep",
            "awk",
            "sed",
            "tar",
            "zip",
            "7z",
            "curl",
            "wget",
            "nc",
            "ncat",
            "socat",
            "openssl",
            "gpg",
            "vim",
            "vi",
            "nano",
            "emacs",
            "code",
            "open",
            "python",
            "python3",
            "node",
            "perl",
            "ruby",
            "jq",
            "sort",
            "uniq",
            "wc",
            "diff",
            "cmp",
            "md5",
            "md5sum",
            "sha256sum",
            "shasum",
            "nl",
            "tac",
            "ssh-keygen",
            "dd",
        ];
        if !READERS.contains(&prog) {
            return;
        }
        for w in args {
            let t = w.text.as_str();
            // `curl -d @file`, `-F x=@file`, `--data-binary @file`, `dd if=file`.
            let t = t
                .split_once("=@")
                .map(|(_, f)| f)
                .or_else(|| t.strip_prefix('@'))
                .or_else(|| t.strip_prefix("if="))
                .unwrap_or(t);
            if t.is_empty() || (t.starts_with('-') && t.len() > 1) || w.dynamic || w.glob {
                continue;
            }
            let p = match t.strip_prefix("~/") {
                Some(r) => normalize(&self.home().join(r)),
                None => self.resolve(t),
            };
            if self.credential_why(&p).is_some() {
                self.credential(&p, prog);
                return;
            }
        }
    }

    fn security(&mut self, args: &[&Word]) {
        let Some(sub) = args.first().map(|w| w.text.as_str()) else {
            return;
        };
        let prints = args.iter().any(|w| w.text == "-w" || w.text == "-g");
        let hit = match sub {
            "dump-keychain" | "export" => true,
            "find-generic-password" | "find-internet-password" => prints,
            _ => false,
        };
        if hit {
            self.hit(
                GuardRule::CredentialRead,
                format!("reads secrets from the keychain (security {sub})"),
            );
        }
    }

    // ----------------------------------------------------- startup writes

    fn startup_why(&self, p: &Path) -> Option<&'static str> {
        let h = normalize(self.home());
        if let Ok(rel) = p.strip_prefix(&h) {
            let s = rel.to_string_lossy();
            const RC: &[&str] = &[
                ".bashrc",
                ".bash_profile",
                ".bash_login",
                ".bash_logout",
                ".profile",
                ".zshrc",
                ".zprofile",
                ".zshenv",
                ".zlogin",
                ".zlogout",
                ".cshrc",
                ".tcshrc",
                ".config/fish/config.fish",
            ];
            if RC.contains(&s.as_ref()) || rel.starts_with(".config/fish/conf.d") {
                return Some("a shell startup file");
            }
            if rel.starts_with("Library/LaunchAgents") {
                return Some("a launch agent");
            }
            if rel.starts_with(".config/systemd/user") {
                return Some("a systemd user unit");
            }
            if rel.starts_with(".config/autostart") {
                return Some("a desktop autostart entry");
            }
        }
        if p.starts_with("/Library/LaunchAgents") || p.starts_with("/Library/LaunchDaemons") {
            return Some("a system launch agent or daemon");
        }
        if p.starts_with("/etc/systemd")
            || p.starts_with("/etc/cron.d")
            || p == Path::new("/etc/crontab")
        {
            return Some("a system service or cron file");
        }
        if [
            "/etc/profile",
            "/etc/zshrc",
            "/etc/bashrc",
            "/etc/zprofile",
            "/etc/zshenv",
        ]
        .iter()
        .any(|f| p == Path::new(f))
            || p.starts_with("/etc/profile.d")
        {
            return Some("a system shell startup file");
        }
        None
    }

    fn write_target(&mut self, p: &Path, how: &str) {
        if let Some(why) = self.startup_why(p) {
            self.hit(
                GuardRule::StartupWrite,
                format!("writes {why} ({}) via {how}", p.display()),
            );
        }
        if self.is_mulchie_path(p) {
            self.hit(
                GuardRule::DisableMulchie,
                format!("writes Mulchie's files ({}) via {how}", p.display()),
            );
        }
    }

    fn writes(&mut self, prog: &str, args: &[&Word]) {
        let nonopt: Vec<&Word> = args.iter().copied().filter(|w| !is_option(w)).collect();
        let targets: Vec<&Word> = match prog {
            "tee" | "truncate" | "touch" => nonopt,
            "sed" | "perl" if args.iter().any(|w| w.text.starts_with("-i")) => nonopt,
            "cp" | "mv" | "install" | "ln" | "rsync" | "scp" => {
                let t = args
                    .windows(2)
                    .find(|p| p[0].text == "-t")
                    .map(|p| p[1])
                    .or_else(|| nonopt.last().copied().filter(|_| nonopt.len() >= 2));
                t.into_iter().collect()
            }
            "dd" => {
                for w in args {
                    if let Some(f) = w.text.strip_prefix("of=") {
                        if !w.dynamic {
                            let p = self.resolve(f);
                            self.write_target(&p, "dd");
                        }
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        for w in targets {
            if let Some(p) = self.path_of(w) {
                // `cp x ~/Library/LaunchAgents/` names the directory.
                let probe = if w.text.ends_with('/') {
                    p.join("x")
                } else {
                    p
                };
                if prog != "touch" || self.startup_why(&probe).is_some() {
                    self.write_target(&probe, prog);
                }
            }
        }
    }
}

// ------------------------------------------------------------ heavy builds

/// When `command` runs a heavy build (compile, test, bundle), the command
/// as given, for advice like `mulchie run -- <cmd>`. `None` when it is not one
/// or is already wrapped in `mulchie run`. Uses the build tool names the
/// monitor classifies as builds, narrowed to the subcommands that compile.
pub fn heavy_build(command: &str, home: &Path) -> Option<String> {
    let text = truncate_utf8(command, MAX_COMMAND_BYTES);
    let script = parse_script(text, home, 0);
    for c in &script.cmds {
        let Some(sc) = simplify(&c.words) else {
            continue;
        };
        let a: Vec<&str> = sc.args.iter().map(|w| w.text.as_str()).collect();
        let first = a.iter().find(|x| !x.starts_with('-')).copied();
        let heavy = match sc.prog.as_str() {
            "mulchie" => return None,
            "cargo" => first.is_some_and(|s| {
                [
                    "build", "b", "test", "t", "check", "c", "clippy", "run", "r", "bench",
                    "install", "doc", "nextest",
                ]
                .contains(&s.trim_start_matches('+'))
            }),
            "make" | "gmake" | "ninja" | "xcodebuild" | "bazel" | "bazelisk" | "buck2"
            | "webpack" | "esbuild" | "rollup" => true,
            "cmake" => a.contains(&"--build"),
            "swift" => first.is_some_and(|s| ["build", "test", "run"].contains(&s)),
            "go" => first.is_some_and(|s| ["build", "test", "install", "run"].contains(&s)),
            "gradle" | "gradlew" => true,
            "mvn" | "mvnw" => true,
            "zig" => first == Some("build"),
            "tsc" => true,
            "next" | "vite" | "nuxt" | "astro" => first == Some("build"),
            "npm" | "pnpm" | "yarn" | "bun" => {
                let script = if first == Some("run") {
                    a.iter().filter(|x| !x.starts_with('-')).nth(1).copied()
                } else {
                    first
                };
                script.is_some_and(|s| {
                    ["build", "test", "compile", "bundle"].contains(&s)
                        || s.starts_with("build:")
                        || s.starts_with("test:")
                })
            }
            _ => false,
        };
        if heavy {
            return Some(text.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
