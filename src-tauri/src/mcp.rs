//! Session tools for claude: a minimal MCP server (Streamable HTTP, JSON
//! responses only) that every Mandor-spawned `claude` is pointed at through
//! `--mcp-config`. It lets one session create another in a group, message it,
//! and list sessions, so a session can hand work over or coordinate others.
//!
//! The server listens on loopback only, on its own port (the remote server is
//! optional and may be LAN-bound). Each PTY gets a fresh random key, sent as a
//! bearer token, which identifies the calling session. The session list lives
//! in the owning window, so tool calls are forwarded to it as an `mcp-request`
//! event and answered through `mcp_reply`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

use crate::pty::PtyState;

static PORT: OnceLock<u16> = OnceLock::new();

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

const INSTRUCTIONS: &str = "You are running inside Mandor, a desktop app that hosts several \
claude sessions side by side, organised into groups (usually one per repository). Use \
create_session to hand work to a new session in another group (for example a frontend \
counterpart of backend work) or to spawn sub-sessions as a coordinator: its prompt must be \
self-contained, because the new session starts with no knowledge of this conversation. \
Messages from other sessions arrive as user turns starting with [Mandor: message from \
session ...]; answer them with send_message, not in your own reply.";

#[derive(Default)]
pub struct McpState {
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
}

/// A fresh per-PTY key: 32 random bytes, hex.
pub fn new_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The environment variable holding a session's key. It stays out of the
/// command line, which other local users can read; claude substitutes it into
/// the header itself.
pub const KEY_ENV: &str = "MANDOR_MCP_KEY";

/// The `--mcp-config` JSON for Mandor's sessions, or None while the server isn't
/// up (claude then simply runs without the tools).
pub fn mcp_config() -> Option<String> {
    let port = PORT.get()?;
    Some(
        json!({
            "mcpServers": {
                "mandor": {
                    "type": "http",
                    "url": format!("http://127.0.0.1:{port}/mcp"),
                    "headers": { "Authorization": format!("Bearer ${{{KEY_ENV}}}") },
                }
            }
        })
        .to_string(),
    )
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let listener =
            match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[mandor] session tools: bind: {e}");
                    return;
                }
            };
        if let Ok(addr) = listener.local_addr() {
            let _ = PORT.set(addr.port());
        }
        let router = Router::new()
            .route("/mcp", post(handle).get(no_stream).delete(no_stream))
            .with_state(app);
        if let Err(e) = axum::serve(listener, router).await {
            eprintln!("[mandor] session tools: {e}");
        }
    });
}

// No server-initiated stream: every reply comes back on its POST.
async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn handle(State(app): State<AppHandle>, headers: HeaderMap, body: Bytes) -> Response {
    // Browsers always send Origin on these requests; claude doesn't. Refusing it
    // keeps web pages (DNS rebinding included) away from the endpoint.
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let Some(caller) = app.state::<PtyState>().session_for_key(key) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Json(rpc_error(Value::Null, -32700, "parse error")).into_response(),
    };
    let Some(id) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response(); // a notification or response
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "initialize" => rpc_result(id, initialize_result(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let known = tools()
                .as_array()
                .is_some_and(|ts| ts.iter().any(|t| t["name"] == name));
            let outcome = if known {
                call_window(&app, &caller, name, args).await
            } else {
                Err(format!("unknown tool {name}"))
            };
            rpc_result(id, tool_result(outcome))
        }
        _ => rpc_error(id, -32601, "method not found"),
    };
    Json(reply).into_response()
}

fn initialize_result(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "mandor", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "create_session",
            "description": "Start a new claude session in Mandor. It appears in Mandor's sidebar \
    in the given group and runs `prompt` as its first message. The folder, name prefix, model and \
    worktree setting default to the group's. The new session is told who created it and can reply \
    with send_message. Returns the new session's id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "group": { "type": "string", "description": "Group name or id (see list_sessions). Omit to pick by folder." },
                    "name": { "type": "string", "description": "Short session name, e.g. the ticket and topic. The group's prefix is added." },
                    "prompt": { "type": "string", "description": "The first message: a self-contained brief of the work." },
                    "cwd": { "type": "string", "description": "Folder to run in. Defaults to the group's folder." },
                    "worktree": { "type": "boolean", "description": "Run in a new git worktree. Defaults to the group's setting." },
                    "model": { "type": "string", "description": "Model alias such as opus or sonnet. Defaults to the group's." }
                },
                "required": ["name", "prompt"]
            }
        },
        {
            "name": "send_message",
            "description": "Type a message into another running Mandor session and submit it, as if \
    the user typed it. If that session is busy, claude queues it. The message is prefixed with \
    your session's name and id so the recipient can reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id, or its exact name." },
                    "message": { "type": "string" }
                },
                "required": ["session", "message"]
            }
        },
        {
            "name": "list_sessions",
            "description": "List Mandor's groups and sessions with each session's id, name, group, \
    folder, status (working, needs-attention, idle, not-running) and who created it.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// Forward a tool call to the window that owns the calling session and wait for
/// its answer.
async fn call_window(
    app: &AppHandle,
    caller: &(String, Option<String>),
    tool: &str,
    args: Value,
) -> Result<String, String> {
    let state = app.state::<McpState>();
    let request_id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = oneshot::channel();
    state
        .pending
        .lock()
        .map_err(|e| e.to_string())?
        .insert(request_id, tx);
    let sent = app.emit(
        "mcp-request",
        json!({
            "requestId": request_id,
            "profileId": caller.1,
            "caller": caller.0,
            "tool": tool,
            "args": args,
        }),
    );
    let answer = match sent {
        Ok(()) => tokio::time::timeout(REPLY_TIMEOUT, rx).await,
        Err(e) => {
            take_pending(&state, request_id);
            return Err(e.to_string());
        }
    };
    match answer {
        Ok(Ok(v)) => window_answer(v),
        _ => {
            take_pending(&state, request_id);
            Err("Mandor's window did not answer; is it open?".into())
        }
    }
}

fn take_pending(state: &McpState, request_id: u64) -> Option<oneshot::Sender<Value>> {
    state.pending.lock().ok()?.remove(&request_id)
}

fn window_answer(v: Value) -> Result<String, String> {
    match (v.get("ok"), v.get("error")) {
        (_, Some(e)) => Err(e.as_str().map_or_else(|| e.to_string(), str::to_string)),
        (Some(Value::String(s)), _) => Ok(s.clone()),
        (Some(other), _) => Ok(other.to_string()),
        _ => Err("empty answer from Mandor's window".into()),
    }
}

/// The window's answer to an `mcp-request`: `{ ok: string }` or `{ error: string }`.
#[tauri::command]
pub fn mcp_reply(state: tauri::State<McpState>, request_id: u64, result: Value) {
    if let Some(tx) = take_pending(&state, request_id) {
        let _ = tx.send(result);
    }
}

fn tool_result(outcome: Result<String, String>) -> Value {
    let (text, is_error) = match outcome {
        Ok(t) => (t, false),
        Err(e) => (e, true),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_references_the_key_by_env_var() {
        let _ = PORT.set(4242);
        let cfg: Value = serde_json::from_str(&mcp_config().unwrap()).unwrap();
        let server = &cfg["mcpServers"]["mandor"];
        assert_eq!(server["url"], "http://127.0.0.1:4242/mcp");
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${MANDOR_MCP_KEY}"
        );
    }

    #[test]
    fn keys_are_long_random_hex() {
        let (a, b) = (new_key(), new_key());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn negotiates_a_known_protocol_version() {
        let v = |asked: &str| {
            initialize_result(&json!({ "protocolVersion": asked }))["protocolVersion"].clone()
        };
        assert_eq!(v("2025-06-18"), "2025-06-18");
        assert_eq!(v("1999-01-01"), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn window_answers_map_to_tool_results() {
        assert_eq!(window_answer(json!({ "ok": "done" })), Ok("done".into()));
        assert_eq!(
            window_answer(json!({ "error": "no group" })),
            Err("no group".into())
        );
        assert!(window_answer(json!({})).is_err());
        let r = tool_result(Err("boom".into()));
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "boom");
    }

    #[test]
    fn every_tool_requires_an_object_schema() {
        for t in tools().as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }
}
