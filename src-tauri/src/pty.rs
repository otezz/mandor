use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    // The window/profile that owns this session (None = default window). Lets each
    // window reconnect only to its own PTYs on reload.
    profile_id: Option<String>,
    // When output last flowed or input was last sent — the one ground-truth idle
    // clock, shared by every connected client (desktop + any remote browser via
    // webserver.rs). Frontend idle-auto-suspend used to guess this per-client from
    // whatever that client happened to observe, which left a freshly-connected
    // client with no history to guess from — see running_ptys' idle_ms.
    last_active: Arc<Mutex<Instant>>,
    // Every currently-known client's own desired (cols, rows) for this session.
    // Multiple clients (desktop + any remote browser) can view and type into the
    // same session at once, but a resize is exclusive — the PTY only has one
    // real size. Absent an explicit pin (below), the applied size is the min
    // across every entry here, so nothing gets cut off for anyone (same idea as
    // tmux/screen sizing a shared session to its smallest attached client). A
    // remote client's entry is removed when its websocket closes (see
    // client_disconnected, called from webserver.rs) so a client that's gone
    // stops constraining everyone else.
    client_sizes: HashMap<String, (u16, u16)>,
    // Explicit "Take over display" override: while Some, only this client's
    // entry in client_sizes drives the real PTY size, ignoring the min-of-all
    // computation above. Cleared automatically when this client disconnects,
    // reverting to auto min-of-all sizing among whoever's left.
    pinned: Option<String>,
    // Epoch ms this PTY was spawned — identifies *this* process. A session id
    // outlives its PTY (restart, resume, suspend then wake all respawn under the
    // same id), so a remote client comparing ids alone can't tell a respawned
    // session from the one it already has terminal output for (see
    // running_ptys' started_at_ms).
    started_at_ms: u64,
}

/// Recompute and apply a session's real PTY size from its current
/// client_sizes/pinned state (see PtySession's doc comments for the rule).
/// A no-op if there's nothing to size against yet (e.g. pinned to a client
/// that hasn't reported a size for this session at all).
fn apply_effective_size(session: &mut PtySession) -> Result<(), String> {
    let size =
        match &session.pinned {
            Some(client_id) => session.client_sizes.get(client_id).copied(),
            None => session.client_sizes.values().copied().reduce(
                |(min_cols, min_rows), (cols, rows)| (min_cols.min(cols), min_rows.min(rows)),
            ),
        };
    let Some((cols, rows)) = size else {
        return Ok(());
    };
    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())
}

/// Build a throwaway `CLAUDE_CONFIG_DIR` for an incognito session: auth/settings/
/// config are symlinked in for parity, but conversation traces (projects/,
/// history.jsonl, sessions/, shell-snapshots/, file-history/) are NOT — so claude
/// writes them fresh inside this dir, and they vanish when it's deleted on close.
/// Where incognito config dirs live: under the platform cache dir (`~/.cache` on
/// Linux, `~/Library/Caches` on macOS), NOT /tmp. /tmp is reaped by the OS
/// (systemd-tmpfiles ages files out, reboots wipe it), which would corrupt a
/// session left open for days; the cache dir is durable for as long as the app
/// runs, and we delete it on close and sweep it on startup.
pub fn mandor_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let cache = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"));
    #[cfg(not(target_os = "macos"))]
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    cache.map(|c| c.join("mandor"))
}

fn incognito_base() -> Option<PathBuf> {
    mandor_cache_dir().map(|c| c.join("incognito"))
}

/// Where persistent per-profile config dirs live. Unlike incognito this is DURABLE
/// (the data dir, not the cache dir) and is NEVER swept — a profile keeps its own
/// login, settings, MCP servers, and session history across restarts.
fn profiles_base() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let data = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(not(target_os = "macos"))]
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        });
    data.map(|d| d.join("mandor").join("profiles"))
}

/// A single profile's config dir, `profiles_base/<id>`. `id` is a UUID from the
/// frontend; reject anything path-like as a traversal guard.
pub fn profile_dir(id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        return None;
    }
    profiles_base().map(|b| b.join(id))
}

/// The real default Claude config dir to seed a new profile from (`$CLAUDE_CONFIG_DIR`
/// if Mandor was launched with one, else `~/.claude`).
fn default_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
}

/// Create a persistent profile config dir, seeding it from the default config so
/// it isn't a blank setup. Everything is COPIED (not symlinked) so the profile is
/// independent and never writes back to `~/.claude`. Credentials are copied only
/// when `copy_login`; otherwise the profile starts logged-out and the user runs
/// `/login` with a different account. Idempotent (won't overwrite existing files).
#[tauri::command]
pub fn create_profile(id: String, copy_login: bool) -> Result<(), String> {
    let dir = profile_dir(&id).ok_or("invalid profile id")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let Some(real) = default_config_dir() else {
        return Ok(()); // nothing to seed from; leave the empty dir
    };
    let mut files = vec![
        "settings.json",
        "settings.local.json",
        ".claude.json",
        "CLAUDE.md",
    ];
    if copy_login {
        files.push(".credentials.json");
    }
    for f in files {
        let (src, dst) = (real.join(f), dir.join(f));
        if src.exists() && !dst.exists() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
    for d in ["commands", "agents", "skills", "plugins"] {
        let (src, dst) = (real.join(d), dir.join(d));
        if src.is_dir() && !dst.exists() {
            let _ = copy_dir_all(&src, &dst);
        }
    }
    Ok(())
}

/// Delete a profile's config dir and everything in it (symlink-safe, so it never
/// follows a link out to the real config).
#[tauri::command]
pub fn delete_profile(id: String) -> Result<(), String> {
    if let Some(dir) = profile_dir(&id) {
        if dir.exists() {
            remove_incognito_dir(&dir);
        }
    }
    Ok(())
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
#[serde(rename_all = "camelCase")]
pub struct RunningPty {
    id: String,
    cwd: String,
    incognito: bool,
    profile_id: Option<String>,
    // Milliseconds since output last flowed or input was last sent — ground truth
    // for idle-auto-suspend, shared by every connected client. See PtySession's
    // `last_active` for why this replaced a per-client guess.
    idle_ms: u64,
    // See PtySession's `started_at_ms`.
    started_at_ms: u64,
}

/// Kebab-case a display name into a valid worktree segment: lowercase, runs of
/// non-alphanumerics collapsed to single dashes, no leading/trailing dash.
/// "Remove Auth0" -> "remove-auth0".
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Spawn interactive `claude` in a PTY under `cwd` and start streaming its output.
///
/// Flags mirror the interactive `claude` CLI:
/// - `resume`: continue a session (`--resume <id>`).
/// - `name`: display name recorded in claude's metadata (`-n`), so it shows in
///   `/resume` and the desktop app — only for fresh sessions (resume keeps the
///   name claude already stored).
/// - `yolo`: run with `--dangerously-skip-permissions`, so claude executes tools
///   without asking. Applies to fresh and resumed sessions alike.
/// - `worktree`: let claude create a git worktree for the session (`-w [name]`),
///   named after the session. This is claude's own feature — no git plumbing here.
/// - `session_id`: force claude's session id (`--session-id <uuid>`) so our PTY
///   handle id equals the claude session id — lets us `--resume` it after a full
///   app restart. Only for fresh sessions; mutually exclusive with `resume`.
/// - `model`: model alias/name (`--model <m>`), e.g. "fable"/"opus"/"sonnet".
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
    profile_id: Option<String>,
    yolo: bool,
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
    // A Finder/Dock launch has no locale either, and without one macOS text tools
    // (pbcopy — which claude uses to copy) read UTF-8 as Mac Roman: "é" → "√©".
    // Like Terminal.app, supply a UTF-8 charset unless the user set a locale.
    #[cfg(target_os = "macos")]
    if ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .all(|v| std::env::var_os(v).is_none())
    {
        cmd.env("LC_CTYPE", "UTF-8");
    }
    // Make claude ring the terminal bell on its notifications so our onBell hook
    // fires (attention indicator, desktop notification, bell sound). Scoped to
    // this session via --settings; the user's global config is untouched, and it
    // works on resume too (unlike --remote-control).
    cmd.arg("--settings");
    cmd.arg(r#"{"preferredNotifChannel":"terminal_bell"}"#);
    // Config dir precedence: incognito (throwaway, deleted on close) wins; else a
    // persistent profile dir if this window is bound to one; else the default
    // (~/.claude, no override). Only the incognito dir is tracked for teardown —
    // a profile dir is durable and must never be deleted here.
    let incognito_dir = if incognito {
        let dir = setup_incognito_config_dir(&id);
        if let Some(ref dir) = dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        dir
    } else {
        if let Some(dir) = profile_id
            .as_deref()
            .filter(|p| !p.is_empty())
            .and_then(profile_dir)
        {
            cmd.env("CLAUDE_CONFIG_DIR", &dir);
        }
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
        // claude requires a worktree name of only letters/digits/dots/underscores/
        // dashes, but the display name can have spaces/caps/punctuation — so pass a
        // kebab-cased slug. If it slugs to empty, drop the arg and let claude name it.
        if let Some(slug) = name.as_deref().map(slugify).filter(|s| !s.is_empty()) {
            cmd.arg(slug);
        }
    }
    if let Some(m) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        cmd.arg("--model");
        cmd.arg(m);
    }
    if remote_control {
        cmd.arg("--remote-control");
    }
    // Opt-in per session: claude runs tools without asking for permission.
    if yolo {
        cmd.arg("--dangerously-skip-permissions");
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
    let last_active = Arc::new(Mutex::new(Instant::now()));

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
    let batch_last_active = last_active.clone();
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
            if let Ok(mut t) = batch_last_active.lock() {
                *t = Instant::now();
            }
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
            profile_id: profile_id.filter(|p| !p.is_empty()),
            last_active,
            client_sizes: HashMap::new(),
            pinned: None,
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        },
    );
    Ok(())
}

#[tauri::command]
pub fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.sessions.lock().map_err(|e| e.to_string())?;
    let session = map.get_mut(&id).ok_or("no such session")?;
    if let Ok(mut t) = session.last_active.lock() {
        *t = Instant::now();
    }
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|e| e.to_string())?;
    session.writer.flush().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn resize_pty(
    state: State<PtyState>,
    id: String,
    cols: u16,
    rows: u16,
    client_id: String,
) -> Result<(), String> {
    let mut map = state.sessions.lock().map_err(|e| e.to_string())?;
    if let Some(session) = map.get_mut(&id) {
        session.client_sizes.insert(client_id.clone(), (cols, rows));
        // Unpinned: every resize feeds the min-of-all computation. Pinned to
        // someone else: still record the size above (so it's ready the moment
        // that pin releases or moves here), but don't touch the real PTY.
        let applies = session.pinned.as_deref().is_none_or(|p| p == client_id);
        if applies {
            apply_effective_size(session)?;
        }
    }
    Ok(())
}

/// Explicit "Take over display": pin this client as the session's sole size
/// driver regardless of the min-of-all computation, until it disconnects (see
/// client_disconnected) or another client takes over instead.
#[tauri::command]
pub fn claim_control(state: State<PtyState>, id: String, client_id: String) -> Result<(), String> {
    let mut map = state.sessions.lock().map_err(|e| e.to_string())?;
    if let Some(session) = map.get_mut(&id) {
        session.pinned = Some(client_id);
        apply_effective_size(session)?;
    }
    Ok(())
}

/// A client disconnected (its websocket closed — see webserver.rs's bridge()).
/// Drop its claimed size from every session and release any pin it held,
/// reverting those sessions to auto min-of-all sizing among whoever's left.
pub fn client_disconnected(state: State<PtyState>, client_id: &str) {
    let Ok(mut map) = state.sessions.lock() else {
        return;
    };
    for session in map.values_mut() {
        let had_size = session.client_sizes.remove(client_id).is_some();
        let was_pinned = session.pinned.as_deref() == Some(client_id);
        if was_pinned {
            session.pinned = None;
        }
        if had_size || was_pinned {
            let _ = apply_effective_size(session);
        }
    }
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
                    profile_id: session.profile_id.clone(),
                    idle_ms: session
                        .last_active
                        .lock()
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0),
                    started_at_ms: session.started_at_ms,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_kebab_cases() {
        assert_eq!(slugify("Remove Auth0"), "remove-auth0");
        assert_eq!(slugify("  fix: the/bug  "), "fix-the-bug");
        assert_eq!(slugify("v1.2.3"), "v1-2-3");
        assert_eq!(slugify("already-kebab"), "already-kebab");
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify(""), "");
    }
}
