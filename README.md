# Mandor

A fast, light desktop GUI for managing **real interactive `claude` terminal
sessions**. Sibling to [`mandor-headless`](../mandor-headless): instead of driving the headless
`claude -p` engine, each session is a pseudo-terminal (PTY) running the
interactive `claude` CLI, rendered with `xterm.js` — so you get native
permission prompts, slash commands, `/resume`, and `--remote-control` for free.

**Stack:** Tauri v2 (Rust) + vanilla JS/CSS (no bundler) + `xterm.js` (vendored)
+ `portable-pty`.

## Features

- Sidebar of sessions with groups, drag-to-reorder, and per-session activity
  (working / needs-attention) indicators.
- New / resume sessions; recent-directory picker; `-w` worktree, `--model`, and
  `--remote-control` options.
- Incognito sessions — run against a throwaway config dir so nothing is written
  to `~/.claude`.
- Themes (Catppuccin flavors, Ghostty Default Dark, custom pasted palettes),
  configurable terminal font (from installed monospace families) and point size,
  optional ligatures.
- Find-in-session, pop-out windows, desktop notifications, and an audible bell.
- A session's PR badge and a "Restart to update" prompt.

## Develop

```bash
bun install
bun run tauri dev    # dev app: separate identifier/config namespace + amber titlebar
```

- Frontend edits: `bun build ui/app.js --target=browser` to syntax-check, then
  Ctrl+R in the running app. Backend edits auto-rebuild.
- Format: `bun run fmt` (Prettier over `ui/`, `cargo fmt` for Rust).

## Build a release

```bash
bun run tauri build --bundles deb
```

See `CLAUDE.md` for project constraints and hard-won lessons, and `HANDOFF.md`
for the command/event contract.

## Credits

- Bell / notification sound: ["Film special effects — short digital notification
  alert"](https://pixabay.com/sound-effects/film-special-effects-short-digital-notification-alert-440353/)
  from Pixabay, used under the [Pixabay Content License](https://pixabay.com/service/license-summary/)
  (free to use, no attribution required). Bundled as `ui/notification.mp3`
  (re-encoded to mono for size).
- Terminal rendering: [`xterm.js`](https://xtermjs.org/) (vendored in `ui/vendor/`).
- Fonts: [JetBrains Mono](https://www.jetbrains.com/lp/mono/) (OFL), used as the
  default terminal font.
