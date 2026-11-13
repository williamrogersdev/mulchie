pub mod api;
pub mod config;
pub mod consumers;
pub mod diskmap;
pub mod engine;
pub mod failures;
pub mod git;
pub mod guard;
pub mod journal;
pub mod limits;
pub mod lock;
pub mod memguard;
pub mod monitor;
pub mod notify;
pub mod paths;
pub mod process;
pub mod procplan;
pub mod reclaim;
pub mod record;
pub mod redact;
pub mod safety;
pub mod session;
pub mod shutdown;
pub mod updates;
pub mod watch;

pub use config::{Config, ConfigError, LoadedConfig};
pub use engine::{Engine, Health, LeaseRequest, State, WorkspaceAction};
pub use monitor::{Monitor, Snapshot};
pub use paths::Paths;
pub use reclaim::ScanReport;
pub use safety::{ActionResult, Gate, Outcome, Plan, PlanItem, Request, SkipReason};

pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e12 {
        format!("{:.1} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}
