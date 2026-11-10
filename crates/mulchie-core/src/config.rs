use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub use crate::paths::{config_dir, home_dir};
use crate::paths::{random_hex, Paths};

/// The config schema version this build writes. A file with a higher version
/// was written by a newer Mulchie and is refused rather than misread.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Empty means "the default roots under the home directory".
    pub scan_roots: Vec<PathBuf>,
    pub stale_hours: u64,
    pub low_disk_gb: u64,
    pub auto_reclaim: bool,
    pub auto_reclaim_target_gb: u64,
    pub include_node_modules: bool,
    pub include_xcode: bool,
    pub throttle_builds: bool,
    pub throttle_mem_free_pct: f32,
    pub throttle_cpu_pct: f32,
    pub build_nice: i32,
    pub scan_interval_mins: u64,
    pub api_port: u16,
    pub max_heavy_jobs: usize,
    pub worktree_idle_days: u64,
    pub worktree_target_stale_hours: u64,
    pub auto_sweep_worktrees: bool,
    pub alert_below_gb: u64,
    /// Gitignored files in a worktree (other than known caches) are archived
    /// before removal. Above this size the worktree is skipped instead.
    pub backup_ignored_max_mb: u64,
    /// What `watch` looks for and how loudly (see [`WatchConfig`]).
    pub watch: WatchConfig,
    /// Agent executables recognised as session roots beyond the built-in list.
    pub agents: AgentsConfig,
    /// Session tracking and the per-session activity record.
    pub sessions: SessionsConfig,
    /// Per-session budgets. Empty: report only.
    pub limits: LimitsConfig,
    /// Guard rules applied to shell commands sent through `POST /v1/hook`.
    pub guard: GuardConfig,
    /// Update and advisory checks.
    pub updates: UpdatesConfig,
}

/// A finding id kept quiet until `until_unix` (`None`: until undismissed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dismissal {
    pub id: String,
    #[serde(default)]
    pub until_unix: Option<u64>,
}

/// Settings for the read-only detectors in [`crate::watch`]. Detectors only
/// report; nothing here makes Mulchie act.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    /// Seconds between runs on the scan loop.
    pub interval_secs: u64,
    /// An agent or build process older than this, idle over the history
    /// window, is reported as stale.
    pub stale_process_hours: u64,
    /// "Idle" for the stale check: average CPU below this percent of one core.
    pub stale_idle_cpu_pct: f32,
    /// A process stopped (SIGSTOP) longer than this is reported.
    pub stopped_hours: u64,
    /// The window for runaway CPU and growing memory.
    pub runaway_minutes: u64,
    /// Average CPU (percent of one core) over the window to count as runaway.
    pub runaway_cpu_pct: f32,
    /// Memory growth over the window to report.
    pub memory_growth_mb: u64,
    /// Look for listening sockets (one bounded `lsof` or `/proc/net` read per run).
    pub check_listeners: bool,
    /// macOS code signatures and quarantine flags of running executables.
    pub check_signatures: bool,
    /// Executables never reported (exact paths).
    pub allow_exes: Vec<PathBuf>,
    /// Finding ids never reported.
    pub allow_ids: Vec<String>,
    /// Findings kept quiet for a while (`POST /v1/findings/dismiss`).
    pub dismissed: Vec<Dismissal>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 60,
            stale_process_hours: 72,
            stale_idle_cpu_pct: 1.0,
            stopped_hours: 6,
            runaway_minutes: 30,
            runaway_cpu_pct: 90.0,
            memory_growth_mb: 1024,
            check_listeners: true,
            check_signatures: true,
            allow_exes: Vec::new(),
            allow_ids: Vec::new(),
            dismissed: Vec::new(),
        }
    }
}

impl WatchConfig {
    pub fn validate(&self, e: &mut Vec<String>) {
        if !(10..=3600).contains(&self.interval_secs) {
            e.push("watch.interval_secs must be between 10 and 3600".into());
        }
        if !(1..=8760).contains(&self.stale_process_hours) {
            e.push("watch.stale_process_hours must be between 1 and 8760".into());
        }
        if !(0.0..=100.0).contains(&self.stale_idle_cpu_pct) {
            e.push("watch.stale_idle_cpu_pct must be between 0 and 100".into());
        }
        if !(1..=8760).contains(&self.stopped_hours) {
            e.push("watch.stopped_hours must be between 1 and 8760".into());
        }
        if !(5..=120).contains(&self.runaway_minutes) {
            e.push("watch.runaway_minutes must be between 5 and 120".into());
        }
        if !(1.0..=10_000.0).contains(&self.runaway_cpu_pct) {
            e.push("watch.runaway_cpu_pct must be between 1 and 10000".into());
        }
        if !(16..=1_048_576).contains(&self.memory_growth_mb) {
            e.push("watch.memory_growth_mb must be between 16 and 1048576".into());
        }
        if let Some(p) = self.allow_exes.iter().find(|p| !p.is_absolute()) {
            e.push(format!(
                "watch.allow_exes entry {} is not absolute",
                p.display()
            ));
        }
        if self.allow_ids.len() + self.dismissed.len() > 10_000 {
            e.push("watch.allow_ids and watch.dismissed hold at most 10000 ids".into());
        }
        if self
            .allow_ids
            .iter()
            .chain(self.dismissed.iter().map(|d| &d.id))
            .any(|i| i.is_empty() || i.len() > 512)
        {
            e.push("finding ids must be 1 to 512 characters".into());
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            scan_roots: Vec::new(),
            stale_hours: 24,
            low_disk_gb: 25,
            auto_reclaim: false,
            auto_reclaim_target_gb: 60,
            include_node_modules: false,
            include_xcode: cfg!(target_os = "macos"),
            throttle_builds: true,
            throttle_mem_free_pct: 8.0,
            throttle_cpu_pct: 92.0,
            build_nice: 10,
            scan_interval_mins: 30,
            api_port: 47474,
            max_heavy_jobs: 0,
            worktree_idle_days: 10,
            worktree_target_stale_hours: 48,
            auto_sweep_worktrees: false,
            alert_below_gb: 50,
            backup_ignored_max_mb: 200,
            watch: WatchConfig::default(),
            agents: AgentsConfig::default(),
            sessions: SessionsConfig::default(),
            limits: LimitsConfig::default(),
            guard: GuardConfig::default(),
            updates: UpdatesConfig::default(),
        }
    }
}

// ------------------------------------------------------------ agents

/// Extra agent executables (see [`crate::session`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AgentsConfig {
    /// Executable file names (no `/`) that start a session, matched like the
    /// built-in list: the process name, the executable's file name, `argv[0]`,
    /// or the script an interpreter runs.
    pub extra: Vec<String>,
}

impl AgentsConfig {
    pub fn validate(&self, e: &mut Vec<String>) {
        if self.extra.len() > 64 {
            e.push("agents.extra holds at most 64 names".into());
        }
        if self
            .extra
            .iter()
            .any(|n| n.is_empty() || n.len() > 64 || n.contains('/') || n.trim() != n)
        {
            e.push(
                "agents.extra names must be 1 to 64 characters, without '/' or surrounding spaces"
                    .into(),
            );
        }
    }
}

// ---------------------------------------------------------- sessions

/// Session tracking and the activity record (`<config dir>/sessions/`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionsConfig {
    /// Track sessions at all. Off: no sessions, records, limits or hook mapping.
    pub enabled: bool,
    /// Records of ended sessions are deleted after this many days.
    pub retain_days: u64,
    /// One session's record stops growing at this size (a `truncated` marker
    /// is written).
    pub max_record_mb: u64,
    /// ... or at this many events.
    pub max_events: u64,
    /// All records together; the oldest ended sessions' records go first.
    pub max_total_mb: u64,
    /// Process-start events written per session per minute; the rest are
    /// summed up in one `processes_coalesced` event.
    pub process_events_per_minute: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retain_days: 14,
            max_record_mb: 4,
            max_events: 20_000,
            max_total_mb: 200,
            process_events_per_minute: 60,
        }
    }
}

impl SessionsConfig {
    pub fn validate(&self, e: &mut Vec<String>) {
        if !(1..=365).contains(&self.retain_days) {
            e.push("sessions.retain_days must be between 1 and 365".into());
        }
        if !(1..=100).contains(&self.max_record_mb) {
            e.push("sessions.max_record_mb must be between 1 and 100".into());
        }
        if !(100..=1_000_000).contains(&self.max_events) {
            e.push("sessions.max_events must be between 100 and 1000000".into());
        }
        if !(10..=10_000).contains(&self.max_total_mb) {
            e.push("sessions.max_total_mb must be between 10 and 10000".into());
        }
        if !(1..=10_000).contains(&self.process_events_per_minute) {
            e.push("sessions.process_events_per_minute must be between 1 and 10000".into());
        }
    }
}

// ------------------------------------------------------------ limits

/// What happens when a session stays over a cap for `sustain_secs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum LimitAction {
    /// A finding and a notification; nothing is signalled.
    #[default]
    Notify,
    /// Lower the priority of the session's processes until it is back under.
    Deprioritize,
    /// SIGSTOP the session's descendants; SIGCONT after `resume_after_secs`.
    Pause,
    /// SIGTERM the session's descendants, SIGKILL after `stop_grace_secs`.
    Stop,
}

/// One cap.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Cap {
    /// The ceiling, in the cap's unit (cores, MB, processes, MB per hour, hours).
    pub max: f64,
    /// How long the session must stay above `max` before the action runs.
    #[serde(default = "default_sustain")]
    pub sustain_secs: u64,
    #[serde(default)]
    pub action: LimitAction,
}

fn default_sustain() -> u64 {
    60
}

/// A budget: any subset of the caps, plus how pausing and stopping behave.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Budget {
    pub cpu_cores: Option<Cap>,
    pub memory_mb: Option<Cap>,
    pub processes: Option<Cap>,
    pub write_mb_per_hour: Option<Cap>,
    pub runtime_hours: Option<Cap>,
    /// Processes paused by a `pause` cap are resumed after this long.
    pub resume_after_secs: u64,
    /// Let `pause` and `stop` reach the agent's own root process too.
    pub include_agent: bool,
    /// Between SIGTERM and SIGKILL for a `stop` cap.
    pub stop_grace_secs: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            cpu_cores: None,
            memory_mb: None,
            processes: None,
            write_mb_per_hour: None,
            runtime_hours: None,
            resume_after_secs: 300,
            include_agent: false,
            stop_grace_secs: 10,
        }
    }
}

impl Budget {
    /// `(name, cap)` for every cap that is set.
    pub fn caps(&self) -> Vec<(&'static str, Cap)> {
        [
            ("cpu_cores", self.cpu_cores),
            ("memory_mb", self.memory_mb),
            ("processes", self.processes),
            ("write_mb_per_hour", self.write_mb_per_hour),
            ("runtime_hours", self.runtime_hours),
        ]
        .into_iter()
        .filter_map(|(n, c)| c.map(|c| (n, c)))
        .collect()
    }

    fn validate(&self, at: &str, e: &mut Vec<String>) {
        for (name, c) in self.caps() {
            if !c.max.is_finite() || c.max <= 0.0 || c.max > 1e9 {
                e.push(format!("{at}.{name}.max must be a positive number"));
            }
            if c.sustain_secs > 86_400 {
                e.push(format!(
                    "{at}.{name}.sustain_secs must be between 0 and 86400"
                ));
            }
        }
        if !(10..=86_400).contains(&self.resume_after_secs) {
            e.push(format!(
                "{at}.resume_after_secs must be between 10 and 86400"
            ));
        }
        if !(1..=300).contains(&self.stop_grace_secs) {
            e.push(format!("{at}.stop_grace_secs must be between 1 and 300"));
        }
    }
}

/// A budget for every session whose workspace is at or under `path`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceBudget {
    pub path: PathBuf,
    pub budget: Budget,
}

/// `limits`: a default budget and per-workspace overrides. The override with
/// the longest matching path replaces the default whole (caps are not merged).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LimitsConfig {
    pub default: Option<Budget>,
    pub workspaces: Vec<WorkspaceBudget>,
    /// With or without budgets: a session using at least this share of the
    /// machine's CPU or memory while health is strained or critical, for
    /// `hog_sustain_secs`, becomes a `session_hog` finding (report only).
    pub hog_share_pct: f32,
    pub hog_sustain_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            default: None,
            workspaces: Vec::new(),
            hog_share_pct: 50.0,
            hog_sustain_secs: 120,
        }
    }
}

impl LimitsConfig {
    /// The budget for a session working in `workspace`, if any.
    pub fn budget_for(&self, workspace: Option<&Path>) -> Option<&Budget> {
        let over = workspace.and_then(|w| {
            self.workspaces
                .iter()
                .filter(|o| w.starts_with(&o.path))
                .max_by_key(|o| o.path.components().count())
        });
        over.map(|o| &o.budget).or(self.default.as_ref())
    }

    /// No budget anywhere: Mulchie only reports.
    pub fn report_only(&self) -> bool {
        self.default.is_none() && self.workspaces.is_empty()
    }

    pub fn validate(&self, e: &mut Vec<String>) {
        if let Some(b) = &self.default {
            b.validate("limits.default", e);
        }
        if self.workspaces.len() > 256 {
            e.push("limits.workspaces holds at most 256 entries".into());
        }
        for (i, w) in self.workspaces.iter().enumerate() {
            if !w.path.is_absolute() {
                e.push(format!("limits.workspaces[{i}].path must be absolute"));
            }
            w.budget
                .validate(&format!("limits.workspaces[{i}].budget"), e);
        }
        if !(10.0..=100.0).contains(&self.hog_share_pct) {
            e.push("limits.hog_share_pct must be between 10 and 100".into());
        }
        if !(10..=3600).contains(&self.hog_sustain_secs) {
            e.push("limits.hog_sustain_secs must be between 10 and 3600".into());
        }
    }
}

// ------------------------------------------------------------- guard

/// How a guard rule answers a match.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum RuleMode {
    Off,
    /// The client asks the user (or tells the agent).
    #[default]
    Warn,
    /// The client refuses the command.
    Block,
}

/// One mode per guard rule (see [`crate::guard`]). Every rule defaults to `warn`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct GuardRules {
    /// `rm -r` / `mv` of something outside the session's workspace, or of
    /// home or `/`.
    pub recursive_delete: RuleMode,
    /// `curl … | sh` and friends.
    pub pipe_to_shell: RuleMode,
    /// `sudo` or `doas`.
    pub sudo: RuleMode,
    /// Force-push to, or delete, a protected branch; rewrite its history.
    pub force_push: RuleMode,
    /// Stop, unload or remove Mulchie, or edit its files.
    pub disable_mulchie: RuleMode,
    /// Read SSH private keys, cloud credentials, or dump the keychain.
    pub credential_read: RuleMode,
    /// Write shell startup files, launch agents, systemd user units or crontab.
    pub startup_write: RuleMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GuardConfig {
    /// Off: every command is allowed and no rule is evaluated.
    pub enabled: bool,
    pub rules: GuardRules,
    /// Branch names the `force_push` rule protects.
    pub protected_branches: Vec<String>,
    /// Under pressure, advise wrapping recognised heavy builds in `mulchie run`.
    pub build_advice: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rules: GuardRules::default(),
            protected_branches: vec!["main".into(), "master".into()],
            build_advice: true,
        }
    }
}

impl GuardConfig {
    pub fn validate(&self, e: &mut Vec<String>) {
        if self.protected_branches.len() > 64 {
            e.push("guard.protected_branches holds at most 64 names".into());
        }
        if self
            .protected_branches
            .iter()
            .any(|b| b.is_empty() || b.len() > 255 || b.contains(char::is_whitespace))
        {
            e.push(
                "guard.protected_branches names must be 1 to 255 characters without spaces".into(),
            );
        }
    }
}

// ----------------------------------------------------------- updates

/// Whether update checks run. `off` makes no network request and runs no
/// version command.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCheck {
    #[default]
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdatesConfig {
    pub check: UpdateCheck,
    /// Mulchie's own latest GitHub release.
    pub mulchie: bool,
    /// Installed agent tools (`claude`, `codex`, `gemini`, `cursor-agent`).
    pub agents: bool,
    /// `brew outdated`, local data only (skipped when brew is absent).
    pub homebrew: bool,
    /// Send lockfile package names and versions to OSV.dev. Off by default:
    /// it tells a third party what your projects depend on.
    pub advisories: bool,
    /// How long results are reused before checking again.
    pub interval_hours: u64,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            check: UpdateCheck::On,
            mulchie: true,
            agents: true,
            homebrew: true,
            advisories: false,
            interval_hours: 24,
        }
    }
}

impl UpdatesConfig {
    pub fn validate(&self, e: &mut Vec<String>) {
        if !(6..=720).contains(&self.interval_hours) {
            e.push("updates.interval_hours must be between 6 and 720".into());
        }
    }
}

/// Scan roots used when the config names none.
pub fn default_scan_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = [
        "Developer",
        "Projects",
        "code",
        "src",
        "dev",
        "repos",
        "work",
    ]
    .iter()
    .map(|n| home.join(n))
    .filter(|p| p.is_dir())
    .collect();
    if roots.is_empty() {
        roots.push(home.to_path_buf());
    }
    roots
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file is not valid JSON for this schema.
    Parse(String),
    /// The file parsed but a value is out of range.
    Invalid(Vec<String>),
    /// Reading or writing failed.
    Io(String),
    /// The file changed on disk since this engine loaded or wrote it; the
    /// change was not saved, and the engine now uses the file's content.
    Changed(String),
    /// This engine does not hold the engine lock: only the running engine
    /// writes the config (send the change to its API instead).
    NotLockHolder(String),
    /// A key the config does not have, by its path (`watch.intervl_secs`,
    /// `limits.workspaces[0].bogus`). Nothing was saved.
    UnknownKey(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(e) => write!(f, "config file could not be parsed: {e}"),
            ConfigError::Invalid(v) => write!(f, "config has invalid values: {}", v.join("; ")),
            ConfigError::Io(e) => write!(f, "config file error: {e}"),
            ConfigError::Changed(e) => write!(f, "config file changed on disk: {e}"),
            ConfigError::NotLockHolder(e) => write!(f, "config not saved: {e}"),
            ConfigError::UnknownKey(p) => write!(
                f,
                "config has no key {p}; nothing was saved (GET /v1/config lists the keys)"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The first key in `given` (a whole config or a merge patch, as JSON) that
/// `cfg`, parsed from it, does not have: serde drops unknown keys, so a key
/// missing after a round trip is one the config does not know. Keys set to
/// null (a patch's removal, an unset option) are not checked; maps keyed by
/// user data keep their keys and pass.
pub fn unknown_key(given: &serde_json::Value, cfg: &Config) -> Option<String> {
    let kept = serde_json::to_value(cfg).ok()?;
    fn walk(g: &serde_json::Value, k: &serde_json::Value, at: &str) -> Option<String> {
        use serde_json::Value;
        match (g, k) {
            (Value::Object(gm), Value::Object(km)) => gm.iter().find_map(|(key, gv)| {
                if gv.is_null() {
                    return None;
                }
                let p = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                match km.get(key) {
                    None => Some(p),
                    Some(kv) => walk(gv, kv, &p),
                }
            }),
            (Value::Array(ga), Value::Array(ka)) => ga
                .iter()
                .zip(ka)
                .enumerate()
                .find_map(|(i, (gv, kv))| walk(gv, kv, &format!("{at}[{i}]"))),
            _ => None,
        }
    }
    walk(given, &kept, "")
}

/// The result of loading: the config to run with, and the problem with the file
/// if there was one. On a problem the config is the defaults with every
/// automatic destructive behaviour off.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub error: Option<ConfigError>,
}

impl Config {
    /// Range checks. Dangerous values are errors, never silently clamped.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut e = Vec::new();
        if self.version > CONFIG_VERSION {
            e.push(format!(
                "version {} is newer than this Mulchie understands ({CONFIG_VERSION})",
                self.version
            ));
        }
        if self.worktree_idle_days < 1 {
            e.push("worktree_idle_days must be at least 1".into());
        }
        if self.stale_hours < 1 {
            e.push("stale_hours must be at least 1".into());
        }
        if self.worktree_target_stale_hours < 1 {
            e.push("worktree_target_stale_hours must be at least 1".into());
        }
        if !(1..=1440).contains(&self.scan_interval_mins) {
            e.push("scan_interval_mins must be between 1 and 1440".into());
        }
        if self.auto_reclaim_target_gb <= self.low_disk_gb {
            e.push("auto_reclaim_target_gb must be greater than low_disk_gb".into());
        }
        if !(0.0..=100.0).contains(&self.throttle_mem_free_pct) {
            e.push("throttle_mem_free_pct must be between 0 and 100".into());
        }
        if !(0.0..=100.0).contains(&self.throttle_cpu_pct) {
            e.push("throttle_cpu_pct must be between 0 and 100".into());
        }
        if !(0..=20).contains(&self.build_nice) {
            e.push("build_nice must be between 0 and 20".into());
        }
        if self.api_port == 0 {
            e.push("api_port must not be 0".into());
        }
        if let Some(r) = self.scan_roots.iter().find(|r| !r.is_absolute()) {
            e.push(format!("scan root {} is not absolute", r.display()));
        }
        self.watch.validate(&mut e);
        self.agents.validate(&mut e);
        self.sessions.validate(&mut e);
        self.limits.validate(&mut e);
        self.guard.validate(&mut e);
        self.updates.validate(&mut e);
        if e.is_empty() {
            Ok(())
        } else {
            Err(e)
        }
    }

    /// Defaults with every automatic destructive behaviour off.
    pub fn safe_defaults() -> Self {
        Self {
            auto_reclaim: false,
            auto_sweep_worktrees: false,
            ..Self::default()
        }
    }

    /// Scan roots to use: the configured ones, or the defaults under `home`.
    pub fn roots(&self, paths: &Paths) -> Vec<PathBuf> {
        if self.scan_roots.is_empty() {
            default_scan_roots(&paths.home)
        } else {
            self.scan_roots.clone()
        }
    }

    /// Parse and validate a config file's bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        let cfg: Config =
            serde_json::from_slice(bytes).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate().map_err(ConfigError::Invalid)?;
        Ok(cfg)
    }

    /// Load `paths.config_file()`. A missing file is not an error.
    pub fn load_from(paths: &Paths) -> LoadedConfig {
        match std::fs::read(paths.config_file()) {
            Ok(b) => match Self::parse(&b) {
                Ok(mut config) => {
                    config.version = CONFIG_VERSION;
                    LoadedConfig {
                        config,
                        error: None,
                    }
                }
                Err(e) => LoadedConfig {
                    config: Self::safe_defaults(),
                    error: Some(e),
                },
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadedConfig {
                config: Self::default(),
                error: None,
            },
            Err(e) => LoadedConfig {
                config: Self::safe_defaults(),
                error: Some(ConfigError::Io(e.to_string())),
            },
        }
    }

    /// Validate and save atomically (temp file in the same directory, then rename).
    ///
    /// An existing file that does not parse or validate is never overwritten: it is
    /// first renamed to `config.json.invalid-<unix secs>-<random>` so the user's
    /// content survives, then the new file is written.
    pub fn save_to(&self, paths: &Paths, now: u64) -> Result<(), ConfigError> {
        self.validate().map_err(ConfigError::Invalid)?;
        let mut cfg = self.clone();
        cfg.version = CONFIG_VERSION;
        let path = paths.config_file();
        let dir = &paths.config_dir;
        create_private_dir(dir).map_err(|e| ConfigError::Io(e.to_string()))?;
        if let Ok(old) = std::fs::read(&path) {
            if Self::parse(&old).is_err() {
                let aside = unique_sibling(&path, &format!("invalid-{now}"))
                    .map_err(|e| ConfigError::Io(e.to_string()))?;
                std::fs::rename(&path, &aside).map_err(|e| {
                    ConfigError::Io(format!("could not move the invalid config aside: {e}"))
                })?;
            }
        }
        let data = serde_json::to_vec_pretty(&cfg).map_err(|e| ConfigError::Io(e.to_string()))?;
        write_atomic(&path, &data, 0o600).map_err(|e| ConfigError::Io(e.to_string()))
    }

    /// Load from the environment's config path. Entry points only.
    pub fn load() -> Self {
        Self::load_from(&Paths::from_env()).config
    }

    /// The config path from the environment. Entry points only.
    pub fn path() -> PathBuf {
        Paths::from_env().config_file()
    }
}

/// The API port for this process: `MULCHIE_PORT` when set and valid, else the
/// config's. The override is never written back to the config file.
pub fn effective_port(cfg: &Config) -> u16 {
    port_from(cfg, std::env::var("MULCHIE_PORT").ok().as_deref())
}

/// The port from an override value (`MULCHIE_PORT`) when it is a valid
/// non-zero port, else the config's.
pub fn port_from(cfg: &Config, env: Option<&str>) -> u16 {
    env.and_then(|p| p.trim().parse().ok())
        .filter(|p: &u16| *p != 0)
        .unwrap_or(cfg.api_port)
}

/// Create `dir` (and parents) with mode 0700 for the last component.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.exists() {
        if let Some(p) = dir.parent() {
            std::fs::create_dir_all(p)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let mode = std::os::unix::fs::MetadataExt::mode(&meta) & 0o777;
    if mode & 0o077 != 0 {
        // Tighten only; never add permissions the owner removed.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

/// A path next to `path` named `<file name>.<tag>-<random>` that does not exist yet.
fn unique_sibling(path: &Path, tag: &str) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    for _ in 0..16 {
        let p = dir.join(format!("{name}.{tag}-{}", random_hex(4)?));
        if std::fs::symlink_metadata(&p).is_err() {
            return Ok(p);
        }
    }
    Err(std::io::Error::other("could not pick a unique file name"))
}

/// Write `data` to a new uniquely named temp file beside `path` (created
/// exclusively with `mode`), fsync it, then rename it over `path`.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", random_hex(8)?));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let res = f.write_all(data).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = res.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        (t, p)
    }

    #[test]
    fn defaults_are_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn dangerous_values_are_errors_not_clamped() {
        let bad = Config {
            worktree_idle_days: 0,
            stale_hours: 0,
            worktree_target_stale_hours: 0,
            scan_interval_mins: 0,
            auto_reclaim_target_gb: 20,
            low_disk_gb: 25,
            ..Config::default()
        };
        let errs = bad.validate().unwrap_err();
        assert_eq!(errs.len(), 5, "{errs:?}");
        assert!(Config {
            scan_interval_mins: 1441,
            ..Config::default()
        }
        .validate()
        .is_err());
        assert!(Config {
            version: CONFIG_VERSION + 1,
            ..Config::default()
        }
        .validate()
        .is_err());
        assert!(Config {
            scan_roots: vec!["relative".into()],
            ..Config::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn missing_file_loads_defaults_without_error() {
        let (_t, p) = setup();
        let l = Config::load_from(&p);
        assert!(l.error.is_none());
        assert_eq!(l.config, Config::default());
    }

    #[test]
    fn unparsable_file_reports_error_and_turns_automation_off() {
        let (_t, p) = setup();
        std::fs::create_dir_all(&p.config_dir).unwrap();
        std::fs::write(p.config_file(), b"{ \"auto_sweep_worktrees\": true, oops").unwrap();
        let l = Config::load_from(&p);
        assert!(matches!(l.error, Some(ConfigError::Parse(_))));
        assert!(!l.config.auto_sweep_worktrees && !l.config.auto_reclaim);
    }

    #[test]
    fn out_of_range_file_reports_error_and_turns_automation_off() {
        let (_t, p) = setup();
        std::fs::create_dir_all(&p.config_dir).unwrap();
        std::fs::write(
            p.config_file(),
            br#"{"auto_sweep_worktrees": true, "worktree_idle_days": 0}"#,
        )
        .unwrap();
        let l = Config::load_from(&p);
        assert!(matches!(l.error, Some(ConfigError::Invalid(_))));
        assert!(!l.config.auto_sweep_worktrees);
    }

    #[test]
    fn save_never_overwrites_an_invalid_file() {
        let (_t, p) = setup();
        std::fs::create_dir_all(&p.config_dir).unwrap();
        let broken = b"{ this is the user's hand-edited file";
        std::fs::write(p.config_file(), broken).unwrap();
        Config::default().save_to(&p, 1234).unwrap();
        let saved = Config::load_from(&p);
        assert!(saved.error.is_none());
        let aside: Vec<_> = std::fs::read_dir(&p.config_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.json.invalid-1234-")
            })
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read(aside[0].path()).unwrap(), broken);
    }

    #[test]
    fn save_refuses_invalid_config_and_leaves_file_alone() {
        let (_t, p) = setup();
        Config::default().save_to(&p, 1).unwrap();
        let before = std::fs::read(p.config_file()).unwrap();
        let bad = Config {
            worktree_idle_days: 0,
            ..Config::default()
        };
        assert!(matches!(bad.save_to(&p, 2), Err(ConfigError::Invalid(_))));
        assert_eq!(std::fs::read(p.config_file()).unwrap(), before);
    }

    #[test]
    fn save_is_atomic_and_private() {
        use std::os::unix::fs::MetadataExt;
        let (_t, p) = setup();
        let cfg = Config {
            stale_hours: 7,
            ..Config::default()
        };
        cfg.save_to(&p, 1).unwrap();
        let names: Vec<String> = std::fs::read_dir(&p.config_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["config.json".to_string()], "no temp file left");
        let meta = std::fs::metadata(p.config_file()).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&p.config_dir).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(Config::load_from(&p).config.stale_hours, 7);
    }

    #[test]
    fn legacy_file_without_version_loads() {
        let (_t, p) = setup();
        std::fs::create_dir_all(&p.config_dir).unwrap();
        std::fs::write(p.config_file(), br#"{"stale_hours": 5}"#).unwrap();
        let l = Config::load_from(&p);
        assert!(l.error.is_none());
        assert_eq!(l.config.version, CONFIG_VERSION);
        assert_eq!(l.config.stale_hours, 5);
    }

    #[test]
    fn port_override_is_not_part_of_the_config() {
        // effective_port reads the environment; the saved config keeps its own port.
        let (_t, p) = setup();
        let cfg = Config::default();
        cfg.save_to(&p, 1).unwrap();
        let text = std::fs::read_to_string(p.config_file()).unwrap();
        assert!(text.contains("\"api_port\": 47474"));
    }

    #[test]
    fn port_override_from_env_value() {
        let cfg = Config {
            api_port: 7777,
            ..Config::default()
        };
        assert_eq!(port_from(&cfg, None), 7777);
        assert_eq!(port_from(&cfg, Some("8123")), 8123);
        assert_eq!(port_from(&cfg, Some(" 8123 ")), 8123);
        assert_eq!(port_from(&cfg, Some("0")), 7777, "0 is no port");
        assert_eq!(port_from(&cfg, Some("70000")), 7777, "out of range");
        assert_eq!(port_from(&cfg, Some("abc")), 7777);
    }

    #[test]
    fn supervision_sections_default_and_validate() {
        let c = Config::default();
        assert!(c.limits.report_only());
        assert!(c.sessions.enabled && c.sessions.retain_days == 14);
        assert_eq!(c.guard.rules.sudo, RuleMode::Warn);
        assert_eq!(c.updates.check, UpdateCheck::On);
        assert!(!c.updates.advisories, "advisories are opt-in");
        // An old file without the new sections loads with their defaults.
        let old = Config::parse(br#"{"version": 1, "stale_hours": 5}"#).unwrap();
        assert_eq!(old.limits, LimitsConfig::default());
        assert_eq!(old.guard, GuardConfig::default());
        // A budget in JSON.
        let c = Config::parse(
            br#"{"limits": {"default": {"cpu_cores": {"max": 4, "action": "pause"},
                 "memory_mb": {"max": 8000, "sustain_secs": 30}},
                 "workspaces": [{"path": "/w/a", "budget": {"processes": {"max": 50, "action": "stop"}}}]},
                 "guard": {"rules": {"recursive_delete": "block"}, "protected_branches": ["trunk"]},
                 "updates": {"check": "off"}, "agents": {"extra": ["my-agent"]}}"#,
        )
        .unwrap();
        let d = c.limits.default.as_ref().unwrap();
        assert_eq!(d.cpu_cores.unwrap().action, LimitAction::Pause);
        assert_eq!(d.cpu_cores.unwrap().sustain_secs, 60);
        assert_eq!(d.memory_mb.unwrap().action, LimitAction::Notify);
        assert_eq!(d.resume_after_secs, 300);
        assert!(!d.include_agent);
        let w = c.limits.budget_for(Some(Path::new("/w/a/sub"))).unwrap();
        assert!(
            w.cpu_cores.is_none(),
            "an override replaces the default whole"
        );
        assert_eq!(w.processes.unwrap().action, LimitAction::Stop);
        assert_eq!(
            c.limits.budget_for(Some(Path::new("/w/b"))).unwrap(),
            d,
            "elsewhere the default"
        );
        assert_eq!(c.limits.budget_for(None).unwrap(), d);
        assert_eq!(c.guard.rules.recursive_delete, RuleMode::Block);
        assert_eq!(c.guard.rules.sudo, RuleMode::Warn);
        assert_eq!(c.updates.check, UpdateCheck::Off);
        assert!(Config::parse(
            br#"{"limits": {"default": {"cpu_cores": {"max": 4, "action": "explode"}}}}"#
        )
        .is_err());
    }

    #[test]
    fn supervision_ranges_are_checked() {
        let bad_cap = Cap {
            max: -1.0,
            sustain_secs: 100_000,
            action: LimitAction::Stop,
        };
        let mut c = Config::default();
        c.limits.default = Some(Budget {
            cpu_cores: Some(bad_cap),
            resume_after_secs: 1,
            stop_grace_secs: 0,
            ..Budget::default()
        });
        c.limits.workspaces.push(WorkspaceBudget {
            path: "rel".into(),
            budget: Budget::default(),
        });
        c.limits.hog_share_pct = 5.0;
        c.sessions.retain_days = 0;
        c.sessions.max_events = 1;
        c.guard.protected_branches = vec!["".into()];
        c.updates.interval_hours = 1;
        c.agents.extra = vec!["a/b".into()];
        let errs = c.validate().unwrap_err();
        for want in [
            "limits.default.cpu_cores.max",
            "limits.default.cpu_cores.sustain_secs",
            "limits.default.resume_after_secs",
            "limits.default.stop_grace_secs",
            "limits.workspaces[0].path",
            "limits.hog_share_pct",
            "sessions.retain_days",
            "sessions.max_events",
            "guard.protected_branches",
            "updates.interval_hours",
            "agents.extra",
        ] {
            assert!(errs.iter().any(|e| e.contains(want)), "{want}: {errs:?}");
        }
        let nan = Config::parse(br#"{"limits": {"default": {"cpu_cores": {"max": 0}}}}"#);
        assert!(matches!(nan, Err(ConfigError::Invalid(_))));
    }
}
