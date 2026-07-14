# mandor-term

A fast, light desktop GUI for managing **real interactive `claude` terminal
sessions** — a sibling to [`mandor`](../mandor), built the opposite way.

- **mandor** drives the *headless* engine (`claude -p` stream-json) and
  re-renders the conversation itself.
- **mandor-term** runs the *real* interactive `claude` CLI in a pseudo-terminal
  (PTY) per session and renders it with a terminal emulator. You get full CLI
  fidelity for free: native permission prompts, slash commands, `/resume`,
  themes, and `--remote-control` (which actually works, because the session is
  interactive — see the note in HANDOFF.md).

The GUI is the chrome + session management (a mandor-like sidebar of sessions,
new/resume); each session is its own PTY running interactive `claude`.

## Stack

- **Tauri v2** (Rust backend). PTYs via **`portable-pty`** (wezterm's crate).
- **Frontend: vanilla JS + CSS, no bundler, no build step.** Terminal rendering
  via **`xterm.js`**, vendored into `ui/vendor/` (like mandor vendors
  marked/mermaid/highlight) — served as-is, CSP-safe.

## Hard constraints (same discipline as mandor)

- **Frontend is vanilla JS/CSS, no bundler, no framework.** `ui/` is served
  directly. Do not add a bundler or framework. Vendored libraries live in
  `ui/vendor/` as local files (no CDNs); everything must be CSP-safe
  (local/inline only — a strict CSP blocks external hosts).
- **Prefer `bun`** over npm for JS deps/scripts.
- Reuse existing patterns/helpers before adding new ones; make minimal, focused
  changes; keep comments minimal and self-explanatory.

## Architecture

- `src-tauri/` — Rust: a `pty` module owns per-session PTYs (spawn interactive
  `claude` in a chosen cwd, stream output, accept input, resize, close). See
  HANDOFF.md for the command/event contract.
- `ui/` — served as-is (`index.html`, `app.js`, `styles.css`, `vendor/`). A
  mandor-style session sidebar; one `xterm.js` terminal per session; switching
  shows the active session's terminal.
- Sessions live in the **shared** Claude store — `~/.claude/projects/<cwd with
  '/' and '.' turned into '-'>/<session-id>.jsonl` — so `claude --resume <id>`,
  mandor, and the real CLI all see the same sessions. Session discovery (for a
  resume picker) is the jq-over-transcripts approach prototyped in
  [`../claude-sessions/cs`](../claude-sessions/cs).

## Development

```bash
bun run dev                             # dev app (own identifier/config namespace + amber titlebar; Rust auto-rebuilds, Ctrl+R reloads UI)
bun build ui/app.js --target=browser    # syntax-check the frontend after edits
cargo check --manifest-path src-tauri/Cargo.toml
```

- After a **frontend** change: syntax-check with the `bun build` line, then
  Ctrl+R in the running app.
- After a **backend** (Rust) change: the dev server auto-rebuilds.

## Formatting (carry over from mandor — set up first, see HANDOFF.md)

- **Rust → `cargo fmt`** (rustfmt defaults). **Frontend → Prettier** (defaults)
  over `ui/`, with `ui/vendor/` ignored.
- Scripts: `bun run fmt` / `bun run fmt:check`. A committed `.githooks/pre-commit`
  runs `fmt:check`, wired via `core.hooksPath` from a `package.json` `prepare`
  script (no Husky). `.editorconfig` covers editors.

## Release build

```bash
bun run tauri build --bundles deb
```

Bump `version` in **both** `tauri.conf.json` and `src-tauri/Cargo.toml` (and
`Cargo.lock` via `cargo update -p <pkg> --precise <v>`) before a rebuild. No
auto-update.

## Working agreements (same as mandor)

- **Commit/push only when asked.** Before committing, self-review and run a
  **leak scan** of the diff for personal/employer identifiers and ticket codes.
  Never commit work identifiers or ticket keys. This repo reads your real
  `~/.claude` store at *runtime* (which contains work paths) — that's fine, but
  never *hardcode or commit* those; use neutral placeholders in committed code,
  tests, docs, and commit messages.
- **Never run `sudo` directly** — hand the user the command to run themselves.
- After a `.deb` build, print the full copy/paste reinstall line.
- **Think twice, code once**; fix root causes not symptoms; verify before
  claiming done (run the build/command; report what you observed).

## Hard-won lessons from mandor (READ THIS — they bite here too)

1. **Blocking work in a Tauri command freezes the UI/IPC.** A *sync* command that
   runs a subprocess or network/fs call blocks the main thread, and the *next*
   `invoke` stalls for that whole duration — it presents as lag everywhere.
   (mandor's session-switch lag was a sync `gh`/`git` command.) **Make such
   commands `async` and run the blocking part in
   `tauri::async_runtime::spawn_blocking`.** PTY reads MUST be on a dedicated
   thread, never the main thread.
2. **PTY output is a high-frequency stream — batch it.** Don't emit a Tauri event
   per read; coalesce (buffer a few ms or up to N KB, then emit once). WebKitGTK
   IPC cost is dominated by per-message overhead; a chatty stream janks. Emit
   **bytes** (base64) so partial UTF-8 isn't corrupted — `xterm.write()` accepts
   byte chunks.
3. **Keep only the active view heavy in the DOM.** mandor's switch lag was partly
   every session's transcript mounted at once. Here: one xterm instance per
   session, but only the active terminal attached/visible; detach/hide inactive
   ones so switching stays cheap.
4. **Interactive vs headless.** The whole point: interactive `claude` in a PTY
   handles permissions, slash commands, and `--remote-control` natively — you do
   NOT need mandor's in-process MCP permission server. Don't reimplement it.
5. **Reconnect on reload.** On a webview reload the Rust backend and its PTYs
   survive; reconnect the frontend to running PTYs (reuse the session id so
   events keep routing) rather than rebuilding cold.
