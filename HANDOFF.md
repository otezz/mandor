# mandor-term — handoff

> Prototype started, then handed off. This note has everything to build it from
> the current state. Read `CLAUDE.md` first (constraints + hard-won lessons).

## Goal

A Tauri desktop app that manages **per-session interactive `claude` terminals**.
Think **mandor's GUI shell, but each session is a real PTY running the
interactive `claude` CLI**, rendered with `xterm.js` — not the headless
stream-json engine mandor uses.

**Why this instead of mandor:** interactive `claude` gives full CLI fidelity for
free — native permission prompts, slash commands, `/resume`, output styling, and
`--remote-control` (verified: remote-control only registers with the web/app in
*interactive* mode; mandor's headless `-p` mode accepts the flag but it's inert,
so mandor sessions can't reach your phone. A PTY-hosted interactive session
*can*).

## Decision log

- **Per-session claude PTYs** (chosen) vs **host a single zellij session**
  (rejected). Per-session gives a mandor-like GUI that owns the session sidebar;
  each session is its own terminal running `claude`. The zellij-host option was
  thinner but duplicated the user's existing `zellij-vtabs` sidebar and made the
  GUI just a terminal window.
- Terminal emulator: **xterm.js**, vendored (no bundler). PTY: **portable-pty**.
- Not building: mandor's MCP permission server / stream-json parsing / worktree
  command wiring — interactive claude handles permissions itself, and worktrees
  can be created by just spawning `claude` in a worktree dir (or `claude -w`).

## Current state (what's already done)

- `bun init` in `/mnt/d/codes/personal/mandor-term`. `package.json`, `bun.lock`,
  `.gitignore` exist. Removed bun's `index.ts`/`tsconfig.json` (not a TS project).
- Installed (dev deps): `@xterm/xterm`, `@xterm/addon-fit`, `@tauri-apps/cli`.
- **Vendor files to copy** (they're in `node_modules`, NOT yet in `ui/vendor/`):
  - `node_modules/@xterm/xterm/lib/xterm.js`  (UMD — exposes global `Terminal`)
  - `node_modules/@xterm/xterm/css/xterm.css`
  - `node_modules/@xterm/addon-fit/lib/addon-fit.js`  (exposes `FitAddon`)
- **Nothing else exists yet** — no `src-tauri/`, no `ui/`, no Tauri config.
- Reference material next door: **`../mandor`** (real Tauri v2 patterns: custom
  titlebar + tray in `src-tauri/src/main.rs`, PATH-merge for GUI launches
  `ensure_tools_on_path`, window-state plugin, capabilities, vendored-lib
  loading in `ui/index.html`, session-switch/perf fixes). **`../claude-sessions/cs`**
  (session discovery: jq over `~/.claude/projects/**` for cwd + first-message
  preview — reuse for the resume picker).

## Build plan (suggested order)

### 0. Tooling first (guardrails before code)
- `.editorconfig` and `.gitignore` already present — verify.
- Add Prettier: `bun add -d prettier`; `.prettierignore` = `ui/vendor/`,
  `src-tauri/target/`, `node_modules/`, `bun.lock`.
- `package.json` scripts: `tauri`, `dev` (`tauri dev`),
  `fmt` (`prettier --write ui && cargo fmt --manifest-path src-tauri/Cargo.toml`),
  `fmt:check` (checks), `prepare` (`git config core.hooksPath .githooks`).
- `.githooks/pre-commit` → runs `bun run --silent fmt:check`, blocks on failure,
  guarded so it no-ops if `bun` is missing. `chmod +x` it and run
  `git config core.hooksPath .githooks`. (Only add the hook AFTER `src-tauri`
  exists, or `cargo fmt --check` in the hook will fail.)

### 1. Tauri scaffold
- `git init` (own identity — never override the user's git config).
- Create `src-tauri/` (Cargo.toml, build.rs, tauri.conf.json, capabilities/,
  icons, `src/main.rs`, `src/pty.rs`). Mirror `../mandor/src-tauri` structure;
  copy its `icons/` to start. Tauri v2, `identifier` e.g. `com.otezz.mandor-term`.
  Carry over mandor's `ensure_tools_on_path()` (GUI launches don't inherit the
  shell PATH, so `claude` won't be found otherwise).
- `bun run tauri dev` should open an empty window before wiring the PTY.

### 2. PTY backend (`src-tauri/src/pty.rs`)
Rust dep: `portable-pty` (check crates.io for the current version). State:
`Mutex<HashMap<String, PtySession>>` where `PtySession` holds the `master`
(`Box<dyn MasterPty>`), a `Box<dyn Write>` writer, and the child. Command/event
contract (all commands `async` where they touch blocking work):

- `open_pty(id, cwd, cols, rows)` — `native_pty_system().openpty(size)`, build
  `CommandBuilder::new("claude")` (later: args like `--resume <id>`), `cmd.cwd(cwd)`,
  `pair.slave.spawn_command(cmd)`. Keep `master` + writer + child in state. Spawn
  a **dedicated reader thread**: read from `master.try_clone_reader()` in a loop,
  **coalesce** output (buffer ~4–8ms or up to ~16KB), base64-encode, and
  `app.emit("pty-output", { id, b64 })`. On EOF, `app.emit("pty-exit", { id })`.
- `write_pty(id, data: String)` — write UTF-8 bytes to the session's writer.
- `resize_pty(id, cols, rows)` — `master.resize(PtySize{ rows, cols, .. })`.
- `close_pty(id)` — kill child, drop the entry.
- `running_ptys()` — `Vec<{ id }>` for reconnect-on-reload.

Gotchas: reader on a thread (never main); spawn via `spawn_blocking` if it ever
blocks; kill children on app exit (`RunEvent::Exit`, like mandor's `kill_all`).

### 3. Frontend (`ui/`)
- `index.html`: load vendored `vendor/xterm.js`, `vendor/addon-fit.js`, link
  `vendor/xterm.css`; then `app.js` (module, defer). No CDNs.
- `app.js`:
  - Per session: `new Terminal(...)` + `FitAddon`; `term.open(divForSession)`.
  - `term.onData(d => invoke("write_pty", { id, data: d }))`.
  - `listen("pty-output", ({payload}) => term.write(base64ToBytes(payload.b64)))`
    (decode to `Uint8Array`; `term.write` accepts it — preserves partial UTF-8).
  - `FitAddon.fit()` on container resize (ResizeObserver) →
    `invoke("resize_pty", { id, cols: term.cols, rows: term.rows })`.
  - Sidebar (mandor-like): session list + "New session" (folder dialog →
    `open_pty` in that dir) + later a "Resume" picker (reuse `cs` discovery via a
    `list_sessions` command or a jq call). Switching sessions shows that
    session's terminal div; keep inactive terminals detached/hidden (perf).
- `styles.css`: mandor's palette/titlebar are a good starting point; keep it lean.

### 4. Polish (later, not for the first cut)
Custom titlebar + tray (port from mandor), resume picker, close/kill action,
reconnect-to-running-PTYs on reload, remember window geometry
(`tauri-plugin-window-state`), theme sync between the app and xterm.

## First-cut definition of done (minimal)

App opens → "New session" picks a folder → a PTY runs interactive `claude` there
→ xterm renders it, keystrokes work, resize works → a second session can be
created and switched to. Everything else is iteration.

## Verify-before-done checklist

- `cargo check` + `bun build ui/app.js --target=browser` clean.
- `bun run tauri dev`: type in the terminal, see claude respond; a permission
  prompt appears **in the terminal** and is answerable; resize reflows.
- The reader thread coalesces (no event-per-byte); switching sessions is instant.
