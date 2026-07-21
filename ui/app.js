const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

// A pop-out window shows a single session's terminal (?popout=1&id=…). It shares
// the backend + localStorage with the main window, so it must NOT persist.
const POPOUT = new URLSearchParams(location.search).get("popout") === "1";
const POPOUT_ID = new URLSearchParams(location.search).get("id");
if (POPOUT) document.body.classList.add("popout");

// id -> session record. id === the claude session id (forced via --session-id),
// so a session can be resumed after a full app restart.
// record: { id, cwd, name, groupId, spawnMode, wantWorktree, live, exited,
//           term, fit, el }. term/fit/el are null until the session is
//           "materialized" (a live PTY reconnected, or a cold one resumed).
const sessions = new Map();
const sessionOrder = []; // session ids in display order
let activeId = null;

// groups: [{ id, name, collapsed, dir }]; a session's groupId points at one.
let groups = [];
let ungroupedCollapsed = false;
let savedSessions = []; // persisted session records, restored on launch
let savedActiveId = null; // the session that was active last, restored on launch
let recentDirs = []; // recently-used session directories, most recent first
let sidebarCollapsed = false;
let appFocused = true;
let notificationsEnabled = true;
let settings = {
  theme: "frappe",
  defaultCwd: "",
  claudePath: "",
  notifications: true,
  defaultModel: "",
  terminalFontSize: 11, // points (converted to px for xterm), like other terminals
  terminalFont: "JetBrains Mono",
  ligatures: false,
  bellSound: true, // audible beep when a session rings the terminal bell
  bellSoundFile: "", // custom sound file path; "" = built-in synth beep
  resumeOnStart: false, // after a full restart, resume all sessions (staggered)
  remoteByDefault: false,
  customThemes: [],
};

// Terminal font size is in points (like Ghostty et al.); xterm wants px, so
// convert at 96/72. Points keep the number consistent with other terminals.
const FONT_PT_MIN = 6;
const FONT_PT_MAX = 24;
const clampFontPt = (pt) => Math.min(FONT_PT_MAX, Math.max(FONT_PT_MIN, pt));
const fontSizePx = () => (settings.terminalFontSize || 11) * (96 / 72);

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

// Centered titlebar text: app name, plus the active session when there is one.
function updateTitle() {
  const s = sessions.get(activeId);
  titleEl.textContent = s ? `Mandor — ${s.name}` : "Mandor";
}

// --- theming: data-driven registry (chrome vars + full 16-color terminal
// palette). Built-in themes carry hand-tuned chrome; terminal-style themes
// (Ghostty, pasted custom) omit `chrome` and have it derived from the palette. ---
function hexToRgb(h) {
  h = h.replace("#", "");
  if (h.length === 3)
    h = h
      .split("")
      .map((c) => c + c)
      .join("");
  const n = parseInt(h, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
function rgbToHex(rgb) {
  return (
    "#" +
    rgb
      .map((v) =>
        Math.max(0, Math.min(255, Math.round(v)))
          .toString(16)
          .padStart(2, "0"),
      )
      .join("")
  );
}
function mixHex(a, b, t) {
  const A = hexToRgb(a);
  const B = hexToRgb(b);
  return rgbToHex(A.map((v, i) => v + (B[i] - v) * t));
}
function luminance(hex) {
  const [r, g, b] = hexToRgb(hex).map((v) => {
    v /= 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
const isDarkColor = (hex) => luminance(hex) < 0.5;

const BUILTIN_THEMES = {
  latte: {
    label: "Latte (light)",
    chrome: {
      bg: "#eff1f5",
      bgAlt: "#e6e9ef",
      border: "#ccd0da",
      fg: "#4c4f69",
      fgDim: "#6c6f85",
      accent: "#1e66f5",
      accentBg: "#ccd0da",
      red: "#d20f39",
      titlebar: "#dce0e8",
    },
    term: {
      background: "#eff1f5",
      foreground: "#4c4f69",
      cursor: "#dc8a78",
      cursorAccent: "#eff1f5",
      selectionBackground: "#acb0be",
      ansi: [
        "#bcc0cc",
        "#d20f39",
        "#40a02b",
        "#df8e1d",
        "#1e66f5",
        "#ea76cb",
        "#179299",
        "#5c5f77",
        "#acb0be",
        "#e7103f",
        "#46b02f",
        "#e49931",
        "#3878f6",
        "#ef95d7",
        "#19a1a8",
        "#6c6f85",
      ],
    },
  },
  frappe: {
    label: "Frappé",
    chrome: {
      bg: "#303446",
      bgAlt: "#292c3c",
      border: "#414559",
      fg: "#c6d0f5",
      fgDim: "#a5adce",
      accent: "#8caaee",
      accentBg: "#414559",
      red: "#e78284",
      titlebar: "#232634",
    },
    term: {
      background: "#303446",
      foreground: "#c6d0f5",
      cursor: "#f2d5cf",
      cursorAccent: "#303446",
      selectionBackground: "#626880",
      ansi: [
        "#51576d",
        "#e78284",
        "#a6d189",
        "#e5c890",
        "#8caaee",
        "#f4b8e4",
        "#81c8be",
        "#b5bfe2",
        "#626880",
        "#eda0a2",
        "#b9dba2",
        "#ecd7ae",
        "#adc2f3",
        "#f38ed8",
        "#98d2ca",
        "#a5adce",
      ],
    },
  },
  macchiato: {
    label: "Macchiato",
    chrome: {
      bg: "#24273a",
      bgAlt: "#1e2030",
      border: "#363a4f",
      fg: "#cad3f5",
      fgDim: "#a5adcb",
      accent: "#8aadf4",
      accentBg: "#363a4f",
      red: "#ed8796",
      titlebar: "#181926",
    },
    term: {
      background: "#24273a",
      foreground: "#cad3f5",
      cursor: "#f4dbd6",
      cursorAccent: "#24273a",
      selectionBackground: "#5b6078",
      ansi: [
        "#494d64",
        "#ed8796",
        "#a6da95",
        "#eed49f",
        "#8aadf4",
        "#f5bde6",
        "#8bd5ca",
        "#b8c0e0",
        "#5b6078",
        "#f2a7b2",
        "#bde3b0",
        "#f4e3c1",
        "#adc5f7",
        "#f493da",
        "#a5ded6",
        "#a5adcb",
      ],
    },
  },
  mocha: {
    label: "Mocha",
    chrome: {
      bg: "#1e1e2e",
      bgAlt: "#181825",
      border: "#313244",
      fg: "#cdd6f4",
      fgDim: "#a6adc8",
      accent: "#89b4fa",
      accentBg: "#313244",
      red: "#f38ba8",
      titlebar: "#11111b",
    },
    term: {
      background: "#1e1e2e",
      foreground: "#cdd6f4",
      cursor: "#f5e0dc",
      cursorAccent: "#1e1e2e",
      selectionBackground: "#585b70",
      ansi: [
        "#45475a",
        "#f38ba8",
        "#a6e3a1",
        "#f9e2af",
        "#89b4fa",
        "#f5c2e7",
        "#94e2d5",
        "#bac2de",
        "#585b70",
        "#f7aec2",
        "#c2ecbf",
        "#fcd682",
        "#aeccfc",
        "#f398da",
        "#b1eae1",
        "#a6adc8",
      ],
    },
  },
  "ghostty-dark": {
    label: "Ghostty Default Dark",
    term: {
      background: "#282c34",
      foreground: "#ffffff",
      cursor: "#ffffff",
      cursorAccent: "#353a44",
      selectionBackground: "#ffffff",
      selectionForeground: "#282c34",
      ansi: [
        "#1d1f21",
        "#cc6566",
        "#b6bd68",
        "#f0c674",
        "#82a2be",
        "#b294bb",
        "#8abeb7",
        "#c4c8c6",
        "#666666",
        "#d54e53",
        "#b9ca4b",
        "#e7c547",
        "#7aa6da",
        "#c397d8",
        "#70c0b1",
        "#eaeaea",
      ],
    },
  },
};

function getTheme(key) {
  if (BUILTIN_THEMES[key]) return BUILTIN_THEMES[key];
  return (settings.customThemes || []).find((t) => t.id === key) || null;
}
// [key, theme] pairs for menus: built-ins first, then custom themes.
function themeEntries() {
  return [
    ...Object.entries(BUILTIN_THEMES),
    ...(settings.customThemes || []).map((t) => [t.id, t]),
  ];
}

// Derive chrome vars from a terminal palette (for themes without hand-tuned
// chrome): shades off the background, dim the foreground, borrow accent/red.
function deriveChrome(term) {
  const bg = term.background;
  const fg = term.foreground;
  const dark = isDarkColor(bg);
  const toward = dark ? "#000000" : "#ffffff";
  const away = dark ? "#ffffff" : "#000000";
  return {
    bg,
    bgAlt: mixHex(bg, toward, 0.35),
    titlebar: mixHex(bg, toward, 0.55),
    border: mixHex(bg, away, 0.14),
    accentBg: mixHex(bg, away, 0.14),
    fg,
    fgDim: mixHex(fg, bg, 0.4),
    accent: term.ansi[12] || term.ansi[4],
    red: term.ansi[9] || term.ansi[1],
  };
}
const chromeFor = (theme) => theme.chrome || deriveChrome(theme.term);

const XTERM_ANSI_KEYS = [
  "black",
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "white",
  "brightBlack",
  "brightRed",
  "brightGreen",
  "brightYellow",
  "brightBlue",
  "brightMagenta",
  "brightCyan",
  "brightWhite",
];
// Build an xterm ITheme (bg/fg/cursor/selection + the 16 ANSI colors).
function xtermTheme(term) {
  const t = {
    background: term.background,
    foreground: term.foreground,
    cursor: term.cursor,
    cursorAccent: term.cursorAccent || term.background,
    selectionBackground: term.selectionBackground,
  };
  if (term.selectionForeground)
    t.selectionForeground = term.selectionForeground;
  term.ansi.forEach((c, i) => (t[XTERM_ANSI_KEYS[i]] = c));
  return t;
}
function activeXtermTheme() {
  return xtermTheme((getTheme(settings.theme) || BUILTIN_THEMES.frappe).term);
}

// Parse a Ghostty-format palette (also matches iTerm2-Color-Schemes' ghostty
// files): `palette = N=#hex`, `background/foreground/cursor-color/...`.
// Returns { term } or null if the required colors (16 palette + bg + fg) are
// missing.
function parseGhosttyTheme(text) {
  const ansi = new Array(16).fill(null);
  const hex6 = /^#?[0-9a-fA-F]{6}$/;
  const norm = (v) => (v.startsWith("#") ? v : "#" + v).toLowerCase();
  let background, foreground, cursor, cursorAccent, selBg, selFg;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    const eq = line.indexOf("=");
    if (!line || eq === -1) continue;
    const key = line.slice(0, eq).trim().toLowerCase();
    const val = line.slice(eq + 1).trim();
    if (key === "palette") {
      const m = val.match(/^(\d{1,2})\s*=\s*(#?[0-9a-fA-F]{6})$/);
      if (m && +m[1] >= 0 && +m[1] < 16) ansi[+m[1]] = norm(m[2]);
    } else if (key === "background" && hex6.test(val)) background = norm(val);
    else if (key === "foreground" && hex6.test(val)) foreground = norm(val);
    else if (key === "cursor-color" && hex6.test(val)) cursor = norm(val);
    else if (key === "cursor-text" && hex6.test(val)) cursorAccent = norm(val);
    else if (key === "selection-background" && hex6.test(val))
      selBg = norm(val);
    else if (key === "selection-foreground" && hex6.test(val))
      selFg = norm(val);
  }
  if (ansi.some((c) => !c) || !background || !foreground) return null;
  return {
    term: {
      background,
      foreground,
      cursor: cursor || foreground,
      cursorAccent: cursorAccent || background,
      selectionBackground: selBg || ansi[8],
      selectionForeground: selFg,
      ansi,
    },
  };
}

// --- persistence (localStorage; PTY liveness is the backend's source of truth) ---
// Kept as-is across the mandor-term→Mandor rename so migrated localStorage
// (sessions, groups, settings) is still found under this key.
const STORE_KEY = "mandor-term.sidebar";

function persist() {
  if (POPOUT) return; // the main window owns persistence
  const sess = sessionOrder
    .map((id) => sessions.get(id))
    .filter(Boolean)
    .filter((s) => !s.incognito) // never persist incognito sessions to disk
    .map((s) => ({
      id: s.id,
      cwd: s.cwd,
      name: s.name,
      groupId: s.groupId ?? null,
      wantRemote: !!s.wantRemote,
      pr: s.pr || null,
    }));
  try {
    localStorage.setItem(
      STORE_KEY,
      JSON.stringify({
        groups,
        ungroupedCollapsed,
        recentDirs,
        sidebarCollapsed,
        settings,
        sessions: sess,
        activeId,
      }),
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
    recentDirs = Array.isArray(data.recentDirs) ? data.recentDirs : [];
    sidebarCollapsed = !!data.sidebarCollapsed;
    const stored =
      data.settings && typeof data.settings === "object" ? data.settings : null;
    if (stored) settings = { ...settings, ...data.settings };
    // Font size is now stored in points. Migrate stores from before that (their
    // value was px) once, so the on-screen size stays about the same (pt ≈ px×¾).
    if (stored && !stored.fontSizeUnitPt) {
      settings.terminalFontSize = clampFontPt(
        Math.round((settings.terminalFontSize || 15) * 0.75),
      );
    }
    settings.fontSizeUnitPt = true;
    if (!Array.isArray(settings.customThemes)) settings.customThemes = [];
    // fall back to the default flavor if the saved theme no longer exists
    // (covers old auto/light/dark values and deleted custom themes)
    if (!getTheme(settings.theme)) settings.theme = "frappe";
    savedSessions = Array.isArray(data.sessions) ? data.sessions : [];
    savedActiveId = data.activeId ?? null;
  } catch (e) {
    console.error(e);
  }
}

function addRecentDir(dir) {
  const i = recentDirs.indexOf(dir);
  if (i !== -1) recentDirs.splice(i, 1);
  recentDirs.unshift(dir);
  if (recentDirs.length > 12) recentDirs.length = 12;
  persist();
}

function removeRecentDir(dir) {
  const i = recentDirs.indexOf(dir);
  if (i === -1) return;
  recentDirs.splice(i, 1);
  persist();
  buildCwdMenu(); // rebuild the open dropdown
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
    if (justDragged || name.isContentEditable) return;
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
  header.addEventListener("mousedown", (e) =>
    dragStart(e, "group", g.id, header),
  );
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
  header.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e, "ungrouped", null);
  });
  return header;
}

function buildRow(s) {
  const row = document.createElement("div");
  row.className = "session-row";
  if (s.id === activeId) row.classList.add("active");
  if (s.exited) row.classList.add("exited");
  else if (!s.live) row.classList.add("cold"); // restored, not yet resumed
  const bg = s.id !== activeId;
  if (bg && s.attention) row.classList.add("has-attention");
  else if (bg && s.working) row.classList.add("working");
  row.dataset.id = s.id;

  const badge = document.createElement("span");
  badge.className = "s-badge"; // activity/attention dot (working / has-attention)
  const name = document.createElement("span");
  name.className = "s-name";
  name.textContent = s.name + (s.exited ? " (exited)" : "");
  const cwdLine = !s.live && !s.exited ? `${s.cwd} — click to resume` : s.cwd;
  name.title = `${cwdLine}\nsession: ${s.id}`;
  const close = document.createElement("span");
  close.className = "s-close";
  close.textContent = "✕";
  close.title = "Close session";
  row.append(badge);
  if (s.incognito) {
    const inc = document.createElement("span");
    inc.className = "s-incognito";
    inc.textContent = "🕶";
    inc.title = "Incognito — isolated config dir, nothing saved to disk";
    row.append(inc);
  }
  row.append(name);
  if (s.pr && s.pr.prUrl) {
    const pr = document.createElement("span");
    pr.className = "s-pr";
    pr.textContent = `#${s.pr.prNumber}`;
    pr.title = `Pull request ${s.pr.prUrl} — click to open`;
    pr.addEventListener("click", (e) => {
      e.stopPropagation();
      invoke("open_url", { url: s.pr.prUrl }).catch(() => {});
    });
    row.append(pr);
  }
  row.append(close);

  row.addEventListener("click", (e) => {
    if (justDragged || e.target === close || name.isContentEditable) return;
    setActive(s.id);
  });
  close.addEventListener("click", (e) => {
    e.stopPropagation();
    requestClose(s.id);
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
  row.addEventListener("mousedown", (e) => dragStart(e, "session", s.id, row));
  return row;
}

// --- drag-to-reorder (pointer-based; Tauri disables HTML5 drag events) ---
let drag = null; // { kind, id, startY, el, moved }
let justDragged = false; // set on drop so the ensuing click doesn't select/toggle

function dragStart(e, kind, id, el) {
  if (
    e.button !== 0 ||
    e.target.closest(".s-close") ||
    e.target.isContentEditable
  )
    return;
  justDragged = false;
  drag = { kind, id, startY: e.clientY, el, moved: false };
}

function clearDropMarks() {
  for (const el of listEl.querySelectorAll(
    ".drop-before, .drop-after, .drop-into",
  ))
    el.classList.remove("drop-before", "drop-after", "drop-into");
}

// The group header element for a group id (null = the Ungrouped header).
function groupHeaderEl(groupId) {
  if (groupId)
    return listEl.querySelector(`.group-header[data-group="${groupId}"]`);
  return [...listEl.querySelectorAll(".group-header")].find(
    (h) => !h.dataset.group,
  );
}

// The sidebar item under a y coordinate, and whether to drop before or after it.
function dropTarget(y) {
  const items = [...listEl.querySelectorAll(".session-row, .group-header")];
  if (!items.length) return null;
  for (const el of items) {
    const r = el.getBoundingClientRect();
    // First item the cursor is within or above; its half decides before/after.
    // (Using the bottom edge lets the lower half of a group's last row read as
    // "after" — the drop point for the end of a group.)
    if (y <= r.bottom) return { el, before: y < r.top + r.height / 2 };
  }
  return { el: items[items.length - 1], before: false };
}

document.addEventListener("mousemove", (e) => {
  if (!drag) return;
  if (!drag.moved) {
    if (Math.abs(e.clientY - drag.startY) < 4) return;
    drag.moved = true;
    document.body.classList.add("dragging");
    drag.el.classList.add("drag-src");
  }
  clearDropMarks();
  const t = dropTarget(e.clientY);
  if (!t || t.el === drag.el) return;

  const onHeader = t.el.classList.contains("group-header");
  if (drag.kind === "session" && onHeader) {
    // Over a COLLAPSED group: expand it so its rows show and the session can be
    // dropped at an exact position (next mousemove computes it among the rows).
    const gid = t.el.dataset.group || null;
    const collapsedGroup =
      gid && groups.find((x) => x.id === gid && x.collapsed);
    if (collapsedGroup || (!gid && ungroupedCollapsed)) {
      if (collapsedGroup) collapsedGroup.collapsed = false;
      else ungroupedCollapsed = false;
      persist();
      renderSessionList();
      const again = listEl.querySelector(`.session-row[data-id="${drag.id}"]`);
      if (again) {
        drag.el = again; // re-acquire the dragged row after the re-render
        again.classList.add("drag-src");
      }
      return;
    }
    // Dropping on a header means "into this group" — ring only, no line
    // (avoids a doubled top line where the ring meets the drop line).
    groupHeaderEl(targetGroupId(t))?.classList.add("drop-into");
    return;
  }
  t.el.classList.add(t.before ? "drop-before" : "drop-after");
  // A session landing among another group's rows: also highlight that group.
  if (drag.kind === "session") {
    const s = sessions.get(drag.id);
    const tg = targetGroupId(t);
    if (s && (s.groupId ?? null) !== tg)
      groupHeaderEl(tg)?.classList.add("drop-into");
  }
});

document.addEventListener("mouseup", (e) => {
  if (!drag) return;
  const d = drag;
  drag = null;
  document.body.classList.remove("dragging");
  d.el.classList.remove("drag-src");
  clearDropMarks();
  if (!d.moved) return; // a plain click; let the click handler run
  justDragged = true;
  const t = dropTarget(e.clientY);
  if (t) {
    if (d.kind === "session") dropSession(d.id, t);
    else dropGroup(d.id, t);
  }
});

// WebKitGTK ignores `user-select: none` alone here — also block selectstart in
// the sidebar (except while renaming, where the target may be a text node).
document.getElementById("sidebar").addEventListener("selectstart", (e) => {
  const node = e.target;
  const el = node.nodeType === 1 ? node : node.parentElement;
  if (el?.closest('[contenteditable="true"]')) return;
  e.preventDefault();
});

// Same for the settings dialog (WebKitGTK ignores user-select alone); inputs and
// textareas stay selectable so their text can still be edited.
document
  .getElementById("settings-modal")
  .addEventListener("selectstart", (e) => {
    const node = e.target;
    const el = node.nodeType === 1 ? node : node.parentElement;
    if (el?.closest('input, textarea, [contenteditable="true"]')) return;
    e.preventDefault();
  });

// The group a drop target belongs to ("" header → null / Ungrouped).
function targetGroupId(t) {
  if (t.el.classList.contains("group-header"))
    return t.el.dataset.group || null;
  const ts = sessions.get(t.el.dataset.id);
  return ts ? (ts.groupId ?? null) : null;
}

function dropSession(id, t) {
  const s = sessions.get(id);
  if (!s) return;
  if (!t.el.classList.contains("group-header") && t.el.dataset.id === id)
    return;
  s.groupId = targetGroupId(t);
  const oi = sessionOrder.indexOf(id);
  if (oi !== -1) sessionOrder.splice(oi, 1);

  if (t.el.classList.contains("group-header")) {
    // top of that group (or end if empty)
    const first = sessionOrder.findIndex(
      (x) => (sessions.get(x)?.groupId ?? null) === s.groupId,
    );
    if (first === -1) sessionOrder.push(id);
    else sessionOrder.splice(first, 0, id);
  } else {
    let ai = sessionOrder.indexOf(t.el.dataset.id);
    if (ai === -1) ai = sessionOrder.length - 1;
    else if (!t.before) ai += 1;
    sessionOrder.splice(ai, 0, id);
  }
  persist();
  renderSessionList();
}

function dropGroup(id, t) {
  const from = groups.findIndex((g) => g.id === id);
  if (from === -1) return;
  const targetGid = targetGroupId(t);
  const [g] = groups.splice(from, 1);
  let to = groups.findIndex((x) => x.id === targetGid);
  if (to === -1)
    groups.push(g); // dropped in the Ungrouped area → last
  else {
    if (!t.before) to += 1;
    groups.splice(to, 0, g);
  }
  persist();
  renderSessionList();
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
    if (s.id === activeId) updateTitle();
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

function copyText(text) {
  navigator.clipboard?.writeText(text).catch(() => {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.cssText = "position:fixed;opacity:0";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } catch {
      /* ignore */
    }
    ta.remove();
  });
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
    items.push({ label: "Open in new window", run: () => popOutSession(id) });
    if (s.live) {
      // Resume can't pass --remote-control (the CLI would fail reconnecting to a
      // dead registration), so enable it in-session — a fresh registration.
      items.push({
        label: "Enable remote control",
        run: () =>
          invoke("write_pty", { id, data: "/remote-control\r" }).catch(
            () => {},
          ),
      });
    }
    if (s.pr && s.pr.prUrl) {
      items.push({
        label: `Open PR #${s.pr.prNumber}`,
        run: () => invoke("open_url", { url: s.pr.prUrl }).catch(() => {}),
      });
    }
    items.push({
      label: "Copy session ID",
      run: () => copyText(s.id),
    });
    items.push({ sep: true });
    const moveTo = [
      {
        label: "Ungrouped",
        current: s.groupId == null,
        run: () => moveSession(id, null),
      },
      ...groups.map((g) => ({
        label: g.name,
        current: g.id === s.groupId,
        run: () => moveSession(id, g.id),
      })),
      { sep: true },
      {
        label: "New group…",
        run: () => {
          const g = createGroup();
          moveSession(id, g.id);
        },
      },
    ];
    items.push({ label: "Move to group", submenu: moveTo });
    items.push({ sep: true });
    items.push({
      label: "Close session",
      danger: true,
      run: () => requestClose(id),
    });
  } else if (kind === "group") {
    const g = groups.find((x) => x.id === id);
    if (!g) return;
    items.push({ label: "New session here", run: () => openNewSession(id) });
    items.push({ sep: true });
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
  } else if (kind === "ungrouped") {
    items.push({ label: "New session here", run: () => openNewSession(null) });
    items.push({ sep: true });
    items.push({ label: "New group", run: createGroupAndRename });
  } else {
    items.push({ label: "New session", run: () => openNewSession(null) });
    items.push({ label: "New group", run: createGroupAndRename });
  }

  const mkButton = (item, container) => {
    if (item.sep) {
      const sep = document.createElement("div");
      sep.className = "context-sep";
      container.appendChild(sep);
      return;
    }
    const btn = document.createElement("button");
    btn.textContent = item.label + (item.current ? "  ✓" : "");
    if (item.danger) btn.className = "danger";
    btn.addEventListener("click", () => {
      closeContextMenu();
      item.run();
    });
    container.appendChild(btn);
  };

  const menu = document.createElement("div");
  menu.className = "context-menu";
  for (const item of items) {
    if (item.submenu) {
      const parent = document.createElement("div");
      parent.className = "context-parent";
      const btn = document.createElement("button");
      btn.className = "context-parent-btn";
      const lbl = document.createElement("span");
      lbl.textContent = item.label;
      const chev = document.createElement("span");
      chev.className = "context-chevron";
      chev.textContent = "▸";
      btn.append(lbl, chev);
      const sub = document.createElement("div");
      sub.className = "context-submenu";
      for (const si of item.submenu) mkButton(si, sub);
      parent.append(btn, sub);
      menu.appendChild(parent);
      continue;
    }
    mkButton(item, menu);
  }

  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  const x = Math.min(e.clientX, window.innerWidth - rect.width - 6);
  const y = Math.min(e.clientY, window.innerHeight - rect.height - 6);
  menu.style.left = `${Math.max(6, x)}px`;
  menu.style.top = `${Math.max(6, y)}px`;
  // Flip submenus leftward if the menu is near the right edge.
  if (x + rect.width + 180 > window.innerWidth) {
    for (const sub of menu.querySelectorAll(".context-submenu"))
      sub.classList.add("submenu-left");
  }
  contextMenuEl = menu;
}

listEl.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  openContextMenu(e, "sidebar", null);
});
window.addEventListener("blur", closeContextMenu);

// --- session lifecycle ---

// Register a session record (no terminal, no PTY yet).
function addSession(rec) {
  const s = {
    id: rec.id,
    cwd: rec.cwd,
    name: rec.name || baseName(rec.cwd),
    groupId: rec.groupId ?? null,
    spawnMode: rec.spawnMode || "resume", // how to spawn when materialized
    wantWorktree: !!rec.wantWorktree,
    wantModel: rec.wantModel || "",
    wantRemote: !!rec.wantRemote,
    incognito: !!rec.incognito, // isolated config dir, nothing saved to disk
    pr: rec.pr || null, // { prNumber, prUrl } if claude opened a PR in it
    live: !!rec.live, // a PTY is running for this session
    exited: false,
    term: null,
    fit: null,
    el: null,
  };
  sessions.set(s.id, s);
  if (!sessionOrder.includes(s.id)) sessionOrder.push(s.id);
  return s;
}

// Give a session an xterm + DOM node (kept detached until activated).
// Terminal font family stack: the chosen font first, then the bundled JetBrains
// Mono and generic monospace as fallbacks.
function terminalFontStack() {
  const f = (settings.terminalFont || "JetBrains Mono").trim();
  const base = '"JetBrains Mono", ui-monospace, monospace';
  return f && f !== "JetBrains Mono" ? `"${f}", ${base}` : base;
}

// Common programming ligatures (longest-first so e.g. "===" wins over "=="). The
// joiner tells xterm to render each match as one run; the CSS enables the font's
// contextual alternates so the ligature glyph actually forms (DOM renderer).
const LIGATURE_RE =
  /<==>|<-->|-->|<--|<->|===|!==|<=>|\.\.\.|->|<-|=>|==|!=|>=|<=|&&|\|\||\|>|<\||\+\+|--|::|:=|\/\/|\/\*|\*\/|\*\*|<<|>>|\.\.|~>|<~|\?\?/g;
function ligatureJoiner(text) {
  const ranges = [];
  LIGATURE_RE.lastIndex = 0;
  let m;
  while ((m = LIGATURE_RE.exec(text)) !== null) {
    ranges.push([m.index, m.index + m[0].length]);
  }
  return ranges;
}

// While selecting, auto-scroll when the pointer nears the top/bottom edge. xterm
// only scrolls when the pointer goes strictly PAST the rows, which in a full-
// height terminal is a sliver at the window edge — so extend it to a small edge
// zone (the region xterm ignores, so the two don't double up), re-driving xterm's
// selection at the clamped edge so it keeps growing as the buffer scrolls.
const SELECTION_EDGE = 28;
function setupSelectionAutoscroll(s, el) {
  el.addEventListener("mousedown", (e) => {
    if (e.button !== 0 || !s.term || s.term.buffer.active.type === "alternate")
      return;
    let last = e;
    let timer = null;
    const onMove = (ev) => (last = ev);
    const onUp = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      if (timer) clearInterval(timer);
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    timer = setInterval(() => {
      if (!s.term) return;
      const screen =
        el.querySelector(".xterm-screen") || el.querySelector(".xterm");
      if (!screen) return;
      const r = screen.getBoundingClientRect();
      const y = last.clientY;
      let dir = 0;
      if (y >= r.bottom - SELECTION_EDGE && y <= r.bottom) dir = 1;
      else if (y <= r.top + SELECTION_EDGE && y >= r.top) dir = -1;
      if (!dir) return; // outside the edge zone (or beyond it — xterm's own job)
      s.term.scrollLines(dir);
      const clampedY = dir === 1 ? r.bottom - 1 : r.top + 1;
      screen.dispatchEvent(
        new MouseEvent("mousemove", {
          bubbles: true,
          clientX: last.clientX,
          clientY: clampedY,
        }),
      );
    }, 50);
  });
}

function attachTerminal(s) {
  if (s.term) return;
  const el = document.createElement("div");
  el.className = "terminal-host";
  if (settings.ligatures) el.classList.add("ligatures");
  el.style.display = "none";
  termsEl.appendChild(el);
  const term = new Terminal({
    fontFamily: terminalFontStack(),
    fontSize: fontSizePx(),
    cursorBlink: true,
    scrollback: 5000,
    scrollSensitivity: 3,
    fastScrollSensitivity: 6,
    allowProposedApi: true,
    theme: activeXtermTheme(),
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  const search = new SearchAddon.SearchAddon();
  term.loadAddon(search);
  term.open(el);
  // Let app shortcuts reach the document handler instead of the PTY:
  // Ctrl/Cmd + , = - _  and  Ctrl/Cmd+Shift+F (find in session).
  term.attachCustomKeyEventHandler((ev) => {
    // Shift+Enter → newline. Terminals send the same bytes for Enter and
    // Shift+Enter, and without an extended keyboard protocol (xterm.js has none)
    // claude can't distinguish key events — so inject a bare LF (0x0a), the byte
    // Ctrl+J sends, which claude treats as "insert newline". Crucially, Enter
    // fires BOTH keydown and keypress; we must swallow both, or xterm still emits
    // its default CR (submit) on the keypress. Inject once, on keydown.
    if (
      (ev.key === "Enter" ||
        ev.code === "Enter" ||
        ev.code === "NumpadEnter") &&
      ev.shiftKey &&
      !ev.ctrlKey &&
      !ev.metaKey &&
      !ev.altKey
    ) {
      if (ev.type === "keydown")
        invoke("write_pty", { id: s.id, data: "\n" }).catch(() => {});
      if (ev.type === "keydown" || ev.type === "keypress") return false;
      return true;
    }
    if (ev.type !== "keydown") return true;
    if (
      (ev.ctrlKey || ev.metaKey) &&
      ev.shiftKey &&
      (ev.key === "F" || ev.key === "f")
    )
      return false;
    if (
      (ev.ctrlKey || ev.metaKey) &&
      !ev.shiftKey &&
      [",", "=", "+", "-", "_"].includes(ev.key)
    )
      return false;
    return true;
  });
  search.onDidChangeResults((r) => {
    if (s.id !== activeId) return;
    if (!r || r.resultCount === 0) {
      findCount.textContent = findInput.value ? "no results" : "";
    } else {
      findCount.textContent =
        r.resultIndex >= 0
          ? `${r.resultIndex + 1}/${r.resultCount}`
          : `${r.resultCount} found`;
    }
  });
  s.term = term;
  s.fit = fit;
  s.search = search;
  s.el = el;
  s.ligatureJoinerId = settings.ligatures
    ? term.registerCharacterJoiner(ligatureJoiner)
    : null;
  setupSelectionAutoscroll(s, el);
  term.onData((data) =>
    invoke("write_pty", { id: s.id, data }).catch(() => {}),
  );
  term.onResize(({ cols, rows }) =>
    invoke("resize_pty", { id: s.id, cols, rows }).catch(() => {}),
  );
  // (optional-chained: guard against older xterm builds without onBell)
  term.onBell?.(() => onBell(s));
}

// Sidebar activity indicator. We don't parse the stream, so state is inferred
// from PTY output: while claude streams output the session is "working" (blue
// pulse); when output stops (or the bell rings) a background session flips to
// "needs attention" (amber) — meaning it finished a turn / is waiting for you.
// Titlebar bell: shown whenever any background session needs attention (visible
// even with the sidebar collapsed). Clicking opens a dropdown of those sessions;
// picking one opens it (which clears its attention, dropping it off the list).
const attentionDropdown = document.getElementById("attention-dropdown");
const attentionBtn = document.getElementById("attention-btn");
const attentionMenu = document.getElementById("attention-menu");

function buildAttentionMenu() {
  attentionMenu.replaceChildren();
  for (const id of sessionOrder) {
    const s = sessions.get(id);
    if (!s || !s.attention) continue;
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = s.name;
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      setActive(id); // clears attention; updateAttentionIndicator refreshes below
    });
    attentionMenu.appendChild(b);
  }
}

function updateAttentionIndicator() {
  if (POPOUT) return;
  const any = [...sessions.values()].some((x) => x.attention);
  attentionDropdown.hidden = !any;
  if (!any) attentionMenu.hidden = true;
  else if (!attentionMenu.hidden) buildAttentionMenu(); // keep an open list fresh
}

if (!POPOUT) {
  attentionBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    const willOpen = attentionMenu.hidden;
    attentionMenu.hidden = true;
    if (willOpen) {
      buildAttentionMenu();
      attentionMenu.hidden = false;
    }
  });
}

function refreshBadge(s) {
  updateAttentionIndicator();
  const row = listEl.querySelector(`.session-row[data-id="${s.id}"]`);
  if (!row) return;
  const bg = s.id !== activeId;
  row.classList.toggle("has-attention", bg && !!s.attention);
  row.classList.toggle("working", bg && !!s.working && !s.attention);
}

// Treat output→idle as "finished a turn" only if the burst lasted at least this
// long AND carried at least this many bytes. The duration gate skips brief
// repaints (a resize redraw); the byte gate skips small *periodic* output that
// stays "working" without a real turn — notably remote-control bridge
// heartbeats, which were raising a false amber dot on idle sessions. A real
// response streams kilobytes; heartbeat/status ticks are tens of bytes.
const WORK_ATTENTION_MS = 1500;
const WORK_ATTENTION_BYTES = 1024;
// Quiet time after the last output before a turn is considered finished. Longer
// than a typical mid-turn pause (tool run / thinking), so we alert once at the
// end instead of on every pause.
const NOTIFY_QUIET_MS = 2500;

// Look up the session's PR from its transcript (claude records one when it runs
// a PR git op). Refreshed when a turn ends, so a just-opened PR shows up soon.
function refreshSessionPr(s) {
  if (!s || s.incognito) return; // incognito transcripts aren't in the shared store
  invoke("session_pr", { id: s.id, cwd: s.cwd })
    .then((pr) => {
      if (JSON.stringify(pr ?? null) === JSON.stringify(s.pr ?? null)) return;
      s.pr = pr || null;
      renderSessionList();
      persist();
    })
    .catch(() => {});
}

function markWorking(s, len = 0) {
  const now = Date.now();
  if (!s.working) {
    s.working = true;
    s.attention = false; // it's active again — clear any prior dot
    refreshBadge(s);
  }
  // Accumulate over the whole turn so mid-turn pauses (tools/thinking) count as
  // one episode. turnStart/turnBytes reset only when a turn finalizes.
  if (!s.turnStart) {
    s.turnStart = now;
    s.turnBytes = 0;
  }
  s.turnBytes += len;

  // Responsive "working" indicator: drops shortly after output stops.
  clearTimeout(s.workTimer);
  s.workTimer = setTimeout(() => {
    s.working = false;
    refreshBadge(s);
  }, 900);

  // Debounced alert: fires only after the session has been quiet for
  // NOTIFY_QUIET_MS, so a turn that pauses for tools notifies once — at the end,
  // not on each pause. Any new output resets the timer.
  clearTimeout(s.turnTimer);
  s.turnTimer = setTimeout(() => {
    const worked = Date.now() - (s.turnStart || Date.now());
    const substantial =
      worked >= WORK_ATTENTION_MS && (s.turnBytes || 0) >= WORK_ATTENTION_BYTES;
    s.turnStart = null; // turn ended
    // A turn ending during reconnect/resume is replayed transcript, not real.
    const warmup = s.warmup;
    s.warmup = false;
    if (substantial) {
      if (s.id !== activeId) s.attention = true;
      // Alert once per episode; cleared when the session is viewed.
      if (!s.alerted) {
        s.alerted = true;
        if (!warmup) playBellSound();
        notifyAttention(s); // guarded by unfocused
      }
      refreshBadge(s);
    }
    refreshSessionPr(s); // a PR may have just been opened this turn
  }, NOTIFY_QUIET_MS);
}

// xterm has no audible bell — it only fires onBell. We play through Web Audio,
// not an <audio> element: WebKit blocks HTMLMediaElement.play() unless it has a
// recent user gesture, and the bell fires from a background PTY event with no
// gesture of its own — so <audio> stays silent while a resumed AudioContext
// plays fine. The sound is decoded once into an AudioBuffer (custom file or the
// bundled default); a short synth beep is the fallback if decoding is
// unavailable.
let bellAudioCtx = null;
let bellBuffer = null; // decoded AudioBuffer of the current bell sound

function ensureAudioCtx() {
  if (!bellAudioCtx) {
    const AC = window.AudioContext || window.webkitAudioContext;
    if (AC) bellAudioCtx = new AC();
  }
  return bellAudioCtx;
}

// WebKit keeps the AudioContext "suspended" until a user gesture. Resume it on
// the first interaction so later event-driven bells can play without one.
function unlockAudio() {
  const ctx = ensureAudioCtx();
  if (ctx && ctx.state === "suspended") ctx.resume().catch(() => {});
}
for (const ev of ["pointerdown", "keydown"])
  window.addEventListener(ev, unlockAudio, { capture: true, passive: true });

function b64ToArrayBuffer(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes.buffer;
}

// Decode the bell sound (a custom file via the backend, else the bundled
// default) into an AudioBuffer. Called on load and whenever the setting changes.
async function loadBellSound() {
  bellBuffer = null;
  const ctx = ensureAudioCtx();
  if (!ctx) return;
  try {
    let arr;
    const path = settings.bellSoundFile;
    if (path) {
      arr = b64ToArrayBuffer(await invoke("read_audio_data", { path }));
    } else {
      arr = await (await fetch("notification.mp3")).arrayBuffer();
    }
    bellBuffer = await ctx.decodeAudioData(arr);
  } catch {
    bellBuffer = null; // undecodable → synth beep fallback
  }
}

function synthBeep() {
  try {
    const ctx = ensureAudioCtx();
    if (!ctx) return;
    if (ctx.state === "suspended") ctx.resume();
    const t = ctx.currentTime;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = "sine";
    osc.frequency.value = 880;
    gain.gain.setValueAtTime(0.0001, t);
    gain.gain.exponentialRampToValueAtTime(0.15, t + 0.01);
    gain.gain.exponentialRampToValueAtTime(0.0001, t + 0.22);
    osc.connect(gain).connect(ctx.destination);
    osc.start(t);
    osc.stop(t + 0.24);
  } catch {
    /* audio unavailable — ignore */
  }
}

function playBellSound() {
  if (!settings.bellSound) return;
  const ctx = ensureAudioCtx();
  if (!ctx) return;
  if (ctx.state === "suspended") ctx.resume().catch(() => {});
  if (bellBuffer) {
    try {
      const src = ctx.createBufferSource();
      src.buffer = bellBuffer;
      src.connect(ctx.destination);
      src.start();
      return;
    } catch {
      /* fall through to synth beep */
    }
  }
  synthBeep();
}

// Terminal bell — claude rings it when a turn finishes / it needs input.
function onBell(s) {
  clearTimeout(s.workTimer);
  clearTimeout(s.turnTimer); // claude's bell is the precise end — cancel the debounce
  s.working = false;
  s.turnStart = null;
  const warmup = s.warmup;
  s.warmup = false;
  if (s.id !== activeId) s.attention = true;
  if (!warmup && !s.alerted) {
    s.alerted = true;
    playBellSound(); // swallow a bell rung during reconnect/resume replay, and
    notifyAttention(s); // only alert once per episode
  }
  refreshBadge(s);
  refreshSessionPr(s);
}

// The webview's own focus is more reliable than Tauri's window focus event on
// GNOME/Wayland + WebKitGTK (where the blur event may never fire, leaving
// `appFocused` stuck true and silently suppressing every notification).
function appInForeground() {
  return document.hasFocus() && document.visibilityState !== "hidden";
}

function notifyAttention(s) {
  if (notificationsEnabled && !appInForeground()) {
    invoke("notify", {
      title: s.name,
      body: "Claude finished / needs input",
    }).catch(() => {});
  }
}

// Spawn the PTY for a materialized session. `new` forces a fresh session id
// (so it's resumable later); otherwise resume the existing conversation.
async function spawnSession(s) {
  if (s.live) return;
  s.live = true;
  const args = {
    id: s.id,
    cwd: s.cwd,
    cols: s.term.cols,
    rows: s.term.rows,
    resume: null,
    sessionId: null,
    name: null,
    worktree: false,
    model: null,
    remoteControl: false,
    incognito: !!s.incognito,
  };
  if (s.spawnMode === "new") {
    args.sessionId = s.id;
    args.name = s.name;
    args.worktree = s.wantWorktree;
    args.model = s.wantModel || null;
    args.remoteControl = !!s.wantRemote; // only a fresh session can register remote control
    s.warmup = false; // a fresh session's first turn is real — ring for it
  } else {
    args.resume = s.id;
    s.warmup = true; // resuming replays the transcript; don't ring for that burst
    // NB: --remote-control + --resume makes the CLI try to reattach to a dead
    // registration and fail ("Couldn't reconnect… start a fresh session without
    // --resume"); enable it in-session via /remote-control instead.
  }
  s.spawnMode = "resume"; // any later respawn resumes
  try {
    await invoke("open_pty", args);
  } catch (e) {
    s.term.writeln(`\r\n\x1b[31mFailed to start claude: ${e}\x1b[0m`);
    s.exited = true;
    s.live = false;
  }
  renderSessionList();
}

function setActive(id) {
  const s = sessions.get(id);
  activeId = id;
  if (s) {
    s.attention = false; // viewing it clears any attention flag
    s.alerted = false; // and re-arms the once-per-episode sound/notification
  }
  // Materialize a cold session (restored across restart) on first activation.
  if (s && !s.term && !s.exited) attachTerminal(s);
  for (const [sid, x] of sessions) {
    if (x.el) x.el.style.display = sid === id ? "block" : "none";
  }
  for (const row of listEl.querySelectorAll(".session-row")) {
    row.classList.toggle("active", row.dataset.id === id);
  }
  for (const x of sessions.values()) refreshBadge(x); // badges are relative to active
  emptyEl.style.display = sessions.size ? "none" : "flex";
  updateTitle();
  if (s && s.term) {
    s.fit.fit(); // size before spawn so the PTY opens at the right dimensions
    s.term.focus();
  }
  if (s && s.needsRepaint && s.live) {
    s.needsRepaint = false;
    nudgeRepaint(s);
  }
  if (s && !s.live && !s.exited && s.term) spawnSession(s);
  persist(); // remember the active session so it reopens on restart
}

// After reconnecting to a live PTY (webview reload) the fresh xterm is empty and
// claude only redraws on an actual size change — but the window size is
// unchanged across a reload, so a plain resize is a no-op. Briefly jiggle the
// PTY size (rows-1 → rows, spaced out) to force claude to reflow and repaint.
// Deferred so the output listener and layout are ready to catch the redraw.
function nudgeRepaint(s) {
  setTimeout(() => {
    if (!s.live || !s.term) return;
    const { cols, rows } = s.term;
    if (rows < 2) return;
    invoke("resize_pty", { id: s.id, cols, rows: rows - 1 })
      .then(() =>
        new Promise((r) => setTimeout(r, 40)).then(() =>
          invoke("resize_pty", { id: s.id, cols, rows }),
        ),
      )
      .catch(() => {});
  }, 120);
}

// New session (modal) or resume (picker/restore). opts:
// { resume?, name?, groupId?, worktree? }. On resume, opts.resume is the claude
// session id and becomes this session's id.
function startSession(cwd, opts = {}) {
  const isResume = !!opts.resume;
  const id = isResume ? opts.resume : crypto.randomUUID();
  if (sessions.has(id)) {
    const existing = sessions.get(id);
    if (opts.name && existing.name !== opts.name) {
      existing.name = opts.name; // adopt the -n name when resuming a restored pill
      renderSessionList();
      persist();
    }
    setActive(id); // already open — just focus it (also de-dupes resume)
    return;
  }
  addSession({
    id,
    cwd,
    name: opts.name,
    groupId: "groupId" in opts ? opts.groupId : autoGroupFor(cwd),
    spawnMode: isResume ? "resume" : "new",
    wantWorktree: opts.worktree,
    wantModel: opts.model,
    // resume has no checkbox — fall back to the "remote by default" setting
    wantRemote:
      "remoteControl" in opts ? opts.remoteControl : settings.remoteByDefault,
    incognito: !!opts.incognito,
    live: false,
  });
  addRecentDir(cwd); // remember the directory for the new-session picker
  renderSessionList();
  persist();
  setActive(id); // attaches, sizes, and spawns
}

// Open a session in its own window. The main window then switches away from it
// so it doesn't fight the pop-out over the shared PTY's size.
function popOutSession(id) {
  const s = sessions.get(id);
  if (!s) return;
  invoke("open_session_window", { id, name: s.name }).catch((e) =>
    console.error(e),
  );
  if (activeId === id) {
    const next = sessionOrder.find((sid) => sid !== id && sessions.has(sid));
    if (next) setActive(next);
  }
}

async function closeSession(id) {
  const s = sessions.get(id);
  if (!s) return;
  if (s.live) {
    try {
      await invoke("close_pty", { id });
    } catch (e) {
      console.error(e);
    }
  }
  if (s.term) s.term.dispose();
  if (s.el) s.el.remove();
  sessions.delete(id);
  const oi = sessionOrder.indexOf(id);
  if (oi !== -1) sessionOrder.splice(oi, 1);

  if (activeId === id) {
    activeId = null;
    const next = sessionOrder.find((sid) => sessions.has(sid)) ?? null;
    if (next) setActive(next);
    else {
      emptyEl.style.display = "flex";
      updateTitle();
    }
  }
  renderSessionList();
  updateAttentionIndicator();
  persist();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Opt-in: resume every restored (cold) session on startup, spawned one-by-one
// with a gap so N `claude` processes don't stampede at launch. Only the active
// session's terminal is visible; the rest stay detached until switched to
// (keeping the DOM light). Skips live/exited/incognito sessions.
async function resumeAllOnStart() {
  for (const id of [...sessionOrder]) {
    if (id === activeId) continue; // the active one is resumed by setActive
    const s = sessions.get(id);
    if (!s || s.live || s.exited || s.incognito) continue;
    if (!s.term) attachTerminal(s);
    await spawnSession(s);
    await sleep(400);
  }
}

// On launch: reconnect to PTYs that survived a webview reload, and restore
// persisted sessions from a full restart as cold entries (resumed on click).
async function restore() {
  loadStore();
  applySidebarCollapsed();
  applySettings();
  let running = [];
  try {
    running = await invoke("running_ptys");
  } catch (e) {
    console.error(e);
  }
  const liveIds = new Set(running.map((r) => r.id));

  for (const rec of savedSessions) {
    if (sessions.has(rec.id)) continue;
    addSession({ ...rec, spawnMode: "resume", live: liveIds.has(rec.id) });
  }
  // Live PTYs missing from storage: incognito ones (never persisted), or after a
  // storage wipe while the backend survived.
  for (const { id, cwd, incognito } of running) {
    if (sessions.has(id)) continue;
    addSession({
      id,
      cwd,
      name: baseName(cwd),
      groupId: autoGroupFor(cwd),
      spawnMode: "resume",
      incognito: !!incognito,
      live: true,
    });
  }
  // Attach terminals to live sessions now so their PTY output isn't dropped;
  // mark them for a repaint nudge on activation (claude lost the reloaded view).
  for (const id of sessionOrder) {
    const s = sessions.get(id);
    if (s.live && !s.term) {
      attachTerminal(s);
      s.needsRepaint = true;
      s.warmup = true; // the reconnect repaint burst isn't a real turn — no bell
    }
  }
  renderSessionList();
  persist();
  // Refresh each session's PR badge from its transcript (persisted value may be
  // stale, and cold-restored sessions have none yet).
  for (const s of sessions.values()) refreshSessionPr(s);
  // Reopen the session that was active last (whether live after a reload or cold
  // after a restart); otherwise prefer a live one, then the first in the list.
  const first =
    (savedActiveId && sessions.has(savedActiveId) ? savedActiveId : null) ??
    sessionOrder.find((id) => sessions.get(id)?.live) ??
    sessionOrder.find((id) => sessions.has(id));
  if (first) setActive(first);
  if (settings.resumeOnStart) resumeAllOnStart();
}

// Pop-out window: render just the one session, reconnected to its running PTY.
async function runPopout() {
  applySettings();
  let running = [];
  try {
    running = await invoke("running_ptys");
  } catch (e) {
    console.error(e);
  }
  const info = running.find((r) => r.id === POPOUT_ID);
  const s = addSession({
    id: POPOUT_ID,
    cwd: info ? info.cwd : "",
    name: info ? baseName(info.cwd) : "session",
    groupId: null,
    spawnMode: "resume",
    live: !!info,
  });
  attachTerminal(s);
  setActive(POPOUT_ID);
}

// --- PTY event routing ---
listen("pty-output", ({ payload }) => {
  const s = sessions.get(payload.id);
  if (!s || !s.term) return;
  const bytes = base64ToBytes(payload.b64);
  s.term.write(bytes);
  if (!POPOUT) markWorking(s, bytes.length); // drive the sidebar activity indicator
});

listen("pty-exit", ({ payload }) => {
  const s = sessions.get(payload.id);
  if (!s || s.exited) return;
  s.exited = true;
  s.live = false;
  renderSessionList();
  if (s.term) s.term.write("\r\n\x1b[90m[claude exited]\x1b[0m\r\n");
});

const resizeObserver = new ResizeObserver(() => {
  const s = sessions.get(activeId);
  if (s && s.fit) s.fit.fit();
});
resizeObserver.observe(termsEl);

const schemeQuery = window.matchMedia("(prefers-color-scheme: dark)");
schemeQuery.addEventListener("change", () => {
  const theme = activeXtermTheme();
  for (const s of sessions.values()) if (s.term) s.term.options.theme = theme;
});

// --- new-session modal ---
const newModal = document.getElementById("new-modal");
const nsName = document.getElementById("ns-name");
const nsWorktree = document.getElementById("ns-worktree");
const nsRemote = document.getElementById("ns-remote");
const nsIncognito = document.getElementById("ns-incognito");
// Incognito shouldn't broadcast to the Claude app by default — turning it on
// clears remote control; turning it off restores the configured default. Either
// way the user can still toggle remote control manually afterward.
nsIncognito.addEventListener("change", () => {
  nsRemote.checked = nsIncognito.checked ? false : !!settings.remoteByDefault;
});
const nsCwdLabel = document.getElementById("ns-cwd-label");
const nsCwdPath = document.getElementById("ns-cwd-path");
const nsCwdMenu = document.getElementById("ns-cwd-menu");
const nsGroupLabel = document.getElementById("ns-group-label");
const nsGroupMenu = document.getElementById("ns-group-menu");
const nsModelLabel = document.getElementById("ns-model-label");
const nsModelMenu = document.getElementById("ns-model-menu");
const nsExistingWrap = document.getElementById("ns-existing-wrap");
const nsExistingList = document.getElementById("ns-existing-list");
const nsExistingFilter = document.getElementById("ns-existing-filter");
let nsSessions = []; // all resumable sessions, filtered by the picked folder
let nsSessionsLoaded = false;
const MODEL_OPTIONS = [
  { value: "", label: "Default" },
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
];
let newSessionModel = "";
let newSessionCwd = "";
let newSessionGroup = ""; // "" = Ungrouped
let nsNameEdited = false;

function setCwdLabel() {
  if (newSessionCwd) {
    nsCwdLabel.textContent = baseName(newSessionCwd);
    nsCwdLabel.title = newSessionCwd;
    nsCwdLabel.classList.remove("placeholder");
    nsCwdPath.textContent = newSessionCwd;
  } else {
    nsCwdLabel.textContent = "Choose a folder…";
    nsCwdLabel.removeAttribute("title");
    nsCwdLabel.classList.add("placeholder");
    nsCwdPath.textContent = "";
  }
}

function setGroupLabel() {
  const g = newSessionGroup && groups.find((x) => x.id === newSessionGroup);
  nsGroupLabel.textContent = g ? g.name : "Ungrouped";
}

function modelLabelFor(value) {
  return (MODEL_OPTIONS.find((o) => o.value === value) || MODEL_OPTIONS[0])
    .label;
}

function setModelLabel() {
  nsModelLabel.textContent = modelLabelFor(newSessionModel);
}

function buildModelMenu() {
  nsModelMenu.replaceChildren();
  for (const o of MODEL_OPTIONS) {
    const b = document.createElement("button");
    b.type = "button";
    b.className =
      "ns-menu-item" + (newSessionModel === o.value ? " current" : "");
    b.textContent = o.label;
    b.addEventListener("click", () => {
      newSessionModel = o.value;
      setModelLabel();
      closeNsMenus();
    });
    nsModelMenu.appendChild(b);
  }
}

function suggestName() {
  if (!nsNameEdited)
    nsName.value = newSessionCwd ? baseName(newSessionCwd) : "";
}

function closeNsMenus() {
  nsCwdMenu.hidden = true;
  nsGroupMenu.hidden = true;
  nsModelMenu.hidden = true;
}

function toggleNsMenu(menu, build) {
  const willOpen = menu.hidden;
  closeNsMenus();
  if (willOpen) {
    build();
    menu.hidden = false;
  }
}

function selectCwd(dir) {
  newSessionCwd = dir;
  // reflect the group whose configured dir matches (same rule as auto-grouping),
  // unless the user already picked one explicitly
  if (!newSessionGroup) {
    newSessionGroup = autoGroupFor(dir) || "";
    setGroupLabel();
  }
  setCwdLabel();
  suggestName();
  nsExistingFilter.value = "";
  renderNsExisting();
  closeNsMenus();
}

function nsExistingNote(text) {
  const li = document.createElement("li");
  li.className = "nse-empty";
  li.textContent = text;
  nsExistingList.appendChild(li);
}

// Show resumable sessions whose cwd is the picked folder or nested under it.
// Visible whenever a folder is selected; shows a note when there are none.
function renderNsExisting() {
  nsExistingList.replaceChildren();
  const cwd = newSessionCwd;
  if (!cwd) {
    nsExistingWrap.hidden = true;
    return;
  }
  nsExistingWrap.hidden = false;

  const scoped = nsSessions.filter(
    (s) => s.cwd === cwd || s.cwd.startsWith(cwd + "/"),
  );
  if (!nsSessionsLoaded) {
    nsExistingFilter.hidden = true;
    nsExistingNote("Loading…");
    return;
  }
  if (!scoped.length) {
    nsExistingFilter.hidden = true;
    nsExistingNote("No previous sessions in this folder.");
    return;
  }

  nsExistingFilter.hidden = false;
  const q = nsExistingFilter.value.trim().toLowerCase();
  const matches = q
    ? scoped.filter(
        (s) =>
          s.cwd.toLowerCase().includes(q) ||
          s.preview.toLowerCase().includes(q) ||
          (s.name || "").toLowerCase().includes(q),
      )
    : scoped;
  if (!matches.length) {
    nsExistingNote("No matches.");
    return;
  }
  for (const s of matches) {
    const li = document.createElement("li");
    li.className = "ns-existing-item";
    li.title = `${s.name ? s.name + "\n" : ""}${s.cwd}\n${s.preview}`;
    if (s.name) {
      const nm = document.createElement("div");
      nm.className = "nse-name";
      nm.textContent = s.name;
      li.append(nm);
    }
    const preview = document.createElement("div");
    preview.className = "nse-preview";
    preview.textContent = s.preview;
    li.append(preview);
    if (s.cwd !== cwd) {
      const sub = document.createElement("div");
      sub.className = "nse-path";
      sub.textContent = s.cwd;
      li.append(sub);
    }
    li.addEventListener("click", () => {
      closeNewModal();
      startSession(s.cwd, { resume: s.id, name: s.name });
    });
    nsExistingList.appendChild(li);
  }
}

nsExistingFilter.addEventListener("input", renderNsExisting);

async function browseCwd() {
  try {
    const dir = await invoke("plugin:dialog|open", {
      options: {
        directory: true,
        multiple: false,
        title: "Choose a folder…",
        defaultPath: newSessionCwd || recentDirs[0] || undefined,
      },
    });
    if (dir) selectCwd(typeof dir === "string" ? dir : dir.path);
  } catch (e) {
    console.error(e);
  }
}

function buildCwdMenu() {
  nsCwdMenu.replaceChildren();
  for (const d of recentDirs) {
    const row = document.createElement("div");
    row.className =
      "ns-menu-item ns-recent" + (d === newSessionCwd ? " current" : "");
    const info = document.createElement("div");
    info.className = "ns-recent-info";
    const nm = document.createElement("span");
    nm.className = "ns-mi-name";
    nm.textContent = baseName(d);
    const pth = document.createElement("span");
    pth.className = "ns-mi-path";
    pth.textContent = d;
    info.append(nm, pth);
    info.addEventListener("click", () => selectCwd(d));
    const rm = document.createElement("button");
    rm.type = "button";
    rm.className = "ns-recent-remove";
    rm.textContent = "×";
    rm.title = "Remove from recent";
    rm.addEventListener("click", (e) => {
      e.stopPropagation();
      removeRecentDir(d);
    });
    row.append(info, rm);
    nsCwdMenu.appendChild(row);
  }
  if (recentDirs.length) {
    const sep = document.createElement("div");
    sep.className = "ns-menu-sep";
    nsCwdMenu.appendChild(sep);
  }
  const browse = document.createElement("button");
  browse.type = "button";
  browse.className = "ns-menu-item";
  browse.textContent = "Browse…";
  browse.addEventListener("click", browseCwd);
  nsCwdMenu.appendChild(browse);
}

function buildNsGroupMenu() {
  nsGroupMenu.replaceChildren();
  const add = (id, label) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className =
      "ns-menu-item" + (newSessionGroup === (id || "") ? " current" : "");
    b.textContent = label;
    b.addEventListener("click", () => {
      newSessionGroup = id || "";
      setGroupLabel();
      closeNsMenus();
    });
    nsGroupMenu.appendChild(b);
  };
  add("", "Ungrouped");
  for (const g of groups) add(g.id, g.name);
  const sep = document.createElement("div");
  sep.className = "ns-menu-sep";
  nsGroupMenu.appendChild(sep);
  const create = document.createElement("button");
  create.type = "button";
  create.className = "ns-menu-item";
  create.textContent = "New group…";
  create.addEventListener("click", () => {
    const g = createGroup();
    newSessionGroup = g.id;
    setGroupLabel();
    closeNsMenus();
  });
  nsGroupMenu.appendChild(create);
}

function openNewSession(presetGroupId = null) {
  const g = presetGroupId && groups.find((x) => x.id === presetGroupId);
  newSessionGroup = presetGroupId || "";
  newSessionCwd = (g && g.dir) || settings.defaultCwd || "";
  newSessionModel = settings.defaultModel || "";
  nsNameEdited = false;
  nsWorktree.checked = false;
  nsRemote.checked = !!settings.remoteByDefault;
  nsIncognito.checked = false;
  setCwdLabel();
  setGroupLabel();
  setModelLabel();
  suggestName();
  closeNsMenus();
  nsSessions = [];
  nsSessionsLoaded = false;
  nsExistingFilter.value = "";
  renderNsExisting();
  invoke("list_sessions")
    .then((items) => {
      nsSessions = items;
    })
    .catch(() => {})
    .finally(() => {
      nsSessionsLoaded = true;
      renderNsExisting();
    });
  newModal.hidden = false;
  nsName.focus();
}

function closeNewModal() {
  newModal.hidden = true;
  closeNsMenus();
}

async function createFromModal() {
  if (!newSessionCwd) {
    toggleNsMenu(nsCwdMenu, buildCwdMenu); // nudge the user to pick a directory
    return;
  }
  const cwd = newSessionCwd;
  const name = nsName.value.trim() || baseName(cwd);
  const groupId = newSessionGroup || null;
  const worktree = nsWorktree.checked;
  const model = newSessionModel;
  const remoteControl = nsRemote.checked;
  const incognito = nsIncognito.checked;
  closeNewModal();
  startSession(cwd, {
    name,
    groupId,
    worktree,
    model,
    remoteControl,
    incognito,
  });
}

document.getElementById("ns-cwd-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  toggleNsMenu(nsCwdMenu, buildCwdMenu);
});
document.getElementById("ns-group-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  toggleNsMenu(nsGroupMenu, buildNsGroupMenu);
});
document.getElementById("ns-model-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  toggleNsMenu(nsModelMenu, buildModelMenu);
});
nsName.addEventListener("input", () => {
  nsNameEdited = true;
});
nsName.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    createFromModal();
  }
});
document.getElementById("new-create").addEventListener("click", (e) => {
  e.stopPropagation(); // keep the "pick a directory" nudge menu open
  createFromModal();
});
document.getElementById("new-cancel").addEventListener("click", closeNewModal);
document.getElementById("new-close").addEventListener("click", closeNewModal);
document
  .getElementById("new-backdrop")
  .addEventListener("click", closeNewModal);

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
      s.cwd.toLowerCase().includes(q) ||
      s.preview.toLowerCase().includes(q) ||
      (s.name || "").toLowerCase().includes(q),
  );
  resumeList.replaceChildren();
  for (const s of filtered) {
    const li = document.createElement("li");
    li.className = "resume-item";

    const folder = document.createElement("div");
    folder.className = "r-folder";
    // prefer the session's name; fall back to the folder when it has none
    folder.textContent = s.name || baseName(s.cwd);
    const path = document.createElement("div");
    path.className = "r-path";
    path.textContent = s.cwd;
    const preview = document.createElement("div");
    preview.className = "r-preview";
    preview.textContent = s.preview;

    li.append(folder, path, preview);
    li.addEventListener("click", () => {
      closeResume();
      startSession(s.cwd, { resume: s.id, name: s.name });
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
  if (e.ctrlKey || e.metaKey) {
    if (e.shiftKey && (e.key === "F" || e.key === "f")) {
      e.preventDefault();
      openFind();
      return;
    }
    if (e.shiftKey) return; // don't treat other shifted combos below
    if (e.key === ",") {
      e.preventDefault();
      openSettings();
      return;
    }
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      changeFontSize(1);
      return;
    }
    if (e.key === "-" || e.key === "_") {
      e.preventDefault();
      changeFontSize(-1);
      return;
    }
  }
  if (e.key !== "Escape") return;
  if (!findBar.hidden) closeFind();
  if (!resumeModal.hidden) closeResume();
  if (!closeModal.hidden) closeCloseModal();
  if (!restartModal.hidden) closeRestartModal();
  if (!attentionMenu.hidden) attentionMenu.hidden = true;
  if (!settingsModal.hidden) closeSettings();
  if (!newModal.hidden) {
    if (!nsCwdMenu.hidden || !nsGroupMenu.hidden) closeNsMenus();
    else closeNewModal();
  }
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
  closeNsMenus();
  setThemeMenu.hidden = true;
  setFontMenu.hidden = true;
  if (!POPOUT) attentionMenu.hidden = true;
});
appMenu.addEventListener("click", (e) => {
  const action = e.target.closest("button")?.dataset.action;
  if (!action) return;
  appMenu.hidden = true;
  if (action === "new") openNewSession();
  else if (action === "resume") openResume();
  else if (action === "group") createGroupAndRename();
  else if (action === "settings") openSettings();
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
  document.body.classList.toggle("maximized", max); // flatten the window border
}

document
  .getElementById("win-min")
  .addEventListener("click", () => invoke("minimize_window"));
winMax.addEventListener("click", () =>
  invoke("toggle_maximize").then(syncMaxIcon),
);
document.getElementById("win-close").addEventListener("click", () => {
  // Pop-out: close just this window (the session keeps running in the main
  // window). Main window: hide to tray so sessions keep running.
  if (POPOUT) appWindow.close();
  else invoke("hide_to_tray");
});

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
appWindow.onFocusChanged(({ payload }) => {
  appFocused = payload;
  if (payload) checkUpdate();
});

// "Restart to update": with no auto-updater, detect that the on-disk binary was
// replaced (a new .deb installed while running) and offer a restart. Checked on
// focus and on an interval; only in the main window.
const titlebarUpdate = document.getElementById("titlebar-update");
let updateShown = false;
async function checkUpdate() {
  if (POPOUT || updateShown) return;
  try {
    if (await invoke("update_available")) {
      updateShown = true;
      titlebarUpdate.hidden = false;
    }
  } catch {
    /* ignore */
  }
}
const restartModal = document.getElementById("restart-modal");
function closeRestartModal() {
  restartModal.hidden = true;
}
// Restart only after warning if sessions are mid-task (restart stops all running
// PTYs; they come back cold/resumable, but active work would be interrupted).
function requestRestart() {
  const working = [...sessions.values()].filter((x) => x.live && x.working);
  if (!working.length) {
    invoke("restart_app").catch(() => {});
    return;
  }
  const n = working.length;
  document.getElementById("restart-msg").textContent =
    `${n} session${n === 1 ? " is" : "s are"} still working. Restarting stops all running sessions — they can be resumed afterward, but in-progress work is interrupted. Restart now?`;
  restartModal.hidden = false;
}
if (!POPOUT) {
  document
    .getElementById("titlebar-update-restart")
    .addEventListener("click", requestRestart);
  document
    .getElementById("titlebar-update-dismiss")
    .addEventListener("click", () => (titlebarUpdate.hidden = true));
  document
    .getElementById("restart-cancel")
    .addEventListener("click", closeRestartModal);
  document
    .getElementById("restart-backdrop")
    .addEventListener("click", closeRestartModal);
  document.getElementById("restart-confirm").addEventListener("click", () => {
    closeRestartModal();
    invoke("restart_app").catch(() => {});
  });
  setInterval(checkUpdate, 60000);
}

// Dropping OS files onto the window types their path(s) into the active session
// (Tauri intercepts native file drops, so HTML5 dnd events don't fire here).
const dropOverlay = document.getElementById("drop-overlay");
appWindow.onDragDropEvent((event) => {
  const p = event.payload;
  if (p.type === "enter" || p.type === "over") {
    dropOverlay.hidden = !activeId; // only when there's a terminal to drop into
    return;
  }
  dropOverlay.hidden = true; // drop / leave / cancel
  if (p.type !== "drop" || !Array.isArray(p.paths) || !p.paths.length) return;
  if (!activeId) return;
  const text =
    p.paths.map((path) => (/\s/.test(path) ? `"${path}"` : path)).join(" ") +
    " ";
  invoke("write_pty", { id: activeId, data: text }).catch(() => {});
});

document
  .getElementById("new-session")
  .addEventListener("click", () => openNewSession());
document.getElementById("resume-session").addEventListener("click", openResume);
invoke("is_dev").then((dev) => document.body.classList.toggle("dev", !!dev));

// --- close-session confirmation ---
const closeModal = document.getElementById("close-modal");
const closeMsg = document.getElementById("close-msg");
let pendingCloseId = null;

function requestClose(id) {
  const s = sessions.get(id);
  if (!s) return;
  if (!s.live) {
    closeSession(id); // nothing running — no need to confirm
    return;
  }
  pendingCloseId = id;
  if (s.incognito) {
    closeMsg.textContent = `“${s.name}” is an incognito session. Closing it stops claude and permanently discards the conversation — it isn’t saved anywhere and can’t be resumed.`;
  } else {
    closeMsg.textContent = `“${s.name}” — its claude process will be stopped.`;
  }
  closeMsg.classList.toggle("close-warn", !!s.incognito);
  closeModal.hidden = false;
}

function closeCloseModal() {
  closeModal.hidden = true;
  pendingCloseId = null;
}

document
  .getElementById("close-cancel")
  .addEventListener("click", closeCloseModal);
document
  .getElementById("close-backdrop")
  .addEventListener("click", closeCloseModal);
document.getElementById("close-confirm").addEventListener("click", () => {
  const id = pendingCloseId;
  closeCloseModal();
  if (id) closeSession(id);
});

// --- sidebar collapse ---
function applySidebarCollapsed() {
  document.body.classList.toggle("sidebar-collapsed", sidebarCollapsed);
  const btn = document.getElementById("sidebar-toggle");
  btn.textContent = sidebarCollapsed ? "»" : "«";
  btn.title = sidebarCollapsed ? "Show sidebar" : "Hide sidebar";
  const s = sessions.get(activeId);
  if (s && s.fit) s.fit.fit();
}

document.getElementById("sidebar-toggle").addEventListener("click", () => {
  sidebarCollapsed = !sidebarCollapsed;
  applySidebarCollapsed();
  persist();
});

// --- window resize grips (native decorations are off) ---
for (const grip of document.querySelectorAll(".resizer")) {
  grip.addEventListener("mousedown", (e) => {
    e.preventDefault();
    appWindow.startResizeDragging(grip.dataset.dir);
  });
}

// --- settings ---
const settingsModal = document.getElementById("settings-modal");
const setDefcwdPath = document.getElementById("set-defcwd");
const setThemeMenu = document.getElementById("set-theme-menu");
const setThemeLabel = document.getElementById("set-theme-label");
const setFontMenu = document.getElementById("set-font-menu");
const setFontLabel = document.getElementById("set-font-label");

let fontListCache = null;

// Terminal font picker: a search box over the installed monospace families
// (JetBrains Mono first), each item previewed in its own font.
async function buildFontMenu() {
  setFontMenu.replaceChildren();
  if (!fontListCache) {
    try {
      fontListCache = await invoke("list_fonts");
    } catch {
      fontListCache = [];
    }
  }
  const families = [
    "JetBrains Mono",
    ...fontListCache.filter((f) => f && f !== "JetBrains Mono"),
  ];

  const filter = document.createElement("input");
  filter.className = "ns-input ns-menu-filter";
  filter.type = "text";
  filter.placeholder = "Search fonts…";
  filter.spellcheck = false;
  filter.addEventListener("click", (e) => e.stopPropagation()); // don't close the menu
  const list = document.createElement("div");
  list.className = "ns-menu-list";

  const renderItems = () => {
    list.replaceChildren();
    const q = filter.value.trim().toLowerCase();
    const shown = q
      ? families.filter((f) => f.toLowerCase().includes(q))
      : families;
    for (const fam of shown) {
      const b = document.createElement("button");
      b.type = "button";
      b.className =
        "ns-menu-item" + (settings.terminalFont === fam ? " current" : "");
      b.textContent = fam;
      b.style.fontFamily = `"${fam}", ui-monospace, monospace`;
      b.addEventListener("click", () => {
        settings.terminalFont = fam;
        setFontLabel.textContent = fam;
        applyTerminalFont();
        persist();
        setFontMenu.hidden = true;
      });
      list.appendChild(b);
    }
    if (!shown.length) {
      const empty = document.createElement("div");
      empty.className = "ns-menu-empty";
      empty.textContent = "No matching fonts";
      list.appendChild(empty);
    }
  };

  filter.addEventListener("input", renderItems);
  setFontMenu.append(filter, list);
  renderItems();
  filter.focus();
}

function themeLabel(value) {
  return (getTheme(value) || BUILTIN_THEMES.frappe).label;
}

// A compact preview strip: background, six ANSI accents, foreground.
function swatchStrip(theme) {
  const strip = document.createElement("span");
  strip.className = "theme-swatches";
  const t = theme.term;
  for (const col of [
    t.background,
    t.ansi[1],
    t.ansi[2],
    t.ansi[3],
    t.ansi[4],
    t.ansi[5],
    t.ansi[6],
    t.foreground,
  ]) {
    const sw = document.createElement("i");
    sw.style.background = col;
    strip.appendChild(sw);
  }
  return strip;
}

function buildThemeMenu() {
  setThemeMenu.replaceChildren();
  for (const [key, theme] of themeEntries()) {
    const b = document.createElement("button");
    b.type = "button";
    b.className =
      "ns-menu-item theme-item" + (settings.theme === key ? " current" : "");
    const name = document.createElement("span");
    name.className = "theme-name";
    name.textContent = theme.label;
    b.append(name, swatchStrip(theme));
    b.addEventListener("click", () => {
      settings.theme = key;
      setThemeLabel.textContent = theme.label;
      applyTheme();
      persist();
      setThemeMenu.hidden = true;
    });
    setThemeMenu.appendChild(b);
  }
}

function applyTheme() {
  const theme = getTheme(settings.theme) || BUILTIN_THEMES.frappe;
  const c = chromeFor(theme);
  const root = document.documentElement;
  root.style.setProperty("--bg", c.bg);
  root.style.setProperty("--bg-alt", c.bgAlt);
  root.style.setProperty("--border", c.border);
  root.style.setProperty("--fg", c.fg);
  root.style.setProperty("--fg-dim", c.fgDim);
  root.style.setProperty("--accent", c.accent);
  root.style.setProperty("--accent-bg", c.accentBg);
  root.style.setProperty("--red", c.red);
  root.style.setProperty("--titlebar", c.titlebar);
  root.dataset.mode = isDarkColor(c.bg) ? "dark" : "light";
  const xt = xtermTheme(theme.term);
  for (const s of sessions.values()) if (s.term) s.term.options.theme = xt;
}

// --- custom themes (paste a Ghostty palette) ---
function renderCustomThemes() {
  const box = document.getElementById("set-custom-themes");
  if (!box) return;
  box.replaceChildren();
  const list = settings.customThemes || [];
  if (!list.length) {
    const empty = document.createElement("div");
    empty.className = "set-hint";
    empty.textContent = "None yet.";
    box.appendChild(empty);
    return;
  }
  for (const t of list) {
    const row = document.createElement("div");
    row.className = "set-theme-row";
    const name = document.createElement("span");
    name.className = "set-theme-row-name";
    name.textContent = t.label;
    const del = document.createElement("button");
    del.type = "button";
    del.className = "set-theme-del";
    del.textContent = "✕";
    del.title = "Delete theme";
    del.addEventListener("click", () => deleteCustomTheme(t.id));
    row.append(swatchStrip(t), name, del);
    box.appendChild(row);
  }
}

function deleteCustomTheme(id) {
  settings.customThemes = (settings.customThemes || []).filter(
    (t) => t.id !== id,
  );
  if (settings.theme === id) {
    settings.theme = "frappe";
    setThemeLabel.textContent = themeLabel(settings.theme);
    applyTheme();
  }
  persist();
  renderCustomThemes();
}

function clearThemeForm() {
  document.getElementById("set-theme-name").value = "";
  document.getElementById("set-theme-paste").value = "";
  const err = document.getElementById("set-theme-err");
  err.hidden = true;
  err.textContent = "";
}

function saveCustomTheme() {
  const err = document.getElementById("set-theme-err");
  const parsed = parseGhosttyTheme(
    document.getElementById("set-theme-paste").value,
  );
  if (!parsed) {
    err.textContent =
      "Couldn't parse — need palette 0–15, background, and foreground.";
    err.hidden = false;
    return;
  }
  const label =
    document.getElementById("set-theme-name").value.trim() || "Custom theme";
  const id = "custom-" + crypto.randomUUID();
  settings.customThemes = settings.customThemes || [];
  settings.customThemes.push({ id, label, term: parsed.term });
  settings.theme = id; // select the new theme right away
  setThemeLabel.textContent = label;
  applyTheme();
  persist();
  renderCustomThemes();
  document.getElementById("set-theme-form").hidden = true;
  clearThemeForm();
}

function applySettings() {
  notificationsEnabled = settings.notifications !== false;
  applyTheme();
  applyTerminalFontSize();
  applyTerminalFont();
  loadBellSound();
  invoke("set_claude_path", { path: settings.claudePath || null }).catch(
    () => {},
  );
}

function applyTerminalFontSize() {
  const px = fontSizePx();
  for (const s of sessions.values()) {
    if (!s.term) continue;
    s.term.options.fontSize = px;
    if (s.el && s.el.style.display !== "none") s.fit.fit();
  }
}

function applyTerminalFont() {
  const stack = terminalFontStack();
  for (const s of sessions.values()) {
    if (!s.term) continue;
    s.term.options.fontFamily = stack;
    s.el?.classList.toggle("ligatures", !!settings.ligatures);
    // Match the ligature joiner to the setting (register/deregister once).
    if (settings.ligatures && s.ligatureJoinerId == null) {
      s.ligatureJoinerId = s.term.registerCharacterJoiner(ligatureJoiner);
    } else if (!settings.ligatures && s.ligatureJoinerId != null) {
      s.term.deregisterCharacterJoiner(s.ligatureJoinerId);
      s.ligatureJoinerId = null;
    }
    if (s.el && s.el.style.display !== "none") s.fit.fit();
  }
}

// Ctrl/Cmd +/- : bump the terminal font size (clamped), live + persisted.
const fontHud = document.getElementById("font-hud");
let fontHudTimer = null;
function showFontHud(size) {
  fontHud.textContent = `${size}pt`;
  fontHud.hidden = false;
  clearTimeout(fontHudTimer);
  fontHudTimer = setTimeout(() => {
    fontHud.hidden = true;
  }, 1000);
}

function changeFontSize(delta) {
  const cur = settings.terminalFontSize || 11;
  const next = clampFontPt(cur + delta);
  if (next === cur) return;
  settings.terminalFontSize = next;
  applyTerminalFontSize();
  const fs = document.getElementById("set-fontsize");
  if (fs) fs.value = next;
  showFontHud(next);
  persist();
}

function setDefCwdLabelText() {
  setDefcwdPath.textContent = settings.defaultCwd || "— none —";
  setDefcwdPath.title = settings.defaultCwd || "";
}

function setBellFileLabel() {
  const el = document.getElementById("set-bell-file");
  el.textContent = settings.bellSoundFile
    ? baseName(settings.bellSoundFile)
    : "Default sound";
  el.title = settings.bellSoundFile || "";
}

function showSettingsSection(section) {
  for (const tab of settingsModal.querySelectorAll(".settings-tab"))
    tab.classList.toggle("active", tab.dataset.section === section);
  for (const sec of settingsModal.querySelectorAll(".settings-section"))
    sec.hidden = sec.dataset.section !== section;
}

// Ghost button used in the settings group manager.
function ghostBtn(label, onClick) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "set-ghost";
  b.textContent = label;
  b.addEventListener("click", onClick);
  return b;
}

function renderSettingsGroups() {
  const box = document.getElementById("set-groups");
  box.replaceChildren();
  if (!groups.length) {
    box.appendChild(
      Object.assign(document.createElement("div"), {
        className: "set-hint",
        textContent: "No groups yet.",
      }),
    );
    return;
  }
  for (const g of groups) {
    const row = document.createElement("div");
    row.className = "set-group-row";
    const name = document.createElement("input");
    name.className = "set-group-name-input";
    name.value = g.name;
    name.spellcheck = false;
    name.addEventListener("change", () => {
      const v = name.value.trim();
      if (v) {
        g.name = v;
        persist();
        renderSessionList();
      } else {
        name.value = g.name;
      }
    });
    const dir = Object.assign(document.createElement("span"), {
      className: "set-group-dir",
      textContent: g.dir || "— no folder —",
      title: g.dir || "",
    });
    row.append(
      name,
      dir,
      ghostBtn(g.dir ? "Change" : "Set folder", async () => {
        await setGroupDir(g);
        renderSettingsGroups();
      }),
    );
    if (g.dir) {
      row.append(
        ghostBtn("Clear", () => {
          g.dir = null;
          persist();
          renderSessionList();
          renderSettingsGroups();
        }),
      );
    }
    row.append(
      ghostBtn("Delete", () => {
        deleteGroup(g.id);
        renderSettingsGroups();
      }),
    );
    box.appendChild(row);
  }
}

function openSettings() {
  setThemeLabel.textContent = themeLabel(settings.theme);
  setThemeMenu.hidden = true;
  setFontLabel.textContent = settings.terminalFont || "JetBrains Mono";
  setFontMenu.hidden = true;
  document.getElementById("set-ligatures").checked = !!settings.ligatures;
  document.getElementById("set-claude-path").value = settings.claudePath;
  document.getElementById("set-fontsize").value =
    settings.terminalFontSize || 11;
  document.getElementById("set-notifications").checked =
    settings.notifications !== false;
  document.getElementById("set-bell-sound").checked =
    settings.bellSound !== false;
  document.getElementById("set-resume-on-start").checked =
    !!settings.resumeOnStart;
  invoke("plugin:autostart|is_enabled")
    .then((on) => {
      document.getElementById("set-run-on-startup").checked = !!on;
    })
    .catch(() => {});
  setBellFileLabel();
  document.getElementById("set-remote-default").checked =
    !!settings.remoteByDefault;
  setDefCwdLabelText();
  renderSettingsGroups();
  renderCustomThemes();
  document.getElementById("set-theme-form").hidden = true;
  clearThemeForm();
  showSettingsSection("appearance");
  invoke("app_info")
    .then((info) => {
      document.getElementById("about-version").textContent = `v${info.version}`;
      document.getElementById("about-desc").textContent = info.description;
    })
    .catch(() => {});
  settingsModal.hidden = false;
}

function closeSettings() {
  settingsModal.hidden = true;
}

for (const tab of document.querySelectorAll(".settings-tab"))
  tab.addEventListener("click", () => showSettingsSection(tab.dataset.section));
document.getElementById("set-add-group").addEventListener("click", () => {
  createGroup();
  renderSettingsGroups();
  // focus the new group's name so it can be renamed immediately
  const inputs = document.querySelectorAll("#set-groups .set-group-name-input");
  const last = inputs[inputs.length - 1];
  if (last) {
    last.focus();
    last.select();
  }
});

document.getElementById("set-theme-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  const willOpen = setThemeMenu.hidden;
  setThemeMenu.hidden = true;
  if (willOpen) {
    buildThemeMenu();
    setThemeMenu.hidden = false;
  }
});
document.getElementById("set-font-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  const willOpen = setFontMenu.hidden;
  setFontMenu.hidden = true;
  if (willOpen) {
    buildFontMenu();
    setFontMenu.hidden = false;
  }
});
document.getElementById("set-ligatures").addEventListener("change", (e) => {
  settings.ligatures = e.target.checked;
  applyTerminalFont();
  persist();
});
document.getElementById("set-add-theme").addEventListener("click", () => {
  const f = document.getElementById("set-theme-form");
  f.hidden = !f.hidden;
  if (!f.hidden) document.getElementById("set-theme-name").focus();
});
document.getElementById("set-theme-cancel").addEventListener("click", () => {
  document.getElementById("set-theme-form").hidden = true;
  clearThemeForm();
});
document
  .getElementById("set-theme-save")
  .addEventListener("click", saveCustomTheme);
document.getElementById("set-claude-path").addEventListener("change", (e) => {
  settings.claudePath = e.target.value.trim();
  invoke("set_claude_path", { path: settings.claudePath || null }).catch(
    () => {},
  );
  persist();
});
document.getElementById("set-notifications").addEventListener("change", (e) => {
  settings.notifications = e.target.checked;
  notificationsEnabled = e.target.checked;
  persist();
});
document.getElementById("set-bell-sound").addEventListener("change", (e) => {
  settings.bellSound = e.target.checked;
  if (e.target.checked) playBellSound(); // preview + unlock audio on the gesture
  persist();
});
document
  .getElementById("set-resume-on-start")
  .addEventListener("change", (e) => {
    settings.resumeOnStart = e.target.checked;
    persist();
  });
document
  .getElementById("set-run-on-startup")
  .addEventListener("change", async (e) => {
    const on = e.target.checked;
    try {
      await invoke(on ? "plugin:autostart|enable" : "plugin:autostart|disable");
    } catch (err) {
      console.error(err);
      e.target.checked = !on; // OS refused — reflect the real state
    }
  });
document
  .getElementById("set-remote-default")
  .addEventListener("change", (e) => {
    settings.remoteByDefault = e.target.checked;
    persist();
  });
document.getElementById("set-fontsize").addEventListener("change", (e) => {
  const n = parseInt(e.target.value, 10);
  settings.terminalFontSize = clampFontPt(isNaN(n) ? 11 : n);
  e.target.value = settings.terminalFontSize;
  applyTerminalFontSize();
  persist();
});
document
  .getElementById("set-defcwd-btn")
  .addEventListener("click", async () => {
    try {
      const dir = await invoke("plugin:dialog|open", {
        options: {
          directory: true,
          title: "Default directory for new sessions",
          defaultPath: settings.defaultCwd || undefined,
        },
      });
      if (dir) {
        settings.defaultCwd = typeof dir === "string" ? dir : dir.path;
        setDefCwdLabelText();
        persist();
      }
    } catch (e) {
      console.error(e);
    }
  });
document.getElementById("set-defcwd-clear").addEventListener("click", () => {
  settings.defaultCwd = "";
  setDefCwdLabelText();
  persist();
});
document
  .getElementById("set-bell-file-btn")
  .addEventListener("click", async () => {
    try {
      const file = await invoke("plugin:dialog|open", {
        options: {
          title: "Choose a bell sound",
          filters: [
            {
              name: "Audio",
              extensions: [
                "wav",
                "mp3",
                "ogg",
                "oga",
                "opus",
                "flac",
                "m4a",
                "aac",
              ],
            },
          ],
        },
      });
      if (file) {
        settings.bellSoundFile = typeof file === "string" ? file : file.path;
        setBellFileLabel();
        await loadBellSound();
        playBellSound(); // preview + unlock audio on this gesture
        persist();
      }
    } catch (e) {
      console.error(e);
    }
  });
document.getElementById("set-bell-file-clear").addEventListener("click", () => {
  settings.bellSoundFile = "";
  setBellFileLabel();
  loadBellSound();
  persist();
});
document
  .getElementById("settings-close")
  .addEventListener("click", closeSettings);
document
  .getElementById("settings-backdrop")
  .addEventListener("click", closeSettings);

// --- find in session (Ctrl/Cmd+Shift+F) ---
const findBar = document.getElementById("find-bar");
const findInput = document.getElementById("find-input");
const findCount = document.getElementById("find-count");
const FIND_OPTS = {
  decorations: {
    matchBackground: "#5c4a1a",
    matchBorder: "#f4b45a",
    matchOverviewRuler: "#f4b45a",
    activeMatchBackground: "#f4b45a",
    activeMatchBorder: "#f4b45a",
    activeMatchColorOverviewRuler: "#f4b45a",
  },
};

function runFind(prev) {
  const s = sessions.get(activeId);
  if (!s || !s.search) return;
  const q = findInput.value;
  if (!q) {
    s.search.clearDecorations?.();
    findCount.textContent = "";
    return;
  }
  if (prev) s.search.findPrevious(q, FIND_OPTS);
  else s.search.findNext(q, FIND_OPTS);
}

function openFind() {
  if (!activeId) return;
  findBar.hidden = false;
  findInput.focus();
  findInput.select();
  if (findInput.value) runFind(false);
}

function closeFind() {
  findBar.hidden = true;
  const s = sessions.get(activeId);
  s?.search?.clearDecorations?.();
  s?.term?.focus();
}

findInput.addEventListener("input", () => runFind(false));
findInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    runFind(e.shiftKey);
  } else if (e.key === "Escape") {
    e.preventDefault();
    closeFind();
  }
});
document
  .getElementById("find-next")
  .addEventListener("click", () => runFind(false));
document
  .getElementById("find-prev")
  .addEventListener("click", () => runFind(true));
document.getElementById("find-close").addEventListener("click", closeFind);

if (POPOUT) runPopout();
else restore();
