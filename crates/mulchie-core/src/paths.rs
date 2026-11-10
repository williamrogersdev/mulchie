//! Every filesystem location Mulchie's logic uses, resolved once at the edge.
//!
//! Logic takes a `&Paths` instead of reading `HOME` or `XDG_CONFIG_HOME` itself,
//! so tests can point everything at a temporary directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The user's home directory. Child `git` processes get it as `HOME`.
    pub home: PathBuf,
    /// `~/.config/mulchie` (or `$XDG_CONFIG_HOME/mulchie`).
    pub config_dir: PathBuf,
    /// Where worktree backups are written (mode 0700).
    pub backups_dir: PathBuf,
    /// Scratch space for short-lived files that need no privacy.
    pub temp_dir: PathBuf,
    /// `XDG_CONFIG_HOME` as the process saw it, passed on to `git`. `None` removes
    /// it from git's environment so git reads only `home`.
    pub xdg_config_home: Option<PathBuf>,
}

impl Paths {
    /// Resolve from the process environment. Only entry points call this.
    pub fn from_env() -> Self {
        let home = home_dir();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let config_dir = match &xdg {
            Some(x) => x.join("mulchie"),
            None => home.join(".config").join("mulchie"),
        };
        Self {
            backups_dir: config_dir.join("backups"),
            config_dir,
            temp_dir: std::env::temp_dir(),
            xdg_config_home: xdg,
            home,
        }
    }

    /// Everything under one directory: `<root>/home`, `<root>/config`,
    /// `<root>/config/backups`, `<root>/tmp`. Used by tests.
    pub fn under(root: &Path) -> Self {
        let config_dir = root.join("config");
        Self {
            home: root.join("home"),
            backups_dir: config_dir.join("backups"),
            config_dir,
            temp_dir: root.join("tmp"),
            xdg_config_home: None,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn journal_file(&self) -> PathBuf {
        self.config_dir.join("actions.jsonl")
    }

    pub fn derived_data(&self) -> PathBuf {
        self.home.join("Library/Developer/Xcode/DerivedData")
    }
}

/// `$HOME`, or `/` when unset. Entry points only; logic uses [`Paths::home`].
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The config directory from the environment. Entry points only.
pub fn config_dir() -> PathBuf {
    Paths::from_env().config_dir
}

/// `n` random bytes as lowercase hex, from `/dev/urandom`.
pub fn random_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}
