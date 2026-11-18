//! In-app updates through `tauri-plugin-updater`.
//!
//! The plugin is registered only when `plugins.updater.pubkey` in
//! `tauri.conf.json` is set (`scripts/release-setup.sh` writes it). Without a
//! key the app builds and runs normally and "Check for Updates…" says that
//! updates are not configured in this build.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

pub const RELEASES: &str = "https://github.com/williamrogersdev/mulchie/releases";

/// The updater public key from the `plugins` section of the Tauri config, if
/// one is set.
pub fn pubkey(plugins: &HashMap<String, serde_json::Value>) -> Option<String> {
    plugins
        .get("updater")?
        .get("pubkey")?
        .as_str()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
}

/// How often the app looks for a newer version of itself by itself.
const BACKGROUND_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
/// The first look waits this long after launch.
const BACKGROUND_FIRST: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether this build can update itself, whether a check is running, and
/// the newer version the last background check found.
pub struct Updates {
    configured: bool,
    busy: AtomicBool,
    available: std::sync::Mutex<Option<String>>,
}

impl Updates {
    pub fn from_plugins(plugins: &HashMap<String, serde_json::Value>) -> Self {
        Self {
            configured: pubkey(plugins).is_some(),
            busy: AtomicBool::new(false),
            available: std::sync::Mutex::new(None),
        }
    }

    /// The newer version the updater found, if any.
    pub fn available(&self) -> Option<String> {
        self.available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_available(&self, v: Option<String>) {
        *self.available.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }

    pub fn configured(&self) -> bool {
        self.configured
    }
}

fn tell(app: &AppHandle, title: &str, msg: String, kind: MessageDialogKind) {
    app.dialog()
        .message(msg)
        .title(title)
        .kind(kind)
        .show(|_| {});
}

async fn ask(app: &AppHandle, title: String, msg: String, yes: &str, no: &str) -> bool {
    let d = app
        .dialog()
        .message(msg)
        .title(title)
        .buttons(MessageDialogButtons::OkCancelCustom(yes.into(), no.into()));
    tauri::async_runtime::spawn_blocking(move || d.blocking_show())
        .await
        .unwrap_or(false)
}

/// What the window shows about updates: this version, a newer one if one is
/// known, and whether this build can install it itself. A build without the
/// updater takes the newer version from the engine's own update check
/// (`engine_report` is the `report` of `GET /v1/updates`).
pub fn status(app: &AppHandle, engine_report: Option<&serde_json::Value>) -> serde_json::Value {
    let updates = app.state::<Updates>();
    let from_engine = || {
        let m = engine_report?.get("mulchie")?;
        (m.get("outdated")?.as_bool()?)
            .then(|| m.get("latest")?.as_str().map(String::from))
            .flatten()
    };
    let available = if updates.configured {
        updates.available().or_else(from_engine)
    } else {
        from_engine()
    };
    serde_json::json!({
        "current": app.package_info().version.to_string(),
        "available": available,
        "can_install": updates.configured,
    })
}

async fn find(app: &AppHandle) -> Result<Option<tauri_plugin_updater::Update>, String> {
    // `releases/latest` skips pre-releases, so ask which release carries the
    // manifest this build may install (see `app_manifest_url`).
    let current = app.package_info().version.to_string();
    let manifest = tauri::async_runtime::spawn_blocking(move || {
        let http = mulchie_core::updates::default_http();
        mulchie_core::updates::app_manifest_url(http.as_ref(), &current)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Could not reach GitHub to list Mulchie releases ({e})."))?;
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let url = tauri::Url::parse(&manifest).map_err(|e| e.to_string())?;
    app.updater_builder()
        .endpoints(vec![url])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("The update manifest at {manifest} could not be read ({e})."))
}

/// Look for a newer version now and then, without asking anything: the
/// window shows an update button when one is found. Only in a build that
/// can update itself.
pub fn watch(app: AppHandle) {
    if !app.state::<Updates>().configured {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("mulchie-update-watch".into())
        .spawn(move || {
            std::thread::sleep(BACKGROUND_FIRST);
            loop {
                if let Ok(found) = tauri::async_runtime::block_on(find(&app)) {
                    app.state::<Updates>()
                        .set_available(found.map(|u| u.version));
                }
                std::thread::sleep(BACKGROUND_EVERY);
            }
        });
}

fn open_releases() {
    let _ = std::process::Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    })
    .arg(RELEASES)
    .spawn();
}

/// "Check for Updates…" from the app menu, the tray menu or the window's
/// update button. One check at a time.
pub fn check(app: AppHandle, engine_report: Option<serde_json::Value>) {
    let updates = app.state::<Updates>();
    if !updates.configured {
        // This build cannot install anything. Say what the engine's own
        // check knows and offer the download page.
        let st = status(&app, engine_report.as_ref());
        let current = st["current"].as_str().unwrap_or("?").to_string();
        let (title, msg) = match st["available"].as_str() {
            Some(v) => (
                format!("Mulchie {v} is available"),
                format!("You have {current}. This build of Mulchie cannot install updates itself.\n\nWith Homebrew: brew upgrade --cask mulchie\nOr download it from the releases page."),
            ),
            None => (
                "This build cannot update itself".to_string(),
                format!("You have Mulchie {current}. This build has no update key, so it cannot check for or install updates itself.\n\nNew versions are on the releases page."),
            ),
        };
        tauri::async_runtime::spawn(async move {
            if ask(&app, title, msg, "Open Releases Page", "Close").await {
                open_releases();
            }
        });
        return;
    }
    if updates.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        run(&app).await;
        app.state::<Updates>().busy.store(false, Ordering::SeqCst);
    });
}

async fn run(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    let found = find(app).await;
    if let Ok(f) = &found {
        app.state::<Updates>()
            .set_available(f.as_ref().map(|u| u.version.clone()));
    }
    let update = match found {
        Ok(Some(u)) => u,
        Ok(None) => {
            return tell(
                app,
                "Mulchie is up to date",
                format!("Version {current} is the newest version."),
                MessageDialogKind::Info,
            )
        }
        Err(e) => {
            return tell(
                app,
                "Could not check for updates",
                format!("{e}\n\nYou can download new versions from {RELEASES}."),
                MessageDialogKind::Warning,
            )
        }
    };
    let notes = update
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| format!("\n\n{b}"))
        .unwrap_or_default();
    let go = ask(
        app,
        format!("Mulchie {} is available", update.version),
        format!(
            "You have {current}. Install {} and relaunch Mulchie?{notes}",
            update.version
        ),
        "Install and Relaunch",
        "Later",
    )
    .await;
    if !go {
        return;
    }
    match update.download_and_install(|_, _| {}, || {}).await {
        Ok(()) => {
            if let Some(b) = app.try_state::<std::sync::Arc<crate::backend::Backend>>() {
                b.shutdown();
            }
            app.restart();
        }
        Err(e) => tell(
            app,
            "The update was not installed",
            format!("{e}\n\nMulchie {current} keeps running. You can download the new version from {RELEASES}."),
            MessageDialogKind::Error,
        ),
    }
}
