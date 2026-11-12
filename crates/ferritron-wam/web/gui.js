// The WAM's editor. A plugin with a web editor ([view] backend = "web") gets
// that editor, unchanged: its folder is copied to gui/ and the page sees the
// same `window.ferritron` as inside a DAW (params, builtins, set/begin/end/
// edit, onChange, t, language). Other plugins get an editor made from their
// parameters.
//
// Not available in the browser: ferritron.instance (null) and messages with
// the plugin (`send` is ignored): the instance runs in the AudioWorklet.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

const REFRESH_MS = 50;

/** Current values of the node's parameters, by key: { value, normalized, text }. */
async function snapshot(wam) {
  const node = wam.audioNode;
  const { values } = await node.call("get");
  const byId = new Map(values);
  const out = {};
  for (const p of node.params) {
    const normalized = byId.get(p.id) ?? 0;
    out[p.key] = { value: wam.meta.toPlain(p.id, normalized), normalized, text: wam.meta.format(p.id, normalized) };
  }
  return out;
}

/** Describes a parameter as the web editor's bridge does. */
function bridgeParam(p, now) {
  const out = { id: p.id, name: p.name, unit: p.unit, kind: p.kind, min: p.min, max: p.max, ...now };
  if (p.choices) out.choices = p.choices;
  return out;
}

/** Sends one edit to the node: plain value in, normalized to the plugin. */
function edit(wam, id, plain) {
  const normalized = wam.meta.toNormalized(id, plain);
  wam.audioNode.call("set", { values: [[id, normalized]] });
}

/** Watches the node and reports changed parameters to `update(key, now)`. */
function watch(wam, update) {
  let last = {};
  let stopped = false;
  const tick = async () => {
    if (stopped) return;
    const now = await snapshot(wam).catch(() => null);
    if (now) {
      for (const [key, v] of Object.entries(now)) if (last[key]?.normalized !== v.normalized) update(key, v);
      last = now;
    }
    if (!stopped) setTimeout(tick, REFRESH_MS);
  };
  tick();
  return () => { stopped = true; };
}

/** The plugin's own editor in an iframe, with `window.ferritron`. */
async function webEditor(wam, url, html) {
  const now = await snapshot(wam);
  const own = {}, builtins = {};
  for (const p of wam.audioNode.params) (p.key.startsWith("ferritron.") ? builtins : own)[p.key] = bridgeParam(p, now[p.key]);
  const frame = document.createElement("iframe");
  frame.style.cssText = "border:0;display:block;width:100%;min-height:240px";
  frame.title = wam.name;
  const ids = Object.fromEntries(wam.audioNode.params.map((p) => [p.id, p]));
  // The page talks through window.ipc.postMessage, as in the native web view.
  const ipc = (message) => {
    const [op, id, value] = String(message).split(":");
    if (op === "set" && ids[id]) edit(wam, Number(id), Number(value));
  };
  const setup = `<base href="${url.href}"><script>
window.ipc = { postMessage: (m) => parent.__ferritronIpc_${wam.instanceId.replace(/\W/g, "_")}(m) };
(() => {
  const params = ${JSON.stringify(own)};
  const builtins = ${JSON.stringify(builtins)};
  const messages = ${JSON.stringify(wam.description.messages ?? {})};
  const listeners = [], instanceListeners = [], messageListeners = [];
  const send = (m) => window.ipc.postMessage(m);
  const id = (key) => { const p = params[key] ?? builtins[key]; if (!p) throw new Error('ferritron: unknown parameter "' + key + '"'); return p.id; };
  window.ferritron = {
    params, builtins, language: ${JSON.stringify(wam.description.language ?? "en")}, nativeContextMenu: true,
    plugin: ${JSON.stringify({ id: wam.description.id ?? "", name: wam.description.name ?? "", vendor: wam.description.vendor ?? "" })},
    t(key, args = {}) { const text = messages[key] ?? key; return text.replace(/\\{\\$(\\w+)\\}/g, (m, n) => (n in args ? String(args[n]) : m)); },
    onChange(fn) { listeners.push(fn); },
    instance: null,
    onInstanceChange(fn) { instanceListeners.push(fn); },
    send(message) {},
    onMessage(fn) { messageListeners.push(fn); },
    begin(key) { send("begin:" + id(key)); },
    set(key, value) { send("set:" + id(key) + ":" + Number(value)); },
    end(key) { send("end:" + id(key)); },
    edit(key, value) { this.begin(key); this.set(key, value); this.end(key); },
    _update(key, value, normalized, text) { const p = params[key] ?? builtins[key]; Object.assign(p, { value, normalized, text }); for (const fn of listeners) fn(key, p); },
  };
})();
</script><script src="${new URL("extras.js", import.meta.url).href}"></script>`;
  window[`__ferritronIpc_${wam.instanceId.replace(/\W/g, "_")}`] = ipc;
  frame.srcdoc = /<head[^>]*>/i.test(html) ? html.replace(/<head[^>]*>/i, (h) => h + setup) : setup + html;
  frame.addEventListener("load", () => {
    const doc = frame.contentDocument?.documentElement;
    if (doc) frame.style.height = `${Math.max(240, doc.scrollHeight)}px`;
  });
  frame.__ferritronStop = watch(wam, (key, v) => frame.contentWindow?.ferritron?._update(key, v.value, v.normalized, v.text));
  return frame;
}

/** An editor made from the parameters: a slider, switch or list for each. */
async function generatedEditor(wam) {
  const now = await snapshot(wam);
  const root = document.createElement("div");
  root.className = "ferritron-wam";
  root.style.cssText = "font:13px system-ui,sans-serif;padding:12px;display:grid;grid-template-columns:auto 1fr auto;gap:8px 12px;align-items:center;min-width:280px";
  const title = document.createElement("div");
  title.textContent = wam.name;
  title.style.cssText = "grid-column:1/-1;font-weight:600;font-size:15px";
  root.append(title);
  const outputs = {};
  for (const p of wam.audioNode.params) {
    if (p.flags & 8) continue; // hidden
    const label = document.createElement("label");
    label.textContent = p.name;
    let input;
    if (p.kind === "enum") {
      input = document.createElement("select");
      p.choices.forEach((c, i) => input.add(new Option(c, i)));
      input.value = String(Math.round(now[p.key].value));
      input.onchange = () => edit(wam, p.id, Number(input.value));
    } else if (p.kind === "bool") {
      input = document.createElement("input");
      input.type = "checkbox";
      input.checked = now[p.key].value >= 0.5;
      input.onchange = () => edit(wam, p.id, input.checked ? 1 : 0);
    } else {
      input = document.createElement("input");
      input.type = "range";
      Object.assign(input, { min: p.min, max: p.max, step: p.kind === "int" ? 1 : (p.max - p.min) / 1000 });
      input.value = now[p.key].value;
      input.oninput = () => edit(wam, p.id, Number(input.value));
    }
    input.setAttribute("aria-label", p.name);
    const out = document.createElement("output");
    out.textContent = `${now[p.key].text} ${p.unit}`.trim();
    outputs[p.key] = { input, out, p };
    root.append(label, input, out);
  }
  root.__ferritronStop = watch(wam, (key, v) => {
    const o = outputs[key];
    if (!o) return;
    o.out.textContent = `${v.text} ${o.p.unit}`.trim();
    if (document.activeElement === o.input) return;
    if (o.input.type === "checkbox") o.input.checked = v.value >= 0.5;
    else o.input.value = String(o.p.kind === "enum" ? Math.round(v.value) : v.value);
  });
  return root;
}

export async function createGui(wam, url) {
  const page = await fetch(new URL("index.html", url)).catch(() => null);
  if (page?.ok) return webEditor(wam, url, await page.text());
  return generatedEditor(wam);
}

export function destroyGui(gui) {
  gui?.__ferritronStop?.();
  gui?.remove();
}
