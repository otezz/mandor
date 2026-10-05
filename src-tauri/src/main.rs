// Prevents an additional console window on Windows in release. DO NOT REMOVE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pty;
mod sessions;
mod store;
mod webserver;

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
    claim_control, close_pty, create_profile, delete_profile, open_pty, resize_pty, running_ptys,
    set_claude_path, sweep_incognito_dirs, write_pty, PtyState,
};
use tauri_plugin_window_state::{AppHandleExt, StateFlags, WindowExt};

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
use sessions::{list_sessions, session_cwd, session_pr};

/// Cache file for the resolved login-shell PATH (see `ensure_tools_on_path`), under
/// the platform cache dir — mirrors `incognito_base`.
#[cfg(unix)]
fn shell_path_cache() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let cache = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"));
    #[cfg(not(target_os = "macos"))]
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    cache.map(|c| c.join("mandor").join("shell-path"))
}

/// Ask the user's login shell for its PATH. Uses an *interactive* login shell
/// (`-lic`) so entries added in `~/.zshrc`/`~/.bashrc` — version managers (nvm,
/// cargo), `~/.local/bin`, etc. — are included; a non-interactive shell misses
/// them. That makes this SLOW (~1s with a heavy rc), so its result is cached.
#[cfg(unix)]
fn probe_shell_path() -> String {
    use std::process::{Command, Stdio};
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // Markers isolate PATH from any banner an interactive rc file might print.
    Command::new(&shell)
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
        .unwrap_or_default()
}

/// Merge a probed shell PATH with a few common user bin dirs and the current PATH
/// (de-duplicated) and set it on this process, so spawned children (the `claude`
/// PTY) resolve tools the same as an interactive terminal.
#[cfg(unix)]
fn apply_resolved_path(shell_path: &str) {
    use std::collections::HashSet;
    let home = std::env::var("HOME").unwrap_or_default();
    let current = std::env::var("PATH").unwrap_or_default();
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

/// GUI launches (from a .desktop entry) don't inherit the shell's PATH, so tools
/// installed under e.g. ~/.local/bin (`claude`) or version managers aren't found
/// and spawning fails with "No such file or directory". Resolve the login shell's
/// PATH and merge it into this process.
///
/// Probing the shell is slow (~1s with a heavy interactive rc) and it ran on the
/// main thread *before* the UI, so every launch stalled. The probe's result is now
/// cached: after the first launch the cached PATH is applied instantly, and the
/// cache is refreshed in the background (a file write only — never touching this
/// process's env, so no cross-thread `set_var`) when it's more than a day old.
#[cfg(unix)]
fn ensure_tools_on_path() {
    let cache = shell_path_cache();

    // Fast path: apply the cached PATH without waiting on the login shell.
    if let Some(path) = cache.as_ref().filter(|p| p.exists()) {
        if let Ok(sp) = std::fs::read_to_string(path) {
            apply_resolved_path(&sp);
            let stale = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map_or(true, |age| age.as_secs() > 24 * 60 * 60);
            if stale {
                let path = path.clone();
                std::thread::spawn(move || write_shell_path_cache(&path, &probe_shell_path()));
            }
            return;
        }
    }

    // First launch (no cache): probe synchronously so the first session resolves
    // tools, then cache the result for subsequent launches.
    let sp = probe_shell_path();
    apply_resolved_path(&sp);
    if let Some(path) = cache {
        write_shell_path_cache(&path, &sp);
    }
}

#[cfg(unix)]
fn write_shell_path_cache(path: &std::path::Path, shell_path: &str) {
    if shell_path.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, shell_path);
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

/// The executable to relaunch on restart. `current_exe()` — what Tauri's own
/// `restart()` uses — is unreliable here: "Restart to update" runs precisely
/// *after* the binary was replaced, and installing over a running program unlinks
/// the old inode, so Linux resolves /proc/self/exe to "<path> (deleted)". That
/// path doesn't exist, so re-launching it fails and the app never comes back.
/// Prefer $APPIMAGE (the .AppImage itself, not its temporary mount), then the path
/// captured at launch (before any replacement), then a de-"(deleted)" current_exe.
fn relaunch_path(startup: &AppStartup) -> Option<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        let p = PathBuf::from(appimage);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(p) = startup.exe.as_ref().filter(|p| p.exists()) {
        return Some(p.clone());
    }
    let cur = std::env::current_exe().ok()?;
    let cleaned = cur
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .unwrap_or(cur);
    cleaned.exists().then_some(cleaned)
}

/// Relaunch the app (from the "Restart to update" prompt). Live PTYs are killed
/// via the ExitRequested handler; persisted sessions come back cold, resumable.
#[tauri::command]
fn restart_app(app: tauri::AppHandle, startup: tauri::State<'_, AppStartup>) {
    save_window_geometry(&app);
    let Some(exe) = relaunch_path(&startup) else {
        app.restart(); // nothing better to try
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The new process can't start while this one lives: the single-instance
        // plugin would make it hand off to this (dying) instance and exit, leaving
        // nothing running. So the relaunch waits for this process to disappear
        // before exec'ing. Detached — own process group, no inherited stdio — so it
        // survives this process exiting.
        //
        // It must also wait for our WebKit children. The webview's localStorage —
        // where the session list lives — is an SQLite database held open by
        // WebKitNetworkProcess, not by us, and that child flushes pending writes as
        // it shuts down. Starting the new instance before it finishes lets a second
        // process open the same database mid-flush, read a stale snapshot, and
        // persist that over the newer state: sessions silently vanish. Waiting for
        // those pids (plus a small margin) keeps the handover ordered.
        let pid = std::process::id();
        let mut pids = vec![pid.to_string()];
        if let Ok(out) = std::process::Command::new("pgrep")
            .args(["-P", &pid.to_string(), "WebKit"])
            .output()
        {
            if out.status.success() {
                pids.extend(
                    String::from_utf8_lossy(&out.stdout)
                        .split_whitespace()
                        .map(str::to_string),
                );
            }
        }
        let script = format!(
            "i=0; for p in {pids}; do while kill -0 $p 2>/dev/null && [ $i -lt 150 ]; \
             do sleep 0.1; i=$((i+1)); done; done; sleep 0.5; exec \"$0\"",
            pids = pids.join(" ")
        );
        let spawned = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg(&exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .is_ok();
        if !spawned {
            app.restart();
        }
        app.exit(0); // ExitRequested → kill_all, then the waiter starts the new one
    }

    #[cfg(not(unix))]
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

/// Open a folder in the user's editor (session menu → "Open in editor").
/// `editor` is a command line such as `code`, `cursor` or `zed -n`; the folder is
/// appended as the last argument. Blank means `code`.
#[tauri::command]
async fn open_in_editor(path: String, editor: Option<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || launch_editor(&path, editor))
        .await
        .map_err(|e| e.to_string())?
}

fn launch_editor(path: &str, editor: Option<String>) -> Result<(), String> {
    if !std::path::Path::new(path).is_dir() {
        return Err(format!("folder not found: {path}"));
    }
    let editor = editor
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "code".into());
    let mut words = shell_words::split(&editor).map_err(|e| format!("editor command: {e}"))?;
    if words.is_empty() {
        return Err("editor command is empty".into());
    }
    let program = words.remove(0);
    // Windows editor launchers are .cmd shims, which only cmd can run.
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", &program]);
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&words)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = match cmd.spawn() {
        Ok(child) => child,
        // VS Code's `code` CLI is an optional extra install on macOS.
        #[cfg(target_os = "macos")]
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && program == "code" => {
            let status = std::process::Command::new("open")
                .args(["-a", "Visual Studio Code", path])
                .status()
                .map_err(|e| format!("couldn't open VS Code: {e}"))?;
            return if status.success() {
                Ok(())
            } else {
                Err("VS Code isn't installed — set your editor in Settings".into())
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("editor command `{program}` not found on PATH"));
        }
        Err(e) => return Err(format!("couldn't run `{program}`: {e}")),
    };
    // Reap it once it exits (editor CLIs return quickly) so it doesn't linger
    // as a zombie for the life of the app.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Secondary windows get the same frame as the main window (see
/// tauri.macos.conf.json): the custom titlebar everywhere, and on macOS native
/// traffic lights too — a borderless NSWindow can't enter native full screen.
fn with_window_frame<'a, R: tauri::Runtime, M: Manager<R>>(
    builder: tauri::WebviewWindowBuilder<'a, R, M>,
) -> tauri::WebviewWindowBuilder<'a, R, M> {
    #[cfg(target_os = "macos")]
    return builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(14.0, 18.0));
    #[cfg(not(target_os = "macos"))]
    builder.decorations(false)
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
    let builder =
        tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
            .title(name)
            .inner_size(1000.0, 700.0)
            .min_inner_size(600.0, 400.0);
    with_window_frame(builder)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or focus) a full sidebar window bound to a profile. The window loads the
/// same UI parameterized by `?profile=<id>`, so its sessions run under that
/// profile's config dir and it keeps its own (per-profile) session list.
#[tauri::command]
fn open_profile_window(app: tauri::AppHandle, id: String, name: String) -> Result<(), String> {
    let label = format!("profile-{id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url = format!("index.html?profile={id}");
    let builder =
        tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
            .title(name)
            .inner_size(1100.0, 760.0)
            .min_inner_size(600.0, 400.0);
    with_window_frame(builder)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Best-effort check for whether this process was launched by GNOME's autostart
/// handling rather than a user action (app grid, tray, second instance).
/// DESKTOP_AUTOSTART_ID (the freedesktop autostart-launch marker) isn't set by
/// this GNOME version, but it does record who requested the launch in our own
/// transient systemd scope's Description — visible in `journalctl` as
/// "Application launched by gnome-session-service" (autostart) vs "...by
/// gnome-shell" (a manual launch) — so read that back instead of guessing.
#[cfg(target_os = "linux")]
fn launched_by_gnome_autostart() -> bool {
    let cgroup = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let scope = cgroup.lines().find_map(|line| {
        let seg = line.rsplit('/').next()?;
        seg.ends_with(".scope").then(|| seg.to_string())
    });
    let Some(scope) = scope else {
        return false;
    };
    std::process::Command::new("systemctl")
        .args(["--user", "show", &scope, "-p", "Description", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("gnome-session-service"))
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn launched_by_gnome_autostart() -> bool {
    false
}

/// On macOS the window frame is fixed by config (native traffic lights), so a
/// saved `decorated: false` from an older borderless build must not be restored.
fn window_state_flags() -> StateFlags {
    let flags = StateFlags::SIZE
        | StateFlags::POSITION
        | StateFlags::MAXIMIZED
        | StateFlags::DECORATIONS
        | StateFlags::FULLSCREEN;
    if cfg!(target_os = "macos") {
        flags - StateFlags::DECORATIONS
    } else {
        flags
    }
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
    // WebKitGTK's Wayland DMA-BUF renderer can attach a stale SHM buffer to a
    // surface already bound to wp_linux_drm_syncobj_surface_v1 (explicit sync),
    // which is a protocol violation — the compositor kills the connection with
    // "Error 71 (Protocol error) dispatching to Wayland display" before any
    // window appears. Confirmed via WAYLAND_DEBUG=1: "Explicit Sync only
    // supported on dmabuf buffers". Seen on NVIDIA, whose explicit-sync/dmabuf
    // support under WebKitGTK is still immature. Force the stable SHM renderer.
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

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
        // Remember the main window's size/position across launches. VISIBLE is
        // deliberately excluded: the window starts hidden (config) and the frontend
        // shows it once loaded — after the plugin has applied the saved size, so
        // WebKitGTK doesn't map it at the config size first (a post-show resize
        // doesn't stick on GTK). Letting the plugin own VISIBLE would also let it
        // persist visible:false when quitting from the tray while hidden, which
        // would relaunch the app invisible.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .manage(PtyState::default())
        .manage(store::StoreState::default())
        .manage(AppStartup {
            exe: std::env::current_exe().ok(),
            start_mtime: std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
        })
        .setup(|app| {
            // Remote access (optional, off by default): a loopback-only HTTP+WS
            // server serving the same ui/ frontend, reached through a tunnel the
            // user already trusts (SSH port-forward, `tailscale serve`) — see
            // webserver.rs. Read once at launch; toggling the setting takes
            // effect on the next restart.
            webserver::spawn(app.handle().clone(), store::get_remote_config());
            // show_notification uses notify_rust directly, so register the sender
            // app as tauri-plugin-notification does; otherwise the first
            // notification looks up an app named "use_default" and macOS asks
            // "Where is use_default?". Unbundled dev builds have no bundle to
            // register, hence Terminal.
            #[cfg(target_os = "macos")]
            let _ = notify_rust::set_application(if tauri::is_dev() {
                "com.apple.Terminal"
            } else {
                &app.config().identifier
            });
            // Tray icon: closing the window hides to tray (sessions keep running);
            // Quit really exits (killing sessions via the ExitRequested handler).
            // On macOS a left click opens the menu, like other menu bar icons;
            // elsewhere it shows the window and the menu is on right click.
            let show_i = MenuItem::with_id(app, "show", "Show Mandor", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Mandor", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            let menu_on_left_click = cfg!(target_os = "macos");
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("no window icon")?)
                .tooltip("Mandor")
                .menu(&menu)
                .show_menu_on_left_click(menu_on_left_click)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        save_window_geometry(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(move |tray, event| {
                    if menu_on_left_click {
                        return;
                    }
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
                // The window is created hidden (config) so its saved size is applied
                // before it's mapped — a resize after mapping doesn't stick on
                // WebKitGTK. Size it here, while still hidden, then show it: the
                // webview (and its JS) only initializes once the window is mapped, so
                // the frontend can't reveal itself and Rust must. VISIBLE is left out
                // of the flags on purpose — the plugin re-reads live visibility on
                // exit, and quitting from the tray while hidden would otherwise
                // persist visible:false and relaunch the app invisible.
                let _ = win.restore_state(
                    StateFlags::SIZE
                        | StateFlags::POSITION
                        | StateFlags::MAXIMIZED
                        | StateFlags::FULLSCREEN,
                );
                let _ = win.show();
                if launched_by_gnome_autostart() {
                    // Let the webview map and run its JS (sessions reconnect
                    // there) once, then hide back to tray without ever
                    // stealing focus — same end state as a user opening and
                    // immediately hiding the window themselves.
                    let w = win.clone();
                    let handle = win.app_handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        let _ = handle.run_on_main_thread(move || {
                            let _ = w.hide();
                        });
                    });
                } else {
                    let _ = win.set_focus();
                }

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
            open_in_editor,
            read_audio_data,
            session_pr,
            session_cwd,
            open_session_window,
            open_profile_window,
            create_profile,
            delete_profile,
            open_pty,
            write_pty,
            resize_pty,
            claim_control,
            close_pty,
            running_ptys,
            set_claude_path,
            list_sessions,
            store::get_store,
            store::save_store,
            store::get_remote_config,
            store::save_remote_config,
            webserver::browse_dir
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

#[cfg(test)]
mod tests {
    use super::launch_editor;

    fn tmp() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    #[test]
    fn editor_rejects_missing_folder() {
        let err = launch_editor("/definitely/not/a/folder", None).unwrap_err();
        assert!(err.contains("folder not found"), "{err}");
    }

    #[test]
    #[cfg(not(target_os = "windows"))] // there it runs via `cmd /C`
    fn editor_reports_unknown_command() {
        let err = launch_editor(&tmp(), Some("mandor-no-such-editor --flag".into())).unwrap_err();
        assert!(err.contains("`mandor-no-such-editor` not found"), "{err}");
    }

    #[test]
    fn editor_rejects_unbalanced_quotes() {
        let err = launch_editor(&tmp(), Some("\"unterminated".into())).unwrap_err();
        assert!(err.starts_with("editor command:"), "{err}");
    }
}
