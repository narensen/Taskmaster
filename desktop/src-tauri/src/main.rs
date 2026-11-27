//! Taskmaster for the desktop: start tm-daemon (the bundled sidecar) on this machine, open
//! its app in a window already signed in, and stop the daemon when the window closes.
//!
//! The window has no system title bar (macOS keeps its traffic lights): the web app draws
//! its own, with its own window controls, and sends desktop notifications through the
//! notification plugin.
//!
//! Updates come from GitHub Releases through the Tauri updater: `check_update` reads
//! `latest.json`, and `install_update` downloads the installer, checks its signature
//! against the public key built into the app, installs it, hands the daemon's running
//! builders to the next daemon, and restarts.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The port the daemon tries first, so the window's address (and what it keeps, such
/// as the theme) is the same every launch.
const PREFERRED_PORT: u16 = 47737;

#[derive(Default)]
struct AppState {
    daemon: Mutex<Option<tm_launcher::Daemon>>,
    update: Mutex<Option<Update>>,
}

#[derive(Serialize)]
struct UpdateInfo {
    version: String,
    current: String,
    date: Option<String>,
    notes: Option<String>,
}

#[tauri::command]
async fn check_update(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<Option<UpdateInfo>, String> {
    let found = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let Some(update) = found else { return Ok(None) };
    let info = UpdateInfo {
        version: update.version.clone(),
        current: update.current_version.clone(),
        date: update.date.map(|d| d.to_string()),
        notes: update.body.clone(),
    };
    *state.update.lock().unwrap() = Some(update);
    Ok(Some(info))
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let update = state.update.lock().unwrap().take().ok_or("check for updates first")?;
    // The plugin refuses an installer whose signature does not match the built-in key.
    update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    // Builders at work go on in the holder; the restarted app's daemon picks them up (H3).
    let daemon = state.daemon.lock().unwrap().take();
    if let Some(daemon) = daemon {
        daemon.hand_off();
    }
    app.restart();
}

#[tauri::command]
fn app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// The sidecar sits next to the app's own executable.
fn sidecar() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = if cfg!(windows) { "tm-daemon.exe" } else { "tm-daemon" };
    Ok(exe.parent().ok_or("the app has no folder")?.join(name))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![check_update, install_update, app_version])
        .setup(|app| {
            let program = sidecar()?;
            let web = app.path().resource_dir()?.join("web");
            let project = std::env::var_os("TASKMASTER_PROJECT").map(PathBuf::from).unwrap_or_else(tm_launcher::default_project);
            let daemon = tm_launcher::start(&tm_launcher::Launch {
                program: &program,
                project: &project,
                web: web.join("index.html").is_file().then_some(web.as_path()),
                port: Some(PREFERRED_PORT),
            })?;
            // The window opens signed in: the token goes where the app keeps it.
            let script = format!(
                "try {{ localStorage.setItem('taskmaster.token', {}); localStorage.removeItem('taskmaster.server'); }} catch (e) {{}}",
                serde_json::to_string(&daemon.token)?
            );
            let url = format!("{}/", daemon.url).parse()?;
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("Taskmaster")
                .inner_size(1360.0, 860.0)
                .min_inner_size(880.0, 600.0)
                .zoom_hotkeys_enabled(false)
                .initialization_script(&script);
            // The app draws its own title bar on the same surface as everything else. macOS
            // keeps its traffic lights, over the app; elsewhere the app draws the controls.
            #[cfg(target_os = "macos")]
            let window = window.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true);
            #[cfg(not(target_os = "macos"))]
            let window = window.decorations(false);
            window.build()?;
            *app.state::<AppState>().daemon.lock().unwrap() = Some(daemon);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Taskmaster could not start")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // Stop the daemon and everything it started with the app.
                app.state::<AppState>().daemon.lock().unwrap().take();
            }
        });
}
