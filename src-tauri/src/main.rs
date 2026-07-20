// Prevents an additional console window on Windows in release. DO NOT REMOVE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pty;
mod sessions;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};

use pty::{
    close_pty, open_pty, resize_pty, running_ptys, set_claude_path, sweep_incognito_dirs,
    write_pty, PtyState,
};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

/// Persist the window's size/position via the window-state plugin, but only while
/// the main window is visible — Mandor hides to tray instead of closing, and a
/// hidden window reports stale geometry that would overwrite the good state.
fn save_window_geometry(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = app.save_window_state(StateFlags::all());
        }
    }
}
use sessions::{list_sessions, session_pr};

/// GUI launches (from a .desktop entry) don't inherit the shell's PATH, so tools
/// installed under e.g. ~/.local/bin (`claude`) or version managers aren't found
/// and spawning fails with "No such file or directory". Merge the login shell's
/// PATH — plus a few common user bin dirs as a fallback — into this process so
/// spawned children (the interactive `claude` PTY) resolve the same as in a
/// terminal.
#[cfg(unix)]
fn ensure_tools_on_path() {
    use std::collections::HashSet;
    use std::process::{Command, Stdio};

    let home = std::env::var("HOME").unwrap_or_default();
    let current = std::env::var("PATH").unwrap_or_default();

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // Markers isolate PATH from any banner an interactive rc file might print.
    let shell_path = Command::new(&shell)
        .args(["-lic", "printf __MP__%s__MP__ \"$PATH\""])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .and_then(|s| {
            let start = s.find("__MP__")? + 6;
            let end = s[start..].find("__MP__")? + start;
            Some(s[start..end].to_string())
        })
        .unwrap_or_default();

    let extras = [
        format!("{home}/.local/bin"),
        format!("{home}/.bun/bin"),
        format!("{home}/.npm-global/bin"),
        "/usr/local/bin".to_string(),
    ];

    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for p in shell_path
        .split(':')
        .map(str::to_string)
        .chain(extras)
        .chain(current.split(':').map(str::to_string))
    {
        if !p.is_empty() && seen.insert(p.clone()) {
            parts.push(p);
        }
    }
    if !parts.is_empty() {
        std::env::set_var("PATH", parts.join(":"));
    }
}

/// Hide the window to the tray (keep-alive). Called from the custom titlebar's
/// close button — a direct command, so it doesn't depend on the OS close events
/// that Wayland drops after a hide/show cycle.
#[tauri::command]
fn hide_to_tray(window: tauri::WebviewWindow) {
    save_window_geometry(window.app_handle()); // capture position before hiding
    let _ = window.hide();
}

/// Really quit (kills sessions via the ExitRequested handler) — from the app menu.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    save_window_geometry(&app);
    app.exit(0);
}

#[tauri::command]
fn minimize_window(window: tauri::WebviewWindow) {
    let _ = window.minimize();
}

#[tauri::command]
fn toggle_maximize(window: tauri::WebviewWindow) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

#[tauri::command]
fn is_maximized(window: tauri::WebviewWindow) -> bool {
    window.is_maximized().unwrap_or(false)
}

/// True in dev (debug) builds — the frontend uses this to tint dev-only UI.
#[tauri::command]
fn is_dev() -> bool {
    cfg!(debug_assertions)
}

/// The executable path + its mtime captured at launch. There's no auto-updater;
/// installing a new .deb (`dpkg -i`) replaces the binary on disk while the old
/// process keeps running, so a newer mtime means an update is waiting for a
/// restart — the frontend shows a "Restart to update" prompt.
struct AppStartup {
    exe: Option<PathBuf>,
    start_mtime: Option<SystemTime>,
}

/// True once the on-disk executable has been replaced since launch. Off in dev
/// (the binary is rebuilt constantly there, which isn't a user-facing update).
#[tauri::command]
async fn update_available(startup: tauri::State<'_, AppStartup>) -> Result<bool, String> {
    if cfg!(debug_assertions) {
        return Ok(false);
    }
    let exe = startup.exe.clone();
    let start = startup.start_mtime;
    tauri::async_runtime::spawn_blocking(move || {
        let (Some(exe), Some(start)) = (exe, start) else {
            return false;
        };
        // dpkg preserves the .deb's build-time mtime, so any change (not just a
        // newer time) means a different binary is on disk — treat that as an update.
        std::fs::metadata(&exe)
            .and_then(|m| m.modified())
            .map(|now| now != start)
            .unwrap_or(false)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Relaunch the app (from the "Restart to update" prompt). Live PTYs are killed
/// via the ExitRequested handler; persisted sessions come back cold, resumable.
#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    save_window_geometry(&app);
    app.restart();
}

/// Installed monospace font families, for the terminal-font picker. Uses fontdb
/// (pure-Rust, cross-platform) so it works the same on Linux, macOS, and Windows
/// without shelling out to fontconfig. The blocking scan runs off the main thread.
#[tauri::command]
async fn list_fonts() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        use std::collections::BTreeSet;
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let mut families = BTreeSet::new();
        for face in db.faces() {
            if !face.monospaced {
                continue;
            }
            if let Some((name, _)) = face.families.first() {
                let name = name.trim();
                if !name.is_empty() {
                    families.insert(name.to_string());
                }
            }
        }
        families.into_iter().collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())
}

/// App metadata for an About view.
#[tauri::command]
fn app_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
    })
}

/// Send a desktop notification via notify-rust (the same path `notify-send`
/// uses). tauri-plugin-notification's show() returns Ok on Linux here but never
/// raises a banner, so we bypass it.
fn show_notification(title: &str, body: &str) -> Result<(), String> {
    // App name is deliberately NOT "Mandor": GNOME resolves that to the running
    // Mandor.desktop and then suppresses its banners (shows them only in the
    // tray). A name with no matching .desktop is treated as a generic
    // notification and always banners — the icon still ties it to Mandor.
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .icon("mandor")
        .appname("Mandor Sessions")
        .show()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Show a desktop notification (used when a session wants attention while the
/// window is unfocused).
#[tauri::command]
fn notify(title: String, body: String) -> Result<(), String> {
    show_notification(&title, &body)
}

/// Read a small audio file as base64 so the webview can play it via a data: URI
/// (avoids asset-protocol scope config for an arbitrary user-picked path).
#[tauri::command]
async fn read_audio_data(path: String) -> Result<String, String> {
    use base64::Engine as _;
    tauri::async_runtime::spawn_blocking(move || {
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if meta.len() > 5_000_000 {
            return Err("audio file too large (max 5 MB)".into());
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Open an http(s) URL in the default browser (used for a session's PR link).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("refusing to open non-http url".into());
    }
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(&url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start` is a cmd builtin; the empty "" is the window-title arg it expects.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", &url]);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Open (or focus) a separate window showing a single session's terminal. The
/// window reconnects to the already-running PTY by id; label is `popout-<id>`.
#[tauri::command]
fn open_session_window(app: tauri::AppHandle, id: String, name: String) -> Result<(), String> {
    let label = format!("popout-{id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return Ok(());
    }
    // `id` is a UUID (url-safe); the human name only goes in the window title.
    let url = format!("index.html?popout=1&id={id}");
    tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
        .title(name)
        .inner_size(1000.0, 700.0)
        .min_inner_size(600.0, 400.0)
        .decorations(false) // use the same custom titlebar as the main window
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reveal and focus the main window (from the tray or a second-instance launch).
fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    #[cfg(unix)]
    ensure_tools_on_path();

    // Clear any incognito config dirs left by a previous crash (clean exits delete
    // their own; nothing incognito survives a restart).
    sweep_incognito_dirs();

    // Dev (debug) builds run under a distinct program name so GNOME/Wayland
    // treats them as a separate app in the taskbar (and doesn't paint them with
    // an installed release build's icon). The app-id / WM_CLASS is derived from
    // the program name at GTK init, so this must run before Tauri.
    #[cfg(all(debug_assertions, target_os = "linux"))]
    glib::set_prgname(Some("mandor-dev"));

    tauri::Builder::default()
        // Must be first: focus the existing window instead of launching a duplicate.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // Optional "launch Mandor at login" (toggled from Settings → Behavior).
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Remember the main window's size/position across launches.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(PtyState::default())
        .manage(AppStartup {
            exe: std::env::current_exe().ok(),
            start_mtime: std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
        })
        .setup(|app| {
            // Tray icon: closing the window hides to tray (sessions keep running);
            // Quit really exits (killing sessions via the ExitRequested handler).
            let show_i = MenuItem::with_id(app, "show", "Show Mandor", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Mandor", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("no window icon")?)
                .tooltip("Mandor")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        save_window_geometry(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // Intercept the window close button: hide to tray instead of quitting,
            // with a one-time notification so it's clear the app is still alive.
            if let Some(win) = app.get_webview_window("main") {
                let win_hide = win.clone();
                let notified = Arc::new(AtomicBool::new(false));
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        // Fallback for OS-level close (e.g. Alt+F4): keep the app alive
                        // and hide to tray. The primary close path is the custom
                        // titlebar button (hide_to_tray command). Deferred so hide()
                        // takes effect on GTK.
                        api.prevent_close();
                        save_window_geometry(win_hide.app_handle()); // before hiding
                        let w = win_hide.clone();
                        let _ = win_hide.app_handle().run_on_main_thread(move || {
                            let _ = w.hide();
                        });
                        if !notified.swap(true, Ordering::Relaxed) {
                            let _ = show_notification(
                                "Mandor is still running",
                                "Your sessions keep running in the background. Reopen or quit from the tray icon.",
                            );
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            hide_to_tray,
            quit_app,
            minimize_window,
            toggle_maximize,
            is_maximized,
            is_dev,
            update_available,
            restart_app,
            list_fonts,
            app_info,
            notify,
            open_url,
            read_audio_data,
            session_pr,
            open_session_window,
            open_pty,
            write_pty,
            resize_pty,
            close_pty,
            running_ptys,
            set_claude_path,
            list_sessions
        ])
        .build(tauri::generate_context!())
        .expect("error while building Mandor")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app.try_state::<PtyState>() {
                    state.kill_all();
                }
            }
        });
}
