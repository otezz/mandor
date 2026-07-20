use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
    // For incognito sessions: a throwaway CLAUDE_CONFIG_DIR to delete on teardown.
    incognito_dir: Option<PathBuf>,
}

/// Build a throwaway `CLAUDE_CONFIG_DIR` for an incognito session: auth/settings/
/// config are symlinked in for parity, but conversation traces (projects/,
/// history.jsonl, sessions/, shell-snapshots/, file-history/) are NOT — so claude
/// writes them fresh inside this dir, and they vanish when it's deleted on close.
/// Where incognito config dirs live: under `~/.cache` (XDG_CACHE_HOME), NOT /tmp.
/// /tmp is reaped by the OS (systemd-tmpfiles ages files out, reboots wipe it),
/// which would corrupt a session left open for days; the cache dir is durable for
/// as long as the app runs, and we delete it on close and sweep it on startup.
fn incognito_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|c| c.join("mandor").join("incognito"))
}

#[cfg(unix)]
fn setup_incognito_config_dir(session_id: &str) -> Option<PathBuf> {
    use std::os::unix::fs::symlink;
    let real = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    let dir = incognito_base()?.join(session_id);
    std::fs::create_dir_all(&dir).ok()?;
    for entry in [
        ".credentials.json",
        "settings.json",
        "settings.local.json",
        "commands",
        "agents",
        "skills",
    ] {
        let src = real.join(entry);
        if src.exists() {
            let _ = symlink(&src, dir.join(entry));
        }
    }
    // plugins/: COPY, not symlink — claude writes GC bookkeeping into it during a
    // session (.in_use/<pid> markers, installed_plugins.json), which through a
    // symlink would land in the real config. A copy keeps plugins working while
    // isolating those writes.
    let plugins = real.join("plugins");
    if plugins.is_dir() {
        let _ = copy_dir_all(&plugins, &dir.join("plugins"));
    }
    // Global memory (CLAUDE.md): COPY, not symlink — the session reads the current
    // instructions, but any memory it writes (`#` add, /memory) stays in the
    // throwaway copy instead of leaking into the real ~/.claude/CLAUDE.md.
    let claude_md = real.join("CLAUDE.md");
    if claude_md.exists() {
        let _ = std::fs::copy(&claude_md, dir.join("CLAUDE.md"));
    }
    // `.claude.json` (onboarding, account, MCP servers, per-project config) is read
    // from $CLAUDE_CONFIG_DIR/.claude.json — without it the session looks brand new
    // (re-onboarding, no account). COPY it, don't symlink: the session gets the full
    // config, but its writes (incl. typed-prompt history) stay in the throwaway copy.
    let claude_json = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join(".claude.json"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude.json")));
    if let Some(src) = claude_json {
        if src.exists() {
            let _ = std::fs::copy(&src, dir.join(".claude.json"));
        }
    }
    Some(dir)
}

#[cfg(not(unix))]
fn setup_incognito_config_dir(_session_id: &str) -> Option<PathBuf> {
    None
}

/// Recursively copy a directory (files copied preserving mode; nested symlinks
/// recreated as symlinks, not followed). Used to bridge `plugins/` into an
/// incognito config dir so its writes don't leak into the real config.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_symlink() {
            #[cfg(unix)]
            if let Ok(target) = std::fs::read_link(&from) {
                let _ = std::os::unix::fs::symlink(target, &to);
            }
        } else if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Remove any leftover incognito config dirs at startup. No incognito PTY survives
/// a process restart, so a leftover dir is crash residue (a clean close/exit/quit
/// already deletes it) — sweep it so a crash can't leave a transcript behind.
pub fn sweep_incognito_dirs() {
    if let Some(base) = incognito_base() {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                remove_incognito_dir(&entry.path());
            }
        }
    }
    // Legacy location: older builds used /tmp/mandor-incognito-*.
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("mandor-incognito-")
            {
                remove_incognito_dir(&entry.path());
            }
        }
    }
}

/// Delete an incognito config dir. Symlinked entries are UNLINKED (never followed)
/// so the real `~/.claude` targets are untouched; only claude's fresh trace files
/// created inside the dir are removed recursively.
fn remove_incognito_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_symlink = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                let _ = std::fs::remove_file(&path); // unlink the symlink, not its target
            } else if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

type Sessions = Arc<Mutex<HashMap<String, PtySession>>>;

#[derive(Clone, Default)]
pub struct PtyState {
    sessions: Sessions,
    // Override for the `claude` executable (None / empty = resolve on PATH).
    claude_bin: Arc<Mutex<Option<String>>>,
}

impl PtyState {
    /// Kill every child on app exit (wired from `RunEvent::ExitRequested`).
    pub fn kill_all(&self) {
        if let Ok(mut map) = self.sessions.lock() {
            for (_, mut session) in map.drain() {
                let _ = session.child.kill();
                if let Some(dir) = session.incognito_dir.take() {
                    remove_incognito_dir(&dir);
                }
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
    incognito: bool,
}

/// Spawn interactive `claude` in a PTY under `cwd` and start streaming its output.
///
/// Flags mirror the interactive `claude` CLI (not mandor's headless engine):
/// - `resume`: continue a session (`--resume <id>`).
/// - `name`: display name recorded in claude's metadata (`-n`), so it shows in
///   `/resume` and the desktop app — only for fresh sessions (resume keeps the
///   name claude already stored).
/// - `worktree`: let claude create a git worktree for the session (`-w [name]`),
///   named after the session. This is claude's own feature — no git plumbing here.
/// - `session_id`: force claude's session id (`--session-id <uuid>`) so our PTY
///   handle id equals the claude session id — lets us `--resume` it after a full
///   app restart. Only for fresh sessions; mutually exclusive with `resume`.
/// - `model`: model alias/name (`--model <m>`), e.g. "opus"/"sonnet"/"haiku".
/// - `remote_control`: register with the Claude app for phone/web access
///   (`--remote-control`). Only pass this for FRESH sessions — combined with
///   `--resume` the CLI tries to reattach to the prior (dead) registration and
///   fails; a resumed session re-enables it in-session via `/remote-control`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn open_pty(
    state: State<PtyState>,
    app: AppHandle,
    id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    resume: Option<String>,
    name: Option<String>,
    worktree: bool,
    session_id: Option<String>,
    model: Option<String>,
    remote_control: bool,
    incognito: bool,
) -> Result<(), String> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let name = name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);

    let bin = state
        .claude_bin
        .lock()
        .ok()
        .and_then(|b| b.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "claude".to_string());

    let mut cmd = CommandBuilder::new(bin);
    cmd.cwd(&cwd);
    // Inherited env carries our merged PATH; TERM isn't inherited on a GUI launch.
    cmd.env("TERM", "xterm-256color");
    // Incognito: run against a throwaway config dir so nothing lands in ~/.claude.
    let incognito_dir = if incognito {
        let dir = setup_incognito_config_dir(&id);
        if let Some(ref dir) = dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        dir
    } else {
        None
    };
    if resume.is_none() {
        if let Some(display_name) = &name {
            cmd.arg("-n");
            cmd.arg(display_name);
        }
    }
    if worktree {
        cmd.arg("-w");
        if let Some(wt_name) = &name {
            cmd.arg(wt_name);
        }
    }
    if let Some(m) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        cmd.arg("--model");
        cmd.arg(m);
    }
    if remote_control {
        cmd.arg("--remote-control");
    }
    if let Some(sid) = &session_id {
        cmd.arg("--session-id");
        cmd.arg(sid);
    }
    if let Some(resume_id) = &resume {
        cmd.arg("--resume");
        cmd.arg(resume_id);
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
        // Sole teardown site: drop the session and reap the child (avoids a
        // zombie — the OS keeps an exited child until it's waited on).
        if let Ok(mut map) = batch_sessions.lock() {
            if let Some(mut session) = map.remove(&batch_id) {
                let _ = session.child.wait();
                if let Some(dir) = session.incognito_dir.take() {
                    remove_incognito_dir(&dir);
                }
            }
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
            incognito_dir,
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
    // Only signal: killing closes the PTY, the reader hits EOF, and the batcher
    // thread removes the entry and reaps the child.
    let mut map = state.sessions.lock().map_err(|e| e.to_string())?;
    if let Some(session) = map.get_mut(&id) {
        let _ = session.child.kill();
    }
    Ok(())
}

/// Set (or clear, with None/empty) the `claude` executable path.
#[tauri::command]
pub fn set_claude_path(state: State<PtyState>, path: Option<String>) {
    if let Ok(mut bin) = state.claude_bin.lock() {
        *bin = path.filter(|s| !s.trim().is_empty());
    }
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
                    incognito: session.incognito_dir.is_some(),
                })
                .collect()
        })
        .unwrap_or_default()
}
