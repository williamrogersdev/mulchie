use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
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
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir();
        let mut roots = Vec::new();
        for name in ["Developer", "Projects", "code", "src", "dev", "repos", "work"] {
            let p = home.join(name);
            if p.is_dir() {
                roots.push(p);
            }
        }
        if roots.is_empty() {
            roots.push(home.clone());
        }
        Self {
            scan_roots: roots,
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
        }
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("mulchie");
    }
    home_dir().join(".config").join("mulchie")
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let mut cfg = Self::load_from(&Self::path());
        if let Some(port) = std::env::var("MULCHIE_PORT").ok().and_then(|p| p.parse().ok()) {
            cfg.api_port = port;
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, data)
    }
}
