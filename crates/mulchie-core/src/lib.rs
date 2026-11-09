pub mod api;
pub mod config;
pub mod diskmap;
pub mod engine;
pub mod monitor;
pub mod reclaim;

pub use config::Config;
pub use engine::{Engine, Health, State, WorkspaceAction};
pub use monitor::{Monitor, Snapshot};
pub use reclaim::{CleanResult, ScanReport};

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
