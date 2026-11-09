#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mulchie_core::diskmap::{PruneResult, RemoveResult, Worktree};
use mulchie_core::engine::{Advice, ProcAction, Sample};
use mulchie_core::{human_bytes, CleanResult, Config, Engine, Health, State, WorkspaceAction};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WindowEvent};

#[tauri::command]
fn get_state(engine: tauri::State<Engine>) -> State {
    engine.snapshot_state()
}

#[tauri::command]
fn get_history(engine: tauri::State<Engine>) -> Vec<Sample> {
    engine.history()
}

#[tauri::command]
fn get_advice(engine: tauri::State<Engine>) -> Advice {
    engine.advice()
}

#[tauri::command]
fn rescan(engine: tauri::State<Engine>) {
    engine.request_scan();
}

#[tauri::command]
fn clean(engine: tauri::State<Engine>, paths: Vec<String>) -> Vec<CleanResult> {
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    engine.clean_paths(&paths, "manual")
}

#[tauri::command]
fn clean_stale(engine: tauri::State<Engine>) -> Vec<CleanResult> {
    engine.reclaim_stale(0, "manual")
}

#[tauri::command]
fn set_config(engine: tauri::State<Engine>, config: Config) -> Result<(), String> {
    engine.set_config(config).map_err(|e| e.to_string())
}

#[tauri::command]
fn workspace_action(
    engine: tauri::State<Engine>,
    path: String,
    action: WorkspaceAction,
) -> Result<usize, String> {
    engine.workspace_action(std::path::Path::new(&path), action, "manual")
}

#[tauri::command]
fn process_action(
    engine: tauri::State<Engine>,
    pid: u32,
    action: ProcAction,
) -> Result<(), String> {
    engine.process_action(pid, action, "manual")
}

#[tauri::command]
fn remove_worktrees(
    engine: tauri::State<Engine>,
    paths: Vec<String>,
    backup: bool,
) -> Vec<RemoveResult> {
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    engine.remove_worktrees(&paths, backup, "manual")
}

#[tauri::command]
fn sweep_plan(engine: tauri::State<Engine>) -> Vec<Worktree> {
    engine.sweep_plan()
}

#[tauri::command]
fn sweep(engine: tauri::State<Engine>) -> Vec<RemoveResult> {
    engine.sweep_worktrees("manual")
}

#[tauri::command]
fn prune(engine: tauri::State<Engine>, key: String) -> PruneResult {
    engine.prune_cache(&key, "manual")
}

#[derive(Serialize)]
struct Integration {
    port: u16,
    token_path: String,
    token: String,
    cli_path: Option<String>,
    cli_linked: Option<String>,
}

fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("mulchie");
    p.is_file().then_some(p)
}

fn link_target() -> PathBuf {
    mulchie_core::config::home_dir().join(".local/bin/mulchie")
}

#[tauri::command]
fn integration(engine: tauri::State<Engine>) -> Integration {
    let linked = link_target();
    Integration {
        port: engine.config().api_port,
        token_path: mulchie_core::api::token_path().display().to_string(),
        token: mulchie_core::api::load_or_create_token().unwrap_or_default(),
        cli_path: bundled_cli().map(|p| p.display().to_string()),
        cli_linked: linked.exists().then(|| linked.display().to_string()),
    }
}

#[tauri::command]
fn install_cli() -> Result<String, String> {
    let src = bundled_cli().ok_or("the CLI is not bundled in this build")?;
    let dst = link_target();
    std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
    if std::fs::symlink_metadata(&dst).is_ok() {
        std::fs::remove_file(&dst).map_err(|e| e.to_string())?;
    }
    std::os::unix::fs::symlink(&src, &dst).map_err(|e| e.to_string())?;
    Ok(dst.display().to_string())
}

#[tauri::command]
fn reveal(engine: tauri::State<Engine>, path: String, terminal: bool) -> Result<(), String> {
    let st = engine.snapshot_state();
    let p = PathBuf::from(&path);
    let known = st.snapshot.workspaces.iter().any(|w| w.path == p)
        || st
            .scan
            .as_ref()
            .map(|s| s.items.iter().any(|i| i.path == p || i.project == p))
            .unwrap_or(false)
        || st
            .diskmap
            .as_ref()
            .map(|m| {
                m.worktrees.iter().any(|w| w.path == p) || m.categories.iter().any(|c| c.path == p)
            })
            .unwrap_or(false)
        || p == mulchie_core::config::config_dir().join("backups");
    if !known || !p.exists() {
        return Err("unknown path".into());
    }
    let mut cmd = std::process::Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    if terminal && cfg!(target_os = "macos") {
        cmd.args(["-a", "Terminal"]);
    }
    cmd.arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_dashboard(app: AppHandle, view: Option<String>) {
    show_dashboard(&app, view);
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

fn show_dashboard(app: &AppHandle, view: Option<String>) {
    if let Some(p) = app.get_webview_window("panel") {
        let _ = p.hide();
    }
    let Some(win) = app.get_webview_window("dashboard") else {
        return;
    };
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
    if let Some(v) = view {
        let _ = win.emit("mulchie://view", v);
    }
}

fn tray_title(st: &State) -> String {
    let s = &st.snapshot;
    if s.disk_total == 0 {
        return String::new();
    }
    let mark = match st.health {
        Health::Critical => "‼︎ ",
        Health::Strained => "! ",
        _ => "",
    };
    format!("{mark}{:.0}G · {:.0}%", s.disk_free as f64 / 1e9, s.cpu_pct)
}

fn toggle_panel(app: &AppHandle, rect: Option<tauri::Rect>) {
    let Some(win) = app.get_webview_window("panel") else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
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
}

fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(title)
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn();
    }
}

fn main() {
    let config = Config::load();
    let port = config.api_port;
    let engine = Engine::new(config);
    engine.start();
    let api_engine = engine.clone();

    tauri::Builder::default()
        .manage(engine.clone())
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_history,
            get_advice,
            rescan,
            clean,
            clean_stale,
            set_config,
            workspace_action,
            process_action,
            remove_worktrees,
            sweep_plan,
            sweep,
            prune,
            integration,
            install_cli,
            reveal,
            open_dashboard,
            quit
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Err(e) = mulchie_core::api::serve(api_engine.clone(), port) {
                eprintln!("mulchie api not started on {port}: {e}");
            }

            let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let tray = TrayIconBuilder::with_id("mulchie")
                .icon(icon)
                .icon_as_template(true)
                .tooltip("Mulchie")
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

            let engine = engine.clone();
            std::thread::spawn(move || {
                let mut was_low = false;
                loop {
                    let st = engine.snapshot_state();
                    let tip = format!(
                        "Mulchie — {} free · load {:.0} · {} agents · {} builds",
                        human_bytes(st.snapshot.disk_free),
                        st.snapshot.load[0],
                        st.snapshot.agent_count,
                        st.snapshot.build_count,
                    );
                    let _ = tray.set_title(Some(tray_title(&st)));
                    let _ = tray.set_tooltip(Some(tip));
                    if st.low_disk && !was_low {
                        notify(
                            "Mulchie: disk space low",
                            &format!(
                                "{} free. Open Mulchie → Disk to reclaim.",
                                human_bytes(st.snapshot.disk_free)
                            ),
                        );
                    }
                    was_low = st.low_disk;
                    std::thread::sleep(Duration::from_secs(3));
                }
            });

            if let Some(win) = app.get_webview_window("panel") {
                let w = win.clone();
                win.on_window_event(move |e| {
                    if let WindowEvent::Focused(false) = e {
                        let _ = w.hide();
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
                        #[cfg(target_os = "macos")]
                        let _ = handle.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run Mulchie");
}
