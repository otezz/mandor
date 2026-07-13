const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

// id -> { cwd, term, fit, el, li, exited }
const sessions = new Map();
let activeId = null;

const listEl = document.getElementById("session-list");
const termsEl = document.getElementById("terminals");
const emptyEl = document.getElementById("empty");
const titleEl = document.getElementById("titlebar-title");

function base64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

function baseName(cwd) {
  const parts = cwd.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || cwd;
}

// --- theme: keep xterm colors in sync with the chrome (light/dark) ---
function xtermTheme() {
  const cs = getComputedStyle(document.body);
  const v = (name) => cs.getPropertyValue(name).trim();
  return {
    background: v("--term-bg"),
    foreground: v("--term-fg"),
    cursor: v("--term-cursor"),
    selectionBackground: v("--term-selection"),
  };
}

// --- session lifecycle ---
function setActive(id) {
  activeId = id;
  for (const [sid, s] of sessions) {
    const on = sid === id;
    s.el.style.display = on ? "block" : "none";
    if (s.li) s.li.classList.toggle("active", on);
  }
  const s = sessions.get(id);
  emptyEl.style.display = sessions.size ? "none" : "flex";
  titleEl.textContent = s ? baseName(s.cwd) : "";
  if (s) {
    s.fit.fit();
    s.term.focus();
  }
}

function updateSidebarEntry(id) {
  const s = sessions.get(id);
  if (!s || !s.li) return;
  s.li.classList.toggle("active", id === activeId);
  s.li.replaceChildren();

  const name = document.createElement("span");
  name.className = "session-name";
  name.textContent = baseName(s.cwd) + (s.exited ? " (exited)" : "");
  name.title = s.cwd;

  const close = document.createElement("button");
  close.className = "session-close";
  close.textContent = "×";
  close.title = "Close session";
  close.addEventListener("click", (e) => {
    e.stopPropagation();
    closeSession(id);
  });

  s.li.append(name, close);
}

function addSidebarEntry(id, session) {
  const li = document.createElement("li");
  li.className = "session";
  li.addEventListener("click", () => setActive(id));
  session.li = li;
  listEl.appendChild(li);
  updateSidebarEntry(id);
}

// Build the xterm + DOM + sidebar entry for a session. Does NOT open a PTY —
// callers open (new/resume) or reconnect to an already-running one.
function createSession(id, cwd) {
  const el = document.createElement("div");
  el.className = "terminal-host";
  el.style.display = "none";
  termsEl.appendChild(el);

  const term = new Terminal({
    fontFamily: "'JetBrains Mono', 'Fira Code', ui-monospace, monospace",
    fontSize: 13,
    cursorBlink: true,
    scrollback: 5000,
    allowProposedApi: true,
    theme: xtermTheme(),
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(el);

  const session = { cwd, term, fit, el, li: null, exited: false };
  sessions.set(id, session);
  addSidebarEntry(id, session);

  term.onData((data) => invoke("write_pty", { id, data }).catch(() => {}));
  term.onResize(({ cols, rows }) =>
    invoke("resize_pty", { id, cols, rows }).catch(() => {}),
  );
  return session;
}

async function startSession(cwd, resume) {
  const id = crypto.randomUUID();
  const session = createSession(id, cwd);
  setActive(id);
  try {
    await invoke("open_pty", {
      id,
      cwd,
      cols: session.term.cols,
      rows: session.term.rows,
      resume: resume ?? null,
    });
  } catch (e) {
    session.term.writeln(`\r\n\x1b[31mFailed to start claude: ${e}\x1b[0m`);
    session.exited = true;
    updateSidebarEntry(id);
  }
}

async function newSession() {
  let cwd;
  try {
    cwd = await invoke("plugin:dialog|open", {
      options: {
        directory: true,
        multiple: false,
        title: "New claude session in…",
      },
    });
  } catch (e) {
    console.error(e);
    return;
  }
  if (!cwd) return;
  cwd = typeof cwd === "string" ? cwd : cwd.path;
  await startSession(cwd, null);
}

async function closeSession(id) {
  const s = sessions.get(id);
  if (!s) return;
  try {
    await invoke("close_pty", { id });
  } catch (e) {
    console.error(e);
  }
  s.term.dispose();
  s.el.remove();
  s.li?.remove();
  sessions.delete(id);

  if (activeId === id) {
    activeId = null;
    const next = sessions.keys().next().value ?? null;
    if (next) setActive(next);
    else {
      emptyEl.style.display = "flex";
      titleEl.textContent = "";
    }
  }
}

// Reattach to PTYs that survived a webview reload. Output routes by id (same id
// → same terminal); the fit on activation nudges a SIGWINCH so claude repaints.
async function reconnect() {
  let running = [];
  try {
    running = await invoke("running_ptys");
  } catch (e) {
    console.error(e);
  }
  for (const { id, cwd } of running) createSession(id, cwd);
  if (running.length) setActive(running[0].id);
}

// --- PTY event routing ---
listen("pty-output", ({ payload }) => {
  const s = sessions.get(payload.id);
  if (s) s.term.write(base64ToBytes(payload.b64));
});

listen("pty-exit", ({ payload }) => {
  const s = sessions.get(payload.id);
  if (!s || s.exited) return;
  s.exited = true;
  updateSidebarEntry(payload.id);
  s.term.write("\r\n\x1b[90m[claude exited]\x1b[0m\r\n");
});

const resizeObserver = new ResizeObserver(() => {
  const s = sessions.get(activeId);
  if (s) s.fit.fit();
});
resizeObserver.observe(termsEl);

const schemeQuery = window.matchMedia("(prefers-color-scheme: dark)");
schemeQuery.addEventListener("change", () => {
  const theme = xtermTheme();
  for (const s of sessions.values()) s.term.options.theme = theme;
});

// --- resume picker ---
const resumeModal = document.getElementById("resume-modal");
const resumeList = document.getElementById("resume-list");
const resumeFilter = document.getElementById("resume-filter");
const resumeEmpty = document.getElementById("resume-empty");
let resumeItems = [];

function renderResume(query) {
  const q = query.toLowerCase();
  const filtered = resumeItems.filter(
    (s) =>
      s.cwd.toLowerCase().includes(q) || s.preview.toLowerCase().includes(q),
  );
  resumeList.replaceChildren();
  for (const s of filtered) {
    const li = document.createElement("li");
    li.className = "resume-item";

    const folder = document.createElement("div");
    folder.className = "r-folder";
    folder.textContent = baseName(s.cwd);
    const path = document.createElement("div");
    path.className = "r-path";
    path.textContent = s.cwd;
    const preview = document.createElement("div");
    preview.className = "r-preview";
    preview.textContent = s.preview;

    li.append(folder, path, preview);
    li.addEventListener("click", () => {
      closeResume();
      startSession(s.cwd, s.id);
    });
    resumeList.appendChild(li);
  }
  resumeEmpty.hidden = filtered.length > 0;
  if (!filtered.length) {
    resumeEmpty.textContent = resumeItems.length
      ? "No matches."
      : "No sessions found.";
  }
}

async function openResume() {
  resumeModal.hidden = false;
  resumeList.replaceChildren();
  resumeEmpty.hidden = false;
  resumeEmpty.textContent = "Loading…";
  resumeFilter.value = "";
  try {
    resumeItems = await invoke("list_sessions");
  } catch (e) {
    console.error(e);
    resumeItems = [];
  }
  renderResume("");
  resumeFilter.focus();
}

function closeResume() {
  resumeModal.hidden = true;
}

resumeFilter.addEventListener("input", () => renderResume(resumeFilter.value));
document.getElementById("resume-close").addEventListener("click", closeResume);
document
  .getElementById("resume-backdrop")
  .addEventListener("click", closeResume);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !resumeModal.hidden) closeResume();
});

// --- titlebar: app menu + window controls ---
const appMenu = document.getElementById("app-menu");
const appMenuBtn = document.getElementById("app-menu-btn");

appMenuBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  appMenu.hidden = !appMenu.hidden;
});
document.addEventListener("click", () => {
  appMenu.hidden = true;
});
appMenu.addEventListener("click", (e) => {
  const action = e.target.closest("button")?.dataset.action;
  if (!action) return;
  appMenu.hidden = true;
  if (action === "new") newSession();
  else if (action === "resume") openResume();
  else if (action === "quit") invoke("quit_app");
});

const MAX_ICON =
  '<svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true"><rect x="1" y="1" width="8" height="8" rx="1" fill="none"/></svg>';
const RESTORE_ICON =
  '<svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true"><path d="M3 3V1h6v6H7" fill="none"/><rect x="1" y="3" width="6" height="6" rx="1" fill="none"/></svg>';
const winMax = document.getElementById("win-max");

async function syncMaxIcon() {
  const max = await invoke("is_maximized").catch(() => false);
  winMax.innerHTML = max ? RESTORE_ICON : MAX_ICON;
  winMax.title = max ? "Restore" : "Maximize";
}

document
  .getElementById("win-min")
  .addEventListener("click", () => invoke("minimize_window"));
winMax.addEventListener("click", () =>
  invoke("toggle_maximize").then(syncMaxIcon),
);
document
  .getElementById("win-close")
  .addEventListener("click", () => invoke("hide_to_tray"));

// Drag / double-click-maximize the titlebar. data-tauri-drag-region doesn't work
// on this Wayland setup, so drive it manually: single mousedown starts a drag,
// a double-click (detail === 2) toggles maximize instead.
document.getElementById("titlebar").addEventListener("mousedown", (e) => {
  if (e.button !== 0 || e.target.closest("button, .dropdown-menu")) return;
  if (e.detail === 2) invoke("toggle_maximize").then(syncMaxIcon);
  else appWindow.startDragging();
});
appWindow.onResized(() => syncMaxIcon());
syncMaxIcon();

document.getElementById("new-session").addEventListener("click", newSession);
document.getElementById("resume-session").addEventListener("click", openResume);
invoke("is_dev").then((dev) => document.body.classList.toggle("dev", !!dev));

reconnect();
