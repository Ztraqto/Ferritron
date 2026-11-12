// `window.ferritron` in the Android app: the same API a plugin's web editor
// has inside a DAW (params, builtins, set/begin/end/edit, onChange, t,
// language), over the activity's `ferritronAndroid` interface. Loaded
// before the page's own scripts.
//
// Not on Android: ferritron.instance (null) and messages with the plugin. The streams API
// (onStream) exists but this app does not carry stream data yet. The end of this file is
// ferritron-web's extras.js (theme, settings, contextMenu, ...), kept identical by a test there.
//
// SPDX-License-Identifier: MIT OR Apache-2.0
(() => {
  const host = window.ferritronAndroid;
  const description = JSON.parse(host.describe());
  const params = {}, builtins = {};
  for (const p of description.params ?? []) {
    if (p.key === "ferritron.mcp") continue;
    const entry = { id: p.id, name: p.name, unit: p.unit, kind: p.kind, min: p.min, max: p.max, flags: p.flags, value: p.default, normalized: 0, text: "" };
    if (p.choices) entry.choices = p.choices;
    (p.key.startsWith("ferritron.") ? builtins : params)[p.key] = entry;
  }
  const byId = {};
  for (const [key, p] of [...Object.entries(params), ...Object.entries(builtins)]) byId[p.id] = [key, p];
  const messages = description.messages ?? {};
  const listeners = [];
  const id = (key) => {
    const p = params[key] ?? builtins[key];
    if (!p) throw new Error(`ferritron: unknown parameter "${key}"`);
    return p.id;
  };
  const refresh = () => {
    let values;
    try { values = JSON.parse(host.values()); } catch { return; }
    for (const [pid, normalized, value, text] of values) {
      const entry = byId[pid];
      if (!entry) continue;
      const [key, p] = entry;
      if (p.normalized === normalized && p.text === text) continue;
      Object.assign(p, { value, normalized, text });
      for (const fn of listeners) fn(key, p);
    }
  };
  window.ipc = { postMessage: (m) => host.post(String(m)) };
  window.ferritron = {
    params, builtins, description,
    language: description.language ?? "en",
    plugin: { id: description.id ?? "", name: description.name ?? "", vendor: description.vendor ?? "" },
    nativeContextMenu: true,
    instance: null,
    t(key, args = {}) {
      const text = messages[key] ?? key;
      return text.replace(/\{\$(\w+)\}/g, (m, name) => (name in args ? String(args[name]) : m));
    },
    onChange(fn) { listeners.push(fn); },
    onInstanceChange() {},
    send() {},
    onMessage() {},
    begin() {},
    set(key, value) { host.post(`set:${id(key)}:${Number(value)}`); refresh(); },
    end() {},
    edit(key, value) { this.set(key, value); },
    /** MIDI bytes to the plugin (the on-screen keyboard). */
    midi(status, data1, data2) { host.post(`midi:${status}:${data1}:${data2}`); },
    _update() {},
  };
  refresh();
  setInterval(refresh, 50);
})();

// --- extras.js begins (identical to crates/ferritron-web/src/extras.js) ---
// Parts of "window.ferritron" that need nothing from the host: the same text runs in the native
// web view (ferritron-web), in the Web Audio Module page (ferritron-wam) and in the Android app
// (ferritron-standalone). It is added after the main bridge and only fills in what is missing.
// Copies of this file are kept byte for byte in crates/ferritron-wam/web/gui.js and
// crates/ferritron-standalone/android/bridge.js (a test of ferritron-web checks it).
//
//   ferritron.onStream(name, fn)       newest values of a "[streams]" entry (a Float32Array)
//   ferritron.theme                    { dark, highContrast, reducedMotion, accent: [r, g, b] | null }
//   ferritron.onThemeChange(fn)        the system theme changed (prefers-color-scheme and friends)
//   ferritron.settings                 get(key, fallback), set(key, value), remove(key), keys(), onChange(fn)
//   ferritron.contextMenu(items, pos)  a menu drawn by Ferritron; resolves the chosen item's id or null
//   ferritron.openFile / saveFile      resolve null where the host has no file dialog
//
// SPDX-License-Identifier: MIT OR Apache-2.0
(() => {
  const f = window.ferritron;
  if (!f || f.__extras) return;
  f.__extras = true;

  // --- streams: the API exists everywhere; hosts that carry data call _stream -----------------
  if (typeof f.onStream !== "function") {
    const listeners = {};
    f.onStream = (name, fn) => { (listeners[name] ??= []).push(fn); };
    f._stream = (name, values) => {
      if (typeof values === "string") {
        const bin = atob(values);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        values = new Float32Array(bytes.buffer);
      }
      for (const fn of listeners[name] ?? []) fn(values);
    };
  }

  // --- theme -------------------------------------------------------------------------------------
  const query = (q) => (window.matchMedia ? window.matchMedia(q) : { matches: false, addEventListener() {} });
  const darkQuery = query("(prefers-color-scheme: dark)");
  const forcedQuery = query("(forced-colors: active)");
  const motionQuery = query("(prefers-reduced-motion: reduce)");
  const accent = () => {
    try {
      if (!(window.CSS && CSS.supports && CSS.supports("color", "AccentColor")) || !document.documentElement) return null;
      const probe = document.createElement("span");
      probe.style.color = "AccentColor";
      document.documentElement.appendChild(probe);
      const rgb = getComputedStyle(probe).color.match(/\d+(\.\d+)?/g);
      probe.remove();
      return rgb && rgb.length >= 3 ? rgb.slice(0, 3).map((v) => Math.round(Number(v))) : null;
    } catch { return null; }
  };
  const themeListeners = [];
  const readTheme = () => ({ dark: darkQuery.matches, highContrast: forcedQuery.matches, reducedMotion: motionQuery.matches, accent: accent() });
  Object.defineProperty(f, "theme", { get: readTheme, enumerable: true, configurable: true });
  f.onThemeChange = (fn) => { themeListeners.push(fn); };
  const themeChanged = () => { const t = readTheme(); for (const fn of themeListeners) fn(t); };
  for (const q of [darkQuery, forcedQuery, motionQuery]) {
    if (q.addEventListener) q.addEventListener("change", themeChanged);
  }

  // --- settings: kept across sessions, shared by the editors of one plugin ---------------------
  // In a plugin, the host side keeps them in the plugin's settings file (shared with
  // ferritron::settings in Rust, every host and process): it calls _settingsHost.
  // Elsewhere (web, phone, tests) the browser's local storage keeps them.
  const prefix = "ferritron.settings." + (f.plugin?.id ?? "default") + ".";
  const memory = {};
  const settingListeners = [];
  const store = (() => {
    try {
      const s = window.localStorage;
      s.getItem("ferritron.settings.probe");
      return s;
    } catch { return null; }
  })();
  const read = (key) => {
    const raw = hosted ? hosted[key] ?? null : store ? store.getItem(prefix + key) : memory[key] ?? null;
    if (raw === null || raw === undefined) return undefined;
    try { return JSON.parse(raw); } catch { return undefined; }
  };
  const notify = (key, value) => { for (const fn of settingListeners) fn(key, value); };
  let hosted = null; // the host's copy: { key: JSON text }
  f.settings = {
    get(key, fallback) { const v = read(key); return v === undefined ? fallback : v; },
    set(key, value) {
      const raw = JSON.stringify(value);
      if (hosted) { hosted[key] = raw; f._post?.("settings:" + JSON.stringify([key, value])); }
      else if (store) { try { store.setItem(prefix + key, raw); } catch { memory[key] = raw; } } else memory[key] = raw;
      notify(key, value);
    },
    remove(key) {
      if (hosted) { delete hosted[key]; f._post?.("settings:" + JSON.stringify([key])); }
      else if (store) store.removeItem(prefix + key); else delete memory[key];
      notify(key, undefined);
    },
    keys() {
      const names = [];
      if (hosted) names.push(...Object.keys(hosted));
      else if (store) {
        for (let i = 0; i < store.length; i++) { const k = store.key(i); if (k && k.startsWith(prefix)) names.push(k.slice(prefix.length)); }
      } else names.push(...Object.keys(memory));
      return names.sort();
    },
    onChange(fn) { settingListeners.push(fn); },
  };
  // The host keeps the settings: take its values; it tells us of other editors' changes.
  f._settingsHost = (initial) => {
    hosted = {};
    for (const [k, v] of Object.entries(initial ?? {})) hosted[k] = JSON.stringify(v);
  };
  f._setting = (key, value) => {
    if (!hosted) return;
    const raw = value === undefined ? undefined : JSON.stringify(value);
    if (hosted[key] === raw) return; // our own change coming back
    if (raw === undefined) delete hosted[key]; else hosted[key] = raw;
    notify(key, value);
  };
  // Another editor of the same plugin (another document) changed a setting.
  if (store && window.addEventListener) {
    window.addEventListener("storage", (e) => {
      if (!e.key || !e.key.startsWith(prefix)) return;
      let value;
      try { value = e.newValue === null ? undefined : JSON.parse(e.newValue); } catch { value = undefined; }
      notify(e.key.slice(prefix.length), value);
    });
  }

  // --- a context menu of our own (where the host has none to lend) --------------------------------
  let openMenu = null;
  f.contextMenu = (items, position = {}) => new Promise((resolve) => {
    if (openMenu) openMenu(null);
    const doc = document;
    const dark = darkQuery.matches;
    const menu = doc.createElement("div");
    menu.setAttribute("role", "menu");
    menu.style.cssText = "position:fixed;z-index:2147483647;min-width:160px;padding:4px 0;font:13px system-ui,sans-serif;border-radius:6px;box-shadow:0 4px 16px rgba(0,0,0,.35);user-select:none;" +
      (dark ? "background:#2b2d33;color:#e8e9ec;border:1px solid #444850;" : "background:#fff;color:#1b1c1f;border:1px solid #c9ccd2;");
    const rows = [];
    let active = -1;
    const mark = (i) => {
      active = i;
      rows.forEach((r, n) => { r.el.style.background = n === i ? (dark ? "#3d6bd1" : "#cfe0ff") : "transparent"; });
    };
    const close = (id) => {
      openMenu = null;
      menu.remove();
      doc.removeEventListener("pointerdown", outside, true);
      doc.removeEventListener("keydown", keys, true);
      resolve(id);
    };
    const outside = (e) => { if (!menu.contains(e.target)) close(null); };
    const move = (step) => {
      if (!rows.length) return;
      let i = active;
      for (let n = 0; n < rows.length; n++) { i = (i + step + rows.length) % rows.length; if (!rows[i].disabled) break; }
      mark(i);
    };
    const keys = (e) => {
      if (e.key === "Escape") { e.preventDefault(); close(null); }
      else if (e.key === "ArrowDown") { e.preventDefault(); move(1); }
      else if (e.key === "ArrowUp") { e.preventDefault(); move(-1); }
      else if (e.key === "Enter" && active >= 0 && !rows[active].disabled) { e.preventDefault(); close(rows[active].id); }
    };
    for (const item of items) {
      if (item.separator) {
        const hr = doc.createElement("div");
        hr.style.cssText = "height:1px;margin:4px 0;background:" + (dark ? "#444850" : "#d8dae0");
        hr.setAttribute("role", "separator");
        menu.appendChild(hr);
        continue;
      }
      const el = doc.createElement("div");
      const disabled = item.enabled === false;
      el.setAttribute("role", item.checked !== undefined ? "menuitemcheckbox" : "menuitem");
      if (item.checked !== undefined) el.setAttribute("aria-checked", String(!!item.checked));
      if (disabled) el.setAttribute("aria-disabled", "true");
      el.textContent = (item.checked ? "✓ " : item.checked === false ? "  " : "") + item.label;
      el.style.cssText = "padding:5px 16px;white-space:nowrap;" + (disabled ? "opacity:.45;" : "cursor:default;");
      const row = { el, id: item.id, disabled };
      rows.push(row);
      const index = rows.length - 1;
      el.addEventListener("pointermove", () => { if (!disabled) mark(index); });
      el.addEventListener("click", () => { if (!disabled) close(item.id); });
      menu.appendChild(el);
    }
    doc.body.appendChild(menu);
    const width = menu.offsetWidth, height = menu.offsetHeight;
    const x = Math.max(0, Math.min(position.x ?? 0, window.innerWidth - width));
    const y = Math.max(0, Math.min(position.y ?? 0, window.innerHeight - height));
    menu.style.left = x + "px";
    menu.style.top = y + "px";
    openMenu = close;
    doc.addEventListener("pointerdown", outside, true);
    doc.addEventListener("keydown", keys, true);
  });

  // --- file dialogs: the native web view has them; elsewhere the answer is "nothing chosen" ------
  if (typeof f.openFile !== "function") f.openFile = () => Promise.resolve(null);
  if (typeof f.saveFile !== "function") f.saveFile = () => Promise.resolve(null);
})();
