use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// PTY output is a high-frequency stream; per-read Tauri events jank WebKitGTK's
/// IPC. A reader thread pushes raw chunks over a channel and a batcher thread
/// coalesces them — flushing once ~8ms have passed or ~16KB has accumulated —
/// then emits a single base64 `pty-output` event.
const FLUSH_BYTES: usize = 16 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(8);

struct PtySession {
    cwd: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

type Sessions = Arc<Mutex<HashMap<String, PtySession>>>;

#[derive(Clone, Default)]
pub struct PtyState {
    sessions: Sessions,
}

impl PtyState {
    /// Kill every child on app exit (wired from `RunEvent::ExitRequested`).
    pub fn kill_all(&self) {
        if let Ok(mut map) = self.sessions.lock() {
            for (_, mut session) in map.drain() {
                let _ = session.child.kill();
            }
        }
    }
}

#[derive(Clone, Serialize)]
struct PtyOutput {
    id: String,
    b64: String,
}

#[derive(Clone, Serialize)]
struct PtyExit {
    id: String,
}

#[derive(Clone, Serialize)]
pub struct RunningPty {
    id: String,
    cwd: String,
}

/// Spawn interactive `claude` in a PTY under `cwd` and start streaming its output.
/// When `resume` is set, continues that Claude session (`claude --resume <id>`).
#[tauri::command]
pub fn open_pty(
    state: State<PtyState>,
    app: AppHandle,
    id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    resume: Option<String>,
) -> Result<(), String> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(&cwd);
    // Inherited env carries our merged PATH; TERM isn't inherited on a GUI launch.
    cmd.env("TERM", "xterm-256color");
    if let Some(session_id) = &resume {
        cmd.arg("--resume");
        cmd.arg(session_id);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // Reader thread: blocking reads off the PTY, never on the main thread.
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        // tx drops here → the batcher sees a disconnect and finishes.
    });

    // Batcher thread: coalesce chunks, emit, and on disconnect emit `pty-exit`.
    let batch_app = app.clone();
    let batch_id = id.clone();
    let batch_sessions = state.sessions.clone();
    std::thread::spawn(move || {
        let engine = base64::engine::general_purpose::STANDARD;
        loop {
            let mut pending = match rx.recv() {
                Ok(chunk) => chunk,
                Err(_) => break,
            };
            let disconnected = loop {
                if pending.len() >= FLUSH_BYTES {
                    break false;
                }
                match rx.recv_timeout(FLUSH_INTERVAL) {
                    Ok(chunk) => pending.extend_from_slice(&chunk),
                    Err(mpsc::RecvTimeoutError::Timeout) => break false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break true,
                }
            };
            let _ = batch_app.emit(
                "pty-output",
                PtyOutput {
                    id: batch_id.clone(),
                    b64: engine.encode(&pending),
                },
            );
            if disconnected {
                break;
            }
        }
        if let Ok(mut map) = batch_sessions.lock() {
            map.remove(&batch_id);
        }
        let _ = batch_app.emit("pty-exit", PtyExit { id: batch_id });
    });

    state.sessions.lock().map_err(|e| e.to_string())?.insert(
        id,
        PtySession {
            cwd,
            master: pair.master,
            writer,
            child,
        },
    );
    Ok(())
}

#[tauri::command]
pub fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.sessions.lock().map_err(|e| e.to_string())?;
    let session = map.get_mut(&id).ok_or("no such session")?;
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|e| e.to_string())?;
    session.writer.flush().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn resize_pty(state: State<PtyState>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.sessions.lock().map_err(|e| e.to_string())?;
    if let Some(session) = map.get(&id) {
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn close_pty(state: State<PtyState>, id: String) -> Result<(), String> {
    if let Some(mut session) = state
        .sessions
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id)
    {
        let _ = session.child.kill();
    }
    Ok(())
}

/// Live PTYs (id + cwd), for reconnect-on-reload.
#[tauri::command]
pub fn running_ptys(state: State<PtyState>) -> Vec<RunningPty> {
    state
        .sessions
        .lock()
        .map(|map| {
            map.iter()
                .map(|(id, session)| RunningPty {
                    id: id.clone(),
                    cwd: session.cwd.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}
