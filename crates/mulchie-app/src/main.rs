#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod update;
mod view;

use backend::{Backend, Batch, ConnInfo, Mode, Options, PlanRequest, TrayInfo};
use mulchie_core::engine::ProcAction;
use mulchie_core::{human_bytes, ActionResult, Config, Paths, Plan, Request, WorkspaceAction};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, WebviewWindow, WindowEvent};

type B<'a> = tauri::State<'a, Arc<Backend>>;

/// Run blocking backend work off the main thread.
async fn blocking<T: Send + 'static>(
    b: &Arc<Backend>,
    f: impl FnOnce(&Backend) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let b = b.clone();
    tauri::async_runtime::spawn_blocking(move || f(&b))
        .await
        .map_err(|e| format!("the request failed inside Mulchie: {e}"))?
}

#[derive(Serialize)]
struct StateReply {
    conn: ConnInfo,
    /// The slim state (see `view::slim_state`); `None` when no engine answers.
    state: Option<Value>,
    error: Option<String>,
}

#[tauri::command]
async fn get_state(b: B<'_>) -> Result<StateReply, String> {
    blocking(&b, |b| {
        let r = b.state();
        Ok(StateReply {
            conn: b.info(),
            state: r.as_ref().ok().cloned().map(view::slim_state),
            error: r.err(),
        })
    })
    .await
}

#[tauri::command]
async fn get_history(b: B<'_>, since: Option<u64>) -> Result<Vec<Value>, String> {
    blocking(&b, move |b| b.history_since(since.unwrap_or(0))).await
}

#[tauri::command]
async fn get_config(b: B<'_>) -> Result<Value, String> {
    blocking(&b, |b| {
        let mut v = view::config_view(&b.state()?);
        v["editable"] = json!(b.mode() == Mode::Engine);
        v["path"] = json!(b.paths().config_file());
        Ok(v)
    })
    .await
}

#[tauri::command]
async fn get_actions(b: B<'_>) -> Result<Value, String> {
    blocking(&b, |b| Ok(view::actions_view(&b.state()?))).await
}

#[tauri::command]
async fn rescan(b: B<'_>) -> Result<(), String> {
    blocking(&b, |b| b.rescan()).await
}

#[tauri::command]
async fn plan(b: B<'_>, request: PlanRequest) -> Result<Plan, String> {
    blocking(&b, move |b| b.plan(&request)).await
}

#[tauri::command]
async fn execute(b: B<'_>, batch: Batch, items: Vec<Request>) -> Result<Vec<ActionResult>, String> {
    blocking(&b, move |b| b.execute(batch, items)).await
}

#[tauri::command]
async fn prune(b: B<'_>, key: String) -> Result<Value, String> {
    blocking(&b, move |b| b.prune(&key)).await
}

#[tauri::command]
async fn workspace_action(
    b: B<'_>,
    path: String,
    action: WorkspaceAction,
) -> Result<usize, String> {
    blocking(&b, move |b| b.workspace_action(Path::new(&path), action)).await
}

#[tauri::command]
async fn process_action(
    b: B<'_>,
    pid: u32,
    start_time: u64,
    action: ProcAction,
) -> Result<(), String> {
    blocking(&b, move |b| b.process_action(pid, start_time, action)).await
}

#[tauri::command]
async fn set_config(b: B<'_>, config: Config) -> Result<(), String> {
    blocking(&b, move |b| b.set_config(config)).await
}

#[derive(Serialize)]
struct Integration {
    port: Option<u16>,
    mode: Mode,
    token_path: String,
    token: Option<String>,
    token_error: Option<String>,
    cli_path: Option<String>,
    /// `~/.local/bin/mulchie` when it is a link to the bundled CLI.
    cli_linked: Option<String>,
    /// Something else is at `~/.local/bin/mulchie` (a file, or a link
    /// elsewhere): its path and what it is.
    cli_other: Option<String>,
    home: String,
    backups_dir: String,
    version: String,
    updates_configured: bool,
}

/// The CLI shipped inside the app bundle (`Contents/MacOS/mulchie`, from
/// `bundle.externalBin`). The empty build placeholder does not count.
fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("mulchie");
    let m = std::fs::metadata(&p).ok()?;
    (m.is_file() && m.len() > 0).then_some(p)
}

#[derive(Debug, PartialEq)]
enum LinkState {
    Missing,
    /// A link to the bundled CLI.
    Ours,
    OtherLink(PathBuf),
    OtherFile,
}

fn link_state(link: &Path, bundled: Option<&Path>) -> LinkState {
    let Ok(m) = std::fs::symlink_metadata(link) else {
        return LinkState::Missing;
    };
    if !m.file_type().is_symlink() {
        return LinkState::OtherFile;
    }
    let target = std::fs::read_link(link).unwrap_or_default();
    match bundled {
        Some(b) if target == b => LinkState::Ours,
        _ => LinkState::OtherLink(target),
    }
}

fn link_target(paths: &Paths) -> PathBuf {
    paths.home.join(".local/bin/mulchie")
}

#[tauri::command]
fn integration(b: B<'_>, app: AppHandle) -> Integration {
    let paths = b.paths();
    let linked = link_target(paths);
    let token = mulchie_core::api::read_token(paths);
    Integration {
        port: b.info().port,
        mode: b.mode(),
        token_path: mulchie_core::api::token_path_in(paths)
            .display()
            .to_string(),
        token_error: token.as_ref().err().map(|e| e.to_string()),
        token: token.ok(),
        cli_path: bundled_cli().map(|p| p.display().to_string()),
        cli_linked: matches!(
            link_state(&linked, bundled_cli().as_deref()),
            LinkState::Ours
        )
        .then(|| linked.display().to_string()),
        cli_other: match link_state(&linked, bundled_cli().as_deref()) {
            LinkState::OtherFile => Some(format!(
                "{} is a different file, not a link to this app's CLI",
                linked.display()
            )),
            LinkState::OtherLink(t) => {
                Some(format!("{} links to {}", linked.display(), t.display()))
            }
            _ => None,
        },
        home: paths.home.display().to_string(),
        backups_dir: paths.backups_dir.display().to_string(),
        version: app.package_info().version.to_string(),
        updates_configured: app.state::<update::Updates>().configured(),
    }
}

#[tauri::command]
fn install_cli(b: B<'_>) -> Result<String, String> {
    let src = bundled_cli().ok_or("the CLI is not bundled in this build")?;
    let dst = link_target(b.paths());
    std::fs::create_dir_all(dst.parent().ok_or("bad link path")?).map_err(|e| e.to_string())?;
    if let Ok(m) = std::fs::symlink_metadata(&dst) {
        if !m.file_type().is_symlink() {
            return Err(format!(
                "{} exists and is not a link; remove it first",
                dst.display()
            ));
        }
        std::fs::remove_file(&dst).map_err(|e| e.to_string())?;
    }
    std::os::unix::fs::symlink(&src, &dst).map_err(|e| e.to_string())?;
    Ok(dst.display().to_string())
}

/// Paths the UI may open in Finder or Terminal: ones the engine reported, and
/// anything in the backups directory.
fn revealable(state: &Value, p: &Path, backups: &Path) -> bool {
    let is = |v: &Value| v.as_str().is_some_and(|s| Path::new(s) == p);
    let any = |list: &Value, keys: &[&str]| {
        list.as_array()
            .is_some_and(|l| l.iter().any(|x| keys.iter().any(|k| is(&x[*k]))))
    };
    if any(&state["snapshot"]["workspaces"], &["path"])
        || any(&state["scan"]["items"], &["path", "project"])
        || any(&state["diskmap"]["worktrees"], &["path"])
        || any(&state["diskmap"]["categories"], &["path"])
        || p == backups
    {
        return true;
    }
    match (p.canonicalize(), backups.canonicalize()) {
        (Ok(p), Ok(b)) => p.starts_with(b),
        _ => false,
    }
}

#[tauri::command]
async fn reveal(b: B<'_>, path: String, terminal: bool) -> Result<(), String> {
    blocking(&b, move |b| {
        let p = PathBuf::from(&path);
        let st = b.state().unwrap_or(Value::Null);
        if !p.exists() || !revealable(&st, &p, &b.paths().backups_dir) {
            return Err("unknown path".into());
        }
        let mut cmd = std::process::Command::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        });
        if cfg!(target_os = "macos") {
            if terminal {
                cmd.args(["-a", "Terminal"]);
            } else if p.is_file() {
                cmd.arg("-R");
            }
        }
        cmd.arg(&p).spawn().map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
fn open_dashboard(app: AppHandle, view: Option<String>) {
    show_dashboard(&app, view);
}

#[tauri::command]
fn window_visible(window: WebviewWindow) -> bool {
    window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false)
}

/// The panel's height for its content, in logical pixels, within limits that
/// keep it on a small screen.
fn panel_height(want: f64) -> f64 {
    if want.is_finite() {
        want.clamp(320.0, 760.0)
    } else {
        620.0
    }
}

/// Size the tray panel to its content (the panel window only).
#[tauri::command]
fn fit_panel(window: WebviewWindow, height: f64) {
    if window.label() == "panel" {
        let _ = window.set_size(tauri::LogicalSize::new(400.0, panel_height(height)));
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// Tell a window whether it is on screen, so it polls only while it is.
fn set_visible(win: &WebviewWindow, on: bool) {
    let _ = win.emit_to(win.label(), "mulchie://visible", on);
}

fn show_dashboard(app: &AppHandle, view: Option<String>) {
    if let Some(p) = app.get_webview_window("panel") {
        let _ = p.hide();
        set_visible(&p, false);
    }
    let Some(win) = app.get_webview_window("dashboard") else {
        return;
    };
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
    set_visible(&win, true);
    if let Some(v) = view {
        let _ = win.emit_to("dashboard", "mulchie://view", v);
    }
}

fn tray_title(t: &TrayInfo) -> String {
    if t.disk_total == 0 {
        return String::new();
    }
    let mark = match t.health.as_str() {
        "critical" => "‼︎ ",
        "strained" | "stale" => "! ",
        _ => "",
    };
    format!("{mark}{:.0}G · {:.0}%", t.disk_free as f64 / 1e9, t.cpu_pct)
}

fn toggle_panel(app: &AppHandle, rect: Option<tauri::Rect>) {
    let Some(win) = app.get_webview_window("panel") else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        set_visible(&win, false);
        return;
    }
    if let Some(rect) = rect {
        let scale = win.scale_factor().unwrap_or(2.0);
        let pos = rect.position.to_physical::<f64>(scale);
        let size = rect.size.to_physical::<f64>(scale);
        let w = win
            .outer_size()
            .map(|s| s.width as f64)
            .unwrap_or(400.0 * scale);
        let x = pos.x + size.width / 2.0 - w / 2.0;
        let y = pos.y + size.height + 4.0;
        let _ = win.set_position(PhysicalPosition::new(x.max(0.0), y));
    }
    let _ = win.show();
    let _ = win.set_focus();
    set_visible(&win, true);
}

/// Every 3 s: supervise the connection, update the tray, and notify once
/// when free space drops below the alert threshold (engine mode only; a
/// service engine sends its own notifications).
fn tray_loop(backend: Arc<Backend>, tray: tauri::tray::TrayIcon) {
    std::thread::Builder::new()
        .name("mulchie-tray".into())
        .spawn(move || {
            let mut was_low = false;
            loop {
                backend.tick();
                match backend.tray_info() {
                    Ok(t) => {
                        let mode = match backend.mode() {
                            Mode::Client => " (connected to the running engine)",
                            _ => "",
                        };
                        let tip = format!(
                            "Mulchie{mode}: {} free · load {:.0} · {} agents · {} builds",
                            human_bytes(t.disk_free),
                            t.load1,
                            t.agents,
                            t.builds,
                        );
                        let _ = tray.set_title(Some(tray_title(&t)));
                        let _ = tray.set_tooltip(Some(tip));
                        let low = t.low_disk.unwrap_or(false);
                        if low && !was_low {
                            mulchie_core::notify::notify(
                                "Mulchie: disk space low",
                                &format!(
                                    "{} free. Open Mulchie → Disk to reclaim.",
                                    human_bytes(t.disk_free)
                                ),
                            );
                        }
                        was_low = low;
                    }
                    Err(_) => {
                        let _ = tray.set_title(Some("–"));
                        let _ = tray.set_tooltip(Some(
                            "Mulchie: the engine is not reachable; retrying".to_string(),
                        ));
                    }
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
        .expect("start the tray thread");
}

fn main() {
    let backend = Arc::new(Backend::start(Paths::from_env(), Options::default()));
    match backend.info() {
        ConnInfo {
            mode: Mode::Client,
            engine_pid,
            port,
            ..
        } => eprintln!(
            "mulchie: another engine runs (pid {engine_pid:?}, port {port:?}); this app is its client"
        ),
        ConnInfo {
            mode: Mode::Disconnected,
            detail,
            ..
        } => eprintln!("mulchie: no engine reachable yet: {}", detail.unwrap_or_default()),
        ConnInfo { detail: Some(d), .. } => eprintln!("mulchie: {d}"),
        _ => {}
    }

    let context = tauri::generate_context!();
    let updates = update::Updates::from_plugins(&context.config().plugins.0);
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());
    if updates.configured() {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }
    let exit_backend = backend.clone();
    let app = builder
        .manage(backend.clone())
        .manage(updates)
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_history,
            get_config,
            get_actions,
            rescan,
            plan,
            execute,
            prune,
            workspace_action,
            process_action,
            set_config,
            integration,
            install_cli,
            reveal,
            open_dashboard,
            window_visible,
            fit_panel,
            quit
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let check = MenuItem::with_id(app, "update", "Check for Updates…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Mulchie", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &check, &sep, &quit])?;

            let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let tray = TrayIconBuilder::with_id("mulchie")
                .icon(icon)
                .icon_as_template(true)
                .tooltip("Mulchie")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "open" => show_dashboard(app, None),
                    "update" => update::check(app.clone()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_panel(tray.app_handle(), Some(rect));
                    }
                })
                .build(app)?;
            tray_loop(backend.clone(), tray);

            if let Some(win) = app.get_webview_window("panel") {
                let w = win.clone();
                win.on_window_event(move |e| {
                    if let WindowEvent::Focused(false) = e {
                        let _ = w.hide();
                        set_visible(&w, false);
                    }
                });
            }
            if let Some(win) = app.get_webview_window("dashboard") {
                let w = win.clone();
                let handle = app.handle().clone();
                win.on_window_event(move |e| {
                    if let WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        let _ = w.hide();
                        set_visible(&w, false);
                        #[cfg(target_os = "macos")]
                        let _ = handle.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    }
                });
            }
            Ok(())
        })
        .build(context)
        .expect("failed to build Mulchie");
    // `--dashboard` opens the dashboard at launch instead of staying in the
    // menu bar.
    let open_at_launch = std::env::args().any(|a| a == "--dashboard");
    app.run(move |app, event| match event {
        RunEvent::Ready if open_at_launch => show_dashboard(app, None),
        RunEvent::Exit => exit_backend.shutdown(),
        // Opening Mulchie again (Finder, Spotlight, Dock) while it runs.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_dashboard(app, None),
        _ => {}
    });
}

#[cfg(test)]
mod tests;
