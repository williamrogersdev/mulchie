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

/// Where released apps look for `latest.json`.
pub const ENDPOINT: &str =
    "https://github.com/williamrogersdev/mulchie/releases/latest/download/latest.json";
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

/// Whether this build can update itself, and whether a check is running.
pub struct Updates {
    configured: bool,
    busy: AtomicBool,
}

impl Updates {
    pub fn from_plugins(plugins: &HashMap<String, serde_json::Value>) -> Self {
        Self {
            configured: pubkey(plugins).is_some(),
            busy: AtomicBool::new(false),
        }
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

/// "Check for Updates…" from the tray menu. One check at a time.
pub fn check(app: AppHandle) {
    let updates = app.state::<Updates>();
    if !updates.configured {
        tell(
            &app,
            "Updates are not configured",
            format!(
                "This build of Mulchie has no update key, so it cannot update itself.\n\nDownload new versions from {RELEASES}."
            ),
            MessageDialogKind::Info,
        );
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
    let found = async {
        let url = tauri::Url::parse(ENDPOINT).map_err(|e| e.to_string())?;
        app.updater_builder()
            .endpoints(vec![url])
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| e.to_string())
    }
    .await;
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
