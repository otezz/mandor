//! Shared app state (sessions, groups, settings, ...) owned by the backend so
//! every connected client — the local desktop webview *and* any remote browser
//! attached via the web server (see `webserver.rs`) — sees the same data.
//!
//! Before this module the sidebar's sessions/groups/settings lived only in the
//! webview's own `localStorage`, which is why a remote browser could never share
//! them: it's a different browser origin with its own storage bucket entirely.
//! Moving the source of truth here means "the same session, viewed
//! independently" is real, not cosmetic.
//!
//! Each profile (see `pty::profile_dir`) gets its own store file, mirroring the
//! pre-existing per-profile `mandor-term.sidebar.<id>` localStorage
//! partitioning. The store's *contents* are opaque JSON to Rust — whatever blob
//! the frontend wants persisted — so the frontend's evolving session/settings
//! schema doesn't have to be duplicated as Rust structs; this module's job is
//! durability and fan-out, not understanding the shape.
//!
//! Per-client UI state (which session is focused, split-view layout, whether
//! the sidebar is collapsed) deliberately stays OUT of this store and in each
//! client's own localStorage — that's what makes two simultaneous clients have
//! independent views of the same shared sessions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

#[derive(Default)]
pub struct StoreState {
    cache: Mutex<HashMap<String, Value>>,
}

/// Broadcast to every window/client when any one of them saves, so a remote
/// browser (and any other open window) picks up changes made elsewhere without
/// polling. Desktop windows already have a `listen("store-update", ...)`-style
/// hookup for other events; this follows the same shape.
#[derive(Clone, Serialize)]
struct StoreUpdate {
    #[serde(rename = "profileId")]
    profile_id: String,
    value: Value,
}

fn store_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let data = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(not(target_os = "macos"))]
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        });
    data.map(|d| d.join("mandor").join("store"))
}

/// "" (the default/main profile) can't be a filename, so it's normalized here.
fn store_key(profile_id: &str) -> String {
    if profile_id.is_empty() {
        "default".to_string()
    } else {
        profile_id.to_string()
    }
}

fn store_file(profile_id: &str) -> Option<PathBuf> {
    store_dir().map(|d| d.join(format!("{}.json", store_key(profile_id))))
}

fn load_from_disk(profile_id: &str) -> Value {
    store_file(profile_id)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// Write via a temp file + rename so a crash mid-write can't leave a corrupt,
/// half-written store — this file is now the single source of truth for the
/// session list, so torn writes are a real risk worth guarding against (unlike
/// localStorage's own backing store, an app crash here has no journal to recover
/// from).
fn save_to_disk(profile_id: &str, value: &Value) {
    let Some(path) = store_file(profile_id) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(value) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[tauri::command]
pub fn get_store(state: State<StoreState>, profile_id: Option<String>) -> Value {
    let key = profile_id.unwrap_or_default();
    let mut cache = state.cache.lock().unwrap();
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    let v = load_from_disk(&key);
    cache.insert(key, v.clone());
    v
}

#[tauri::command]
pub fn save_store(
    app: AppHandle,
    state: State<StoreState>,
    profile_id: Option<String>,
    value: Value,
) {
    let key = profile_id.unwrap_or_default();
    {
        let mut cache = state.cache.lock().unwrap();
        cache.insert(key.clone(), value.clone());
    }
    save_to_disk(&key, &value);
    // Fire-and-forget: the saving client already has this value locally, so a
    // failed/absent listener elsewhere is fine — this is only for OTHER clients.
    let _ = app.emit(
        "store-update",
        StoreUpdate {
            profile_id: key,
            value,
        },
    );
}

/// Whether the remote web server (`webserver.rs`) runs at all, and how. This is
/// a property of the running app process, not of any one profile — so unlike
/// the per-profile store above, it lives in its own file and is only ever shown
/// in the main window's Settings. Read once at startup: toggling it requires a
/// restart (a bound listener can't be hot-swapped as cheaply as most settings,
/// and this is rare enough to change that the tradeoff is an easy one).
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfig {
    pub enabled: bool,
    pub port: u16,
    /// "" = no token required — the loopback bind + your own tunnel (SSH
    /// port-forward, `tailscale serve`) is the trust boundary. Set one for
    /// defense-in-depth if more than one person/device can reach the tunnel.
    #[serde(default)]
    pub token: String,
    /// Off (default): bind 127.0.0.1 only — nothing but a tunnel you run
    /// yourself (SSH -L, `tailscale serve`) can ever reach this. On: bind every
    /// interface, so any device on the same LAN/Wi-Fi can connect directly at
    /// this machine's LAN IP, with no tunnel at all. This is a real widening of
    /// who can reach a full read/write control surface for your sessions — to
    /// anything on that network, not just your own devices — so it's a
    /// separate, explicit opt-in rather than folded into "enabled".
    #[serde(default)]
    pub bind_lan: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7420,
            token: String::new(),
            bind_lan: false,
        }
    }
}

fn remote_config_file() -> Option<PathBuf> {
    store_dir().map(|d| d.join("remote.json"))
}

#[tauri::command]
pub fn get_remote_config() -> RemoteConfig {
    remote_config_file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

#[tauri::command]
pub fn save_remote_config(config: RemoteConfig) -> Result<(), String> {
    let path = remote_config_file().ok_or("no data directory available")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())
}
