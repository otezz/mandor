//! Remote access: an optional HTTP+WebSocket server that serves the exact same
//! `ui/` frontend. By default it binds loopback only, reachable through a
//! tunnel you already trust (an SSH port-forward, or `tailscale serve`
//! publishing this loopback port onto your tailnet) — the tunnel is the whole
//! security boundary, the same principle as the outbound SSH-host feature.
//! `RemoteConfig::bind_lan` is a separate, explicit opt-in to bind every
//! interface instead, so any device on the same LAN can connect directly with
//! no tunnel at all; that's a real widening of who can reach this, not a
//! default.
//!
//! `ui/app.js` has exactly one seam where it reaches the backend:
//! `window.__TAURI__.core.invoke` / `.event.listen`. When that global is absent
//! (a plain browser tab, not the Tauri webview) the frontend's own transport
//! shim switches to `fetch()` for commands and one shared WebSocket for events —
//! see the top of `ui/app.js`. This module is the server side of that shim: a
//! generic `/api/invoke/{command}` dispatcher that calls the *exact same*
//! `#[tauri::command]` functions the desktop app calls (no duplicated logic),
//! and a `/ws` bridge that re-publishes Tauri's own `pty-output` / `pty-exit` /
//! `store-update` events to connected browsers.
//!
//! Deliberately out of scope here (see `ui/app.js`'s `IS_REMOTE` gating): native
//! window chrome, opening new OS windows, the native folder-picker dialog
//! (replaced by the `browse_dir` command, a plain directory listing), the
//! autostart toggle, and switching to a different profile's window — none of
//! those are meaningful from a browser tab.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, State,
    },
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Listener, Manager};

use crate::pty::{self, PtyState};
use crate::store::{self, RemoteConfig, StoreState};

#[derive(rust_embed::RustEmbed)]
#[folder = "../ui"]
struct UiAssets;

/// Start the remote server in the background. Fire-and-forget: a bind failure
/// (e.g. the port is already in use) is logged, not fatal to the app — Mandor
/// works fully offline either way, remote access is strictly additive.
pub fn spawn(app: AppHandle, config: RemoteConfig) {
    if !config.enabled {
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run(app, config).await {
            eprintln!("[mandor] remote server: {e}");
        }
    });
}

async fn run(app: AppHandle, config: RemoteConfig) -> Result<(), String> {
    let token = config.token.trim().to_string();
    let router = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/invoke/{command}", post(invoke_handler))
        .fallback(get(asset_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            check_token(token.clone(), req, next)
        }))
        .with_state(app);

    // Bind every interface only when the user explicitly opted into LAN access
    // (see RemoteConfig::bind_lan) — the default stays loopback-only, so a
    // fresh install or an unmodified config never becomes reachable from the
    // network just because "enabled" is on.
    let host = if config.bind_lan {
        [0, 0, 0, 0]
    } else {
        [127, 0, 0, 1]
    };
    let addr = SocketAddr::from((host, config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    if config.bind_lan {
        println!(
            "[mandor] remote server listening on http://{addr} (LAN — reachable by any device on this network)"
        );
    } else {
        println!("[mandor] remote server listening on http://{addr}");
    }
    axum::serve(listener, router)
        .await
        .map_err(|e| e.to_string())
}

async fn check_token(
    token: String,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if token.is_empty() {
        return next.run(req).await;
    }
    let query = req.uri().query().unwrap_or("");
    let ok = query_param(query, "token") == Some(token.as_str());
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or wrong ?token=").into_response()
    }
}

fn query_param<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        (k == key).then(|| it.next().unwrap_or(""))
    })
}

// --- static assets (the same ui/ the desktop webview loads) ---

async fn asset_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match UiAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (axum::http::header::CONTENT_TYPE, mime.as_ref().to_string()),
                    // No ETag/Last-Modified is set below, so with no explicit
                    // directive here a browser's own heuristic caching (mobile
                    // Safari/Chrome in particular) can go on serving a stale
                    // app.js indefinitely across page loads, with no way to
                    // tell it just happened — a real, previously-hit bug.
                    (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// --- generic command dispatch: mirrors the Tauri IPC surface over HTTP ---

async fn invoke_handler(
    State(app): State<AppHandle>,
    AxumPath(command): AxumPath<String>,
    body: Bytes,
) -> Response {
    let args: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
    match dispatch(&app, &command, args).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}
fn req_str(v: &Value, k: &str) -> Result<String, String> {
    opt_str(v, k).ok_or_else(|| format!("missing field: {k}"))
}
fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn num_u16(v: &Value, k: &str) -> Result<u16, String> {
    v.get(k)
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("missing/invalid field: {k}"))
}
fn to_value<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// The command surface exposed remotely. Window-chrome and native-dialog
/// commands (hide_to_tray, minimize_window, toggle_maximize, is_maximized,
/// open_session_window, open_profile_window, the autostart/dialog plugins) are
/// deliberately absent — `ui/app.js` hides the UI that would call them when
/// `IS_REMOTE` is set, per the module doc comment above.
async fn dispatch(app: &AppHandle, command: &str, args: Value) -> Result<Value, String> {
    let pty_state = app.state::<PtyState>();
    let store_state = app.state::<StoreState>();
    match command {
        "open_pty" => {
            pty::open_pty(
                pty_state,
                app.clone(),
                req_str(&args, "id")?,
                req_str(&args, "cwd")?,
                num_u16(&args, "cols")?,
                num_u16(&args, "rows")?,
                opt_str(&args, "resume"),
                opt_str(&args, "name"),
                flag(&args, "worktree"),
                opt_str(&args, "sessionId"),
                opt_str(&args, "model"),
                flag(&args, "remoteControl"),
                flag(&args, "incognito"),
                opt_str(&args, "profileId"),
                flag(&args, "agents"),
                flag(&args, "yolo"),
            )?;
            Ok(Value::Null)
        }
        "write_pty" => {
            pty::write_pty(pty_state, req_str(&args, "id")?, req_str(&args, "data")?)?;
            Ok(Value::Null)
        }
        "resize_pty" => {
            pty::resize_pty(
                pty_state,
                req_str(&args, "id")?,
                num_u16(&args, "cols")?,
                num_u16(&args, "rows")?,
                req_str(&args, "clientId")?,
            )?;
            Ok(Value::Null)
        }
        "claim_control" => {
            pty::claim_control(
                pty_state,
                req_str(&args, "id")?,
                req_str(&args, "clientId")?,
            )?;
            Ok(Value::Null)
        }
        "close_pty" => {
            pty::close_pty(pty_state, req_str(&args, "id")?)?;
            Ok(Value::Null)
        }
        "set_claude_path" => {
            pty::set_claude_path(pty_state, opt_str(&args, "path"));
            Ok(Value::Null)
        }
        "running_ptys" => to_value(pty::running_ptys(pty_state)),
        "create_profile" => {
            pty::create_profile(req_str(&args, "id")?, flag(&args, "copyLogin"))?;
            Ok(Value::Null)
        }
        "delete_profile" => {
            pty::delete_profile(req_str(&args, "id")?)?;
            Ok(Value::Null)
        }
        "list_sessions" => {
            to_value(crate::sessions::list_sessions(opt_str(&args, "profileId")).await?)
        }
        "session_pr" => to_value(
            crate::sessions::session_pr(
                req_str(&args, "id")?,
                req_str(&args, "cwd")?,
                opt_str(&args, "profileId"),
            )
            .await?,
        ),
        "browse_dir" => Ok(browse_dir(opt_str(&args, "path"))),
        "get_store" => Ok(store::get_store(store_state, opt_str(&args, "profileId"))),
        "save_store" => {
            store::save_store(
                app.clone(),
                store_state,
                opt_str(&args, "profileId"),
                args.get("value").cloned().unwrap_or(Value::Null),
            );
            Ok(Value::Null)
        }
        "app_info" => Ok(super::app_info()),
        "is_dev" => Ok(json!(super::is_dev())),
        "list_fonts" => to_value(super::list_fonts().await?),
        "notify" => {
            super::notify(req_str(&args, "title")?, req_str(&args, "body")?)?;
            Ok(Value::Null)
        }
        "open_url" => {
            super::open_url(req_str(&args, "url")?)?;
            Ok(Value::Null)
        }
        "read_audio_data" => Ok(json!(
            super::read_audio_data(req_str(&args, "path")?).await?
        )),
        "update_available" => Ok(json!(super::update_available(app.state())
            .await
            .unwrap_or(false))),
        "restart_app" => {
            super::restart_app(app.clone(), app.state());
            Ok(Value::Null)
        }
        "quit_app" => {
            super::quit_app(app.clone());
            Ok(Value::Null)
        }
        other => Err(format!("unknown or unavailable remotely: {other}")),
    }
}

// --- folder browsing (remote stand-in for the native file/folder dialog) ---
//
// A real Tauri command (registered in main.rs's generate_handler!) rather than
// a one-off HTTP route: the desktop app never calls this (it has the native
// dialog), but defining it as a command lets the remote dispatch table below
// call it exactly like every other command, through the one generic path,
// instead of carrying a second, differently-shaped route just for this.

#[tauri::command]
pub fn browse_dir(path: Option<String>) -> Value {
    let requested = path
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("/"));
    let dir = if requested.is_dir() {
        requested
    } else {
        dirs_home().unwrap_or_else(|| PathBuf::from("/"))
    };
    let mut entries = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue; // only directories are choosable
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue; // hide dotfiles/dirs — matches the native picker's default view
            }
            entries.push(json!({
                "name": name,
                "path": entry.path().to_string_lossy(),
            }));
        }
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "path": dir.to_string_lossy(),
        "parent": dir.parent().map(|p| p.to_string_lossy().into_owned()),
        "entries": entries,
    })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// --- WebSocket bridge: forwards pty-output / pty-exit / store-update ---

async fn ws_handler(State(app): State<AppHandle>, uri: Uri, ws: WebSocketUpgrade) -> Response {
    // Tags this connection with the same clientId resize_pty/claim_control use,
    // so its claimed session sizes can be released (see pty::client_disconnected)
    // the moment this socket closes — a real disconnect signal a stateless
    // /api/invoke POST can't give us.
    let client_id = uri
        .query()
        .and_then(|q| query_param(q, "clientId"))
        .map(str::to_string);
    ws.on_upgrade(move |socket| bridge(socket, app, client_id))
}

const BRIDGED_EVENTS: [&str; 3] = ["pty-output", "pty-exit", "store-update"];

async fn bridge(socket: WebSocket, app: AppHandle, client_id: Option<String>) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let mut listener_ids = Vec::new();
    for name in BRIDGED_EVENTS {
        let tx = tx.clone();
        let name = name.to_string();
        let id = app.listen(name.clone(), move |event| {
            let payload: Value = serde_json::from_str(event.payload()).unwrap_or(Value::Null);
            let msg = json!({ "event": name, "payload": payload }).to_string();
            let _ = tx.send(msg);
        });
        listener_ids.push(id);
    }

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                match outgoing {
                    Some(msg) => {
                        if sender.send(Message::Text(msg.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            incoming = receiver.next() => {
                // Commands go over /api/invoke; this socket is server->client
                // only. Any message (including a plain ping) just proves the
                // connection is alive; a close or error ends the bridge.
                match incoming {
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }

    for id in listener_ids {
        app.unlisten(id);
    }
    if let Some(client_id) = client_id {
        pty::client_disconnected(app.state::<PtyState>(), &client_id);
    }
}
