# mandor-term

A fast, light desktop GUI for managing **real interactive `claude` terminal
sessions**. Sibling to [`mandor`](../mandor): instead of driving the headless
`claude -p` engine, each session is a pseudo-terminal (PTY) running the
interactive `claude` CLI, rendered with `xterm.js` — so you get native
permission prompts, slash commands, `/resume`, and `--remote-control` for free.

**Stack:** Tauri v2 (Rust) + vanilla JS/CSS (no bundler) + `xterm.js` (vendored)
+ `portable-pty`.

> **Status: early prototype / scaffolding.** See `HANDOFF.md` for the build plan
> and current state, and `CLAUDE.md` for constraints and hard-won lessons carried
> over from mandor.

## Develop

```bash
bun install
bun run tauri dev
```
