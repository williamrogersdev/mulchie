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
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(e) => write!(f, "config file could not be parsed: {e}"),
            ConfigError::Invalid(v) => write!(f, "config has invalid values: {}", v.join("; ")),
            ConfigError::Io(e) => write!(f, "config file error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

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
    std::env::var("MULCHIE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
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
}
