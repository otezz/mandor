// Prevents an additional console window on Windows in release. DO NOT REMOVE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pty;
mod sessions;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};

use pty::{close_pty, open_pty, resize_pty, running_ptys, set_claude_path, write_pty, PtyState};
use sessions::list_sessions;

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
    let _ = window.hide();
}

/// Really quit (kills sessions via the ExitRequested handler) — from the app menu.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
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

/// App metadata for an About view.
#[tauri::command]
fn app_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
    })
}

/// Show a desktop notification (used when a session wants attention while the
/// window is unfocused).
#[tauri::command]
fn notify(app: tauri::AppHandle, title: String, body: String) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
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

    tauri::Builder::default()
        // Must be first: focus the existing window instead of launching a duplicate.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // Remember the main window's size/position across launches.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(PtyState::default())
        .setup(|app| {
            // Tray icon: closing the window hides to tray (sessions keep running);
            // Quit really exits (killing sessions via the ExitRequested handler).
            let show_i = MenuItem::with_id(app, "show", "Show mandor-term", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit mandor-term", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("no window icon")?)
                .tooltip("mandor-term")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => app.exit(0),
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
                let handle = app.handle().clone();
                let win_hide = win.clone();
                let notified = Arc::new(AtomicBool::new(false));
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        // Fallback for OS-level close (e.g. Alt+F4): keep the app alive
                        // and hide to tray. The primary close path is the custom
                        // titlebar button (hide_to_tray command). Deferred so hide()
                        // takes effect on GTK.
                        api.prevent_close();
                        let w = win_hide.clone();
                        let _ = win_hide.app_handle().run_on_main_thread(move || {
                            let _ = w.hide();
                        });
                        if !notified.swap(true, Ordering::Relaxed) {
                            use tauri_plugin_notification::NotificationExt;
                            let _ = handle
                                .notification()
                                .builder()
                                .title("mandor-term is still running")
                                .body("Your sessions keep running in the background. Reopen or quit from the tray icon.")
                                .show();
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
            app_info,
            notify,
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
        .expect("error while building mandor-term")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app.try_state::<PtyState>() {
                    state.kill_all();
                }
            }
        });
}
