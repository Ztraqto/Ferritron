// `window.ferritron` in the Android app: the same API a plugin's web editor
// has inside a DAW (params, builtins, set/begin/end/edit, onChange, t,
// language), over the activity's `ferritronAndroid` interface. Loaded
// before the page's own scripts.
//
// Not on Android: ferritron.instance (null) and messages with the plugin.
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
