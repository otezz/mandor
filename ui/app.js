const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

// id -> { id, cwd, name, groupId, term, fit, el, exited }
const sessions = new Map();
const sessionOrder = []; // session ids in display order
let activeId = null;

// groups: [{ id, name, collapsed, dir }]; a session's groupId points at one.
let groups = [];
let ungroupedCollapsed = false;
// { [sessionId]: { groupId, name } } — restored onto reconnected PTYs by id.
let savedMeta = {};

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

// --- persistence (localStorage; PTY liveness is the backend's source of truth) ---
const STORE_KEY = "mandor-term.sidebar";

function persist() {
  const sessionMeta = {};
  for (const [id, s] of sessions) {
    sessionMeta[id] = { groupId: s.groupId ?? null, name: s.name };
  }
  try {
    localStorage.setItem(
      STORE_KEY,
      JSON.stringify({ groups, ungroupedCollapsed, sessionMeta }),
    );
  } catch (e) {
    console.error(e);
  }
}

function loadStore() {
  try {
    const data = JSON.parse(localStorage.getItem(STORE_KEY) || "{}");
    groups = Array.isArray(data.groups) ? data.groups : [];
    ungroupedCollapsed = !!data.ungroupedCollapsed;
    savedMeta = data.sessionMeta || {};
  } catch (e) {
    console.error(e);
  }
}

// --- groups ---
function autoGroupFor(cwd) {
  if (!cwd) return null;
  for (const g of groups) {
    if (g.dir && (cwd === g.dir || cwd.startsWith(g.dir + "/"))) return g.id;
  }
  return null;
}

function uniqueGroupName(base) {
  base = (base || "New group").trim();
  const taken = new Set(groups.map((g) => g.name));
  if (!taken.has(base)) return base;
  let n = 2;
  while (taken.has(`${base} ${n}`)) n++;
  return `${base} ${n}`;
}

function createGroup(name = "New group", dir = null) {
  const g = {
    id: crypto.randomUUID(),
    name: uniqueGroupName(name),
    collapsed: false,
    dir,
  };
  groups.push(g);
  persist();
  renderSessionList();
  return g;
}

function createGroupAndRename() {
  const g = createGroup();
  const el = listEl.querySelector(
    `.group-header[data-group="${g.id}"] .group-name`,
  );
  if (el) startRenameGroup(g, el);
}

function deleteGroup(id) {
  const i = groups.findIndex((g) => g.id === id);
  if (i === -1) return;
  groups.splice(i, 1);
  for (const s of sessions.values()) if (s.groupId === id) s.groupId = null;
  persist();
  renderSessionList();
}

function moveSession(id, groupId) {
  const s = sessions.get(id);
  if (!s) return;
  s.groupId = groupId;
  persist();
  renderSessionList();
}

async function setGroupDir(g) {
  try {
    const dir = await invoke("plugin:dialog|open", {
      options: {
        directory: true,
        title: `Auto-add sessions started under… (for "${g.name}")`,
        defaultPath: g.dir || undefined,
      },
    });
    if (dir) {
      g.dir = typeof dir === "string" ? dir : dir?.path;
      persist();
      renderSessionList();
    }
  } catch {
    /* cancelled */
  }
}

// --- sidebar rendering ---
function renderSessionList() {
  listEl.replaceChildren();
  const grouped = groups.length > 0;
  const claimed = new Set();

  for (const g of groups) {
    listEl.appendChild(buildGroupHeader(g));
    const members = sessionOrder.filter(
      (id) => sessions.get(id)?.groupId === g.id,
    );
    members.forEach((id) => claimed.add(id));
    if (!g.collapsed) {
      for (const id of members) listEl.appendChild(buildRow(sessions.get(id)));
    }
  }

  const ungrouped = sessionOrder.filter(
    (id) => sessions.has(id) && !claimed.has(id),
  );
  if (grouped) listEl.appendChild(buildUngroupedHeader(ungrouped.length));
  if (!grouped || !ungroupedCollapsed) {
    for (const id of ungrouped) listEl.appendChild(buildRow(sessions.get(id)));
  }
}

function buildGroupHeader(g) {
  const header = document.createElement("div");
  header.className = "group-header";
  header.dataset.group = g.id;

  const caret = document.createElement("span");
  caret.className = "group-caret";
  caret.textContent = g.collapsed ? "▸" : "▾";
  const name = document.createElement("span");
  name.className = "group-name";
  name.textContent = g.name;
  if (g.dir) name.title = `auto-adds sessions under ${g.dir}`;
  const count = document.createElement("span");
  count.className = "group-count";
  count.textContent = String(
    [...sessions.values()].filter((s) => s.groupId === g.id).length,
  );
  header.append(caret, name, count);

  header.addEventListener("click", () => {
    if (name.isContentEditable) return;
    g.collapsed = !g.collapsed;
    persist();
    renderSessionList();
  });
  name.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    startRenameGroup(g, name);
  });
  header.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e, "group", g.id);
  });
  return header;
}

function buildUngroupedHeader(count) {
  const header = document.createElement("div");
  header.className = "group-header";
  const caret = document.createElement("span");
  caret.className = "group-caret";
  caret.textContent = ungroupedCollapsed ? "▸" : "▾";
  const name = document.createElement("span");
  name.className = "group-name";
  name.textContent = "Ungrouped";
  const c = document.createElement("span");
  c.className = "group-count";
  c.textContent = String(count);
  header.append(caret, name, c);
  header.addEventListener("click", () => {
    ungroupedCollapsed = !ungroupedCollapsed;
    persist();
    renderSessionList();
  });
  return header;
}

function buildRow(s) {
  const row = document.createElement("div");
  row.className = "session-row" + (s.id === activeId ? " active" : "");
  row.dataset.id = s.id;

  const name = document.createElement("span");
  name.className = "s-name";
  name.textContent = s.name + (s.exited ? " (exited)" : "");
  name.title = s.cwd;
  const close = document.createElement("span");
  close.className = "s-close";
  close.textContent = "✕";
  close.title = "Close session";
  row.append(name, close);

  row.addEventListener("click", (e) => {
    if (e.target === close || name.isContentEditable) return;
    setActive(s.id);
  });
  close.addEventListener("click", (e) => {
    e.stopPropagation();
    closeSession(s.id);
  });
  name.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    startRenameSession(s, name);
  });
  row.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e, "session", s.id);
  });
  return row;
}

// --- inline rename (shared by sessions and groups) ---
function editInline(el, current, commit) {
  el.contentEditable = "true";
  el.spellcheck = false;
  el.focus();
  const range = document.createRange();
  range.selectNodeContents(el);
  const sel = window.getSelection();
  sel.removeAllRanges();
  sel.addRange(range);

  el.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      el.blur();
    } else if (e.key === "Escape") {
      e.preventDefault();
      el.textContent = current;
      el.blur();
    }
  });
  el.addEventListener(
    "blur",
    () => {
      el.contentEditable = "false";
      const v = el.textContent.trim();
      if (v && v !== current) commit(v);
      renderSessionList();
    },
    { once: true },
  );
}

function startRenameSession(s, el) {
  editInline(el, s.name, (v) => {
    s.name = v;
    if (s.id === activeId) titleEl.textContent = v;
    persist();
  });
}

function startRenameGroup(g, el) {
  editInline(el, g.name, (v) => {
    g.name = v;
    persist();
  });
}

// --- right-click context menu ---
let contextMenuEl = null;

function closeContextMenu() {
  contextMenuEl?.remove();
  contextMenuEl = null;
}

function openContextMenu(e, kind, id) {
  closeContextMenu();
  const items = [];

  if (kind === "session") {
    const s = sessions.get(id);
    if (!s) return;
    items.push({
      label: "Rename",
      run: () => {
        const el = listEl.querySelector(
          `.session-row[data-id="${id}"] .s-name`,
        );
        if (el) startRenameSession(s, el);
      },
    });
    items.push({ sep: true });
    if (s.groupId != null)
      items.push({
        label: "Move to Ungrouped",
        run: () => moveSession(id, null),
      });
    for (const g of groups) {
      if (g.id === s.groupId) continue;
      items.push({
        label: `Move to ${g.name}`,
        run: () => moveSession(id, g.id),
      });
    }
    items.push({
      label: "New group with this…",
      run: () => {
        const g = createGroup();
        moveSession(id, g.id);
      },
    });
    items.push({ sep: true });
    items.push({
      label: "Close session",
      danger: true,
      run: () => closeSession(id),
    });
  } else if (kind === "group") {
    const g = groups.find((x) => x.id === id);
    if (!g) return;
    items.push({
      label: "Rename",
      run: () => {
        const el = listEl.querySelector(
          `.group-header[data-group="${id}"] .group-name`,
        );
        if (el) startRenameGroup(g, el);
      },
    });
    items.push({
      label: g.dir ? "Change auto-add folder…" : "Set auto-add folder…",
      run: () => setGroupDir(g),
    });
    items.push({ sep: true });
    items.push({
      label: "Delete group",
      danger: true,
      run: () => deleteGroup(id),
    });
  } else {
    items.push({ label: "New group", run: createGroupAndRename });
  }

  const menu = document.createElement("div");
  menu.className = "context-menu";
  for (const item of items) {
    if (item.sep) {
      const sep = document.createElement("div");
      sep.className = "context-sep";
      menu.appendChild(sep);
      continue;
    }
    const btn = document.createElement("button");
    btn.textContent = item.label;
    if (item.danger) btn.className = "danger";
    btn.addEventListener("click", () => {
      closeContextMenu();
      item.run();
    });
    menu.appendChild(btn);
  }

  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  const x = Math.min(e.clientX, window.innerWidth - rect.width - 6);
  const y = Math.min(e.clientY, window.innerHeight - rect.height - 6);
  menu.style.left = `${Math.max(6, x)}px`;
  menu.style.top = `${Math.max(6, y)}px`;
  contextMenuEl = menu;
}

listEl.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  openContextMenu(e, "sidebar", null);
});
window.addEventListener("blur", closeContextMenu);

// --- session lifecycle ---
function setActive(id) {
  activeId = id;
  for (const [sid, s] of sessions) {
    s.el.style.display = sid === id ? "block" : "none";
  }
  for (const row of listEl.querySelectorAll(".session-row")) {
    row.classList.toggle("active", row.dataset.id === id);
  }
  const s = sessions.get(id);
  emptyEl.style.display = sessions.size ? "none" : "flex";
  titleEl.textContent = s ? s.name : "";
  if (s) {
    s.fit.fit();
    s.term.focus();
  }
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

  const meta = savedMeta[id] || {};
  const session = {
    id,
    cwd,
    name: meta.name || baseName(cwd),
    groupId: meta.groupId ?? autoGroupFor(cwd),
    term,
    fit,
    el,
    exited: false,
  };
  sessions.set(id, session);
  if (!sessionOrder.includes(id)) sessionOrder.push(id);

  term.onData((data) => invoke("write_pty", { id, data }).catch(() => {}));
  term.onResize(({ cols, rows }) =>
    invoke("resize_pty", { id, cols, rows }).catch(() => {}),
  );
  renderSessionList();
  persist();
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
    renderSessionList();
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
  sessions.delete(id);
  const oi = sessionOrder.indexOf(id);
  if (oi !== -1) sessionOrder.splice(oi, 1);
  delete savedMeta[id];

  if (activeId === id) {
    activeId = null;
    const next = sessionOrder.find((sid) => sessions.has(sid)) ?? null;
    if (next) setActive(next);
    else {
      emptyEl.style.display = "flex";
      titleEl.textContent = "";
    }
  }
  renderSessionList();
  persist();
}

// Reattach to PTYs that survived a webview reload. Output routes by id (same id
// → same terminal); the fit on activation nudges a SIGWINCH so claude repaints.
async function reconnect() {
  loadStore();
  let running = [];
  try {
    running = await invoke("running_ptys");
  } catch (e) {
    console.error(e);
  }
  for (const { id, cwd } of running) createSession(id, cwd);
  const alive = new Set(running.map((r) => r.id));
  for (const k of Object.keys(savedMeta))
    if (!alive.has(k)) delete savedMeta[k];
  persist();
  renderSessionList();
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
  renderSessionList();
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
  if (e.key !== "Escape") return;
  if (!resumeModal.hidden) closeResume();
  closeContextMenu();
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
  closeContextMenu();
});
appMenu.addEventListener("click", (e) => {
  const action = e.target.closest("button")?.dataset.action;
  if (!action) return;
  appMenu.hidden = true;
  if (action === "new") newSession();
  else if (action === "resume") openResume();
  else if (action === "group") createGroupAndRename();
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
document
  .getElementById("new-group")
  .addEventListener("click", createGroupAndRename);
invoke("is_dev").then((dev) => document.body.classList.toggle("dev", !!dev));

reconnect();
