//! Web view backend: shows a plugin's HTML/JS user interface inside the host's
//! window (WebView2 on Windows, WKWebView on macOS, through `wry`).
//!
//! The UI files are embedded in the plugin binary and served from a custom
//! `ferritron` protocol (no localhost server). Pages talk to the plugin with
//! the `window.ferritron` object injected before any page script runs:
//!
//! ```js
//! ferritron.params.gain        // { id, name, unit, kind, min, max, choices, value, normalized, text }
//! ferritron.builtins["ferritron.quality"]   // Ferritron's switches, same shape: "ferritron.bypass",
//!                                           // and "ferritron.quality" with `[quality]`
//! ferritron.edit("ferritron.quality", 2)    // 0 Auto, 1 Normal, 2 Best
//! ferritron.onChange((key, param) => { ... })   // value changed (UI, automation or host)
//! ferritron.begin("gain"); ferritron.set("gain", -6); ferritron.end("gain");   // a drag gesture
//! ferritron.edit("gain", 0)    // begin + set + end, for one-shot changes
//! ferritron.t("ui-title")      // a message from locales/<lang>/*.ftl, in the user's language
//! ferritron.t("ui-voices", { count: 4 })   // `ui-voices = { $count } voices`
//! ferritron.language           // e.g. "ja"
//! ferritron.nativeContextMenu = true   // bring back the browser's right-click menu
//! ferritron.instance           // { id, track: { name, color: [r, g, b], index } } (null when unknown)
//! ferritron.onInstanceChange((instance) => { ... })   // the track was renamed, recolored, ...
//! ferritron.send({ kind: "rename", name: "Lead" })   // to the plugin (InstanceHandle::on_message)
//! ferritron.onMessage((message) => { ... })           // from the plugin (InstanceHandle::post)
//! ferritron.onStream("spectrum", (values) => { ... }) // a Float32Array: the newest values of a
//!                                                     // `[streams]` entry (at most every 33 ms)
//! ferritron.plugin             // { id, name, vendor }
//! ferritron.theme              // { dark, highContrast, reducedMotion, accent: [r, g, b] | null }
//! ferritron.onThemeChange((theme) => { ... })         // the system theme changed
//! ferritron.settings.get("zoom", 1); ferritron.settings.set("zoom", 1.5)   // kept across sessions
//! ferritron.settings.onChange((key, value) => { ... })  // also when another editor of the plugin sets it
//! await ferritron.contextMenu([{ id: "reset", label: "Reset" }, { separator: true }], { x, y })   // id or null
//! await ferritron.openFile({ title, dir, filters: [{ name: "Audio", extensions: ["wav", "aif"] }] })   // path or null
//! await ferritron.saveFile({ title, dir, name: "preset.json", filters: [...] })                       // path or null
//! ```
//!
//! Pointer and wheel events are the browser's own and carry what the plugin needs: `shiftKey`,
//! `ctrlKey`, `altKey`, `metaKey` on every pointer event (E1), `deltaX` / `deltaY` / `deltaMode`
//! on `wheel` (E8; trackpads give pixel deltas), pointer capture (`setPointerCapture`) keeps a
//! drag going outside the window and `requestPointerLock()` gives relative movement (E10), and
//! `prefers-color-scheme` follows the system theme (E7). The editor cannot ask the host to
//! resize its window yet (`ViewHost` has no such call), and has no host context menu or
//! full screen: `ferritron.contextMenu` draws a menu of its own.
//!
//! Messages are JSON in both directions. The plugin receives them on the UI
//! thread in the handler it set with `InstanceHandle::on_message` (usually in
//! `Process::init`) and answers, or sends at any time, with
//! `InstanceHandle::post`.
//!
//! Values passed to `set` are plain (e.g. dB). Every edit is reported to the
//! host, so DAWs can record it as automation.
//!
//! A right click shows no browser menu (reload, inspect, ...). An editor that
//! wants its own menu listens to `contextmenu` and draws it; the event still
//! arrives, only the browser's default is suppressed.
//!
//! Opening an editor is fast because the browser behind it is started ahead
//! of time: on Windows, WebView2's browser process starts (without blocking
//! the host) when the first plugin instance is created and stays while
//! instances exist, so editors open in about 60 ms instead of 300 ms or more.
//! `[view] preload = false` starts it only when an editor opens.

// Linux has no web view yet: the shared code is unused there.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code, clippy::extra_unused_type_parameters))]

use ferritron_core::__private::PluginMeta;
use ferritron_core::abi::ViewApi;
#[cfg(not(any(windows, target_os = "macos")))]
use ferritron_core::abi::ViewHost;
use ferritron_core::{ParamInfo, ParamKind, Process};
#[cfg(not(any(windows, target_os = "macos")))]
use std::ffi::c_void;

/// The embedded UI files: `(path relative to the UI folder, contents)`.
/// Implemented by `#[ferritron::plugin]` when `[view]` is declared.
pub trait WebAssets {
    const ASSETS: &'static [(&'static str, &'static [u8])];
    /// `[view] background = "#rrggbb"`: painted until the page draws itself.
    const BACKGROUND: Option<[u8; 3]> = None;
    /// `[view] preload` (default true): start the browser behind the editor
    /// when an instance is created, not when the editor opens.
    const PRELOAD: bool = true;
}

/// Called by Ferritron when an instance of `P` is created (`true`) or
/// destroyed (`false`): with `[view] preload`, the browser behind editors is
/// started ahead of time and kept while instances exist. Runs on the thread
/// that creates instances; only the host's UI thread starts anything.
pub fn preload<P: WebAssets>(created: bool) {
    #[cfg(any(windows, target_os = "macos"))]
    if P::PRELOAD {
        if created { imp::retain() } else { imp::release() }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = created;
}

/// The `ferritron.view.v1` table for plugin `P`: default size and resize limits
/// in logical pixels.
pub const fn view_api<P: Process + PluginMeta + WebAssets>(size: (u32, u32), min: (u32, u32), max: (u32, u32)) -> ViewApi {
    ViewApi {
        width: size.0,
        height: size.1,
        min_width: min.0,
        min_height: min.1,
        max_width: max.0,
        max_height: max.1,
        open: open::<P>,
        close,
        set_size,
    }
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"), // never close a <script> by accident
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_num(v: f64) -> String {
    if v.is_finite() { format!("{v}") } else { "0".into() }
}

/// `ferritron.instance`: the instance's identity and track, or `null`.
fn instance_json(instance: Option<&ferritron_core::InstanceHandle>) -> String {
    let Some(instance) = instance else { return "null".into() };
    let track = instance.track();
    let name = track.name.as_deref().map_or_else(|| "null".into(), json_str);
    let color = track.color.map_or_else(|| "null".into(), |[r, g, b]| format!("[{r},{g},{b}]"));
    let index = track.index.map_or_else(|| "null".into(), |i| i.to_string());
    format!("{{\"id\":{},\"track\":{{\"name\":{name},\"color\":{color},\"index\":{index}}}}}", json_str(&instance.id().to_string()))
}

/// What the UI needs to show one parameter in its current state.
fn param_json<P: Process + PluginMeta>(index: usize, info: &ParamInfo, normalized: f64) -> String {
    let localized = P::localized();
    let plain = info.denormalize(normalized);
    let (kind, min, max, choices) = match info.kind {
        ParamKind::Float { min, max } => ("float", min, max, String::new()),
        ParamKind::Int { min, max } => ("int", min as f64, max as f64, String::new()),
        ParamKind::Bool => ("bool", 0.0, 1.0, String::new()),
        ParamKind::Enum { choices } => {
            let names: Vec<String> = (0..choices.len()).map(|n| json_str(&localized.format(index, info, n as f64))).collect();
            ("enum", 0.0, (choices.len().max(1) - 1) as f64, format!(",\"choices\":[{}]", names.join(",")))
        }
    };
    format!(
        "{{\"id\":{},\"name\":{},\"unit\":{},\"kind\":\"{kind}\",\"min\":{},\"max\":{}{choices},\"value\":{},\"normalized\":{},\"text\":{}}}",
        info.id,
        json_str(localized.name_of(index, info)),
        json_str(info.unit),
        json_num(min),
        json_num(max),
        json_num(plain),
        json_num(normalized),
        json_str(&localized.format(index, info, plain)),
    )
}

const BRIDGE: &str = r#"
(() => {
  const params = __PARAMS__;
  const builtins = __BUILTINS__;
  const messages = __MESSAGES__;
  const listeners = [];
  const instanceListeners = [];
  const messageListeners = [];
  const streamListeners = {};
  const dialogs = new Map();
  let dialogId = 0;
  const send = (m) => window.ipc.postMessage(m);
  // Options of a file dialog as the host side reads them: percent-encoded fields, filters as
  // "Name|ext;ext~Name|ext".
  const dialog = (kind, o = {}) => new Promise((resolve) => {
    const n = ++dialogId;
    dialogs.set(n, resolve);
    const filters = (o.filters ?? []).map((f) => `${f.name ?? ""}|${(f.extensions ?? []).join(";")}`).join("~");
    const enc = (v) => encodeURIComponent(v ?? "");
    send(`dialog:${n}:${kind}:${enc(o.title)}:${enc(o.dir)}:${enc(filters)}:${enc(o.name)}`);
  });
  const id = (key) => {
    const p = params[key] ?? builtins[key];
    if (!p) throw new Error(`ferritron: unknown parameter "${key}"`);
    return p.id;
  };
  window.ferritron = {
    params,
    builtins,
    language: __LANGUAGE__,
    plugin: __PLUGIN__,
    nativeContextMenu: false,
    t(key, args = {}) {
      const text = messages[key] ?? key;
      return text.replace(/\{\$(\w+)\}/g, (m, name) => (name in args ? String(args[name]) : m));
    },
    onChange(fn) { listeners.push(fn); },
    instance: __INSTANCE__,
    onInstanceChange(fn) { instanceListeners.push(fn); },
    send(message) { send(`msg:${JSON.stringify(message)}`); },
    _post: send,
    onMessage(fn) { messageListeners.push(fn); },
    onStream(name, fn) { (streamListeners[name] ??= []).push(fn); },
    openFile(options) { return dialog("open", options); },
    saveFile(options) { return dialog("save", options); },
    _dialog(n, path) { const resolve = dialogs.get(n); dialogs.delete(n); if (resolve) resolve(path); },
    _stream(name, b64) {
      const bin = atob(b64);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      const values = new Float32Array(bytes.buffer);
      for (const fn of streamListeners[name] ?? []) fn(values);
    },
    _instance(info) {
      this.instance = info;
      for (const fn of instanceListeners) fn(info);
    },
    _message(text) {
      let message;
      try { message = JSON.parse(text); } catch { message = text; }
      for (const fn of messageListeners) fn(message);
    },
    begin(key) { send(`begin:${id(key)}`); },
    set(key, value) { send(`set:${id(key)}:${Number(value)}`); },
    end(key) { send(`end:${id(key)}`); },
    edit(key, value) { this.begin(key); this.set(key, value); this.end(key); },
    _update(key, value, normalized, text) {
      const p = params[key] ?? builtins[key];
      Object.assign(p, { value, normalized, text });
      for (const fn of listeners) fn(key, p);
    },
  };
  // No browser menu on right click. Captured first, so the page's own
  // `contextmenu` listeners still run (and can show a menu of their own).
  window.addEventListener("contextmenu", (e) => {
    if (!window.ferritron.nativeContextMenu) e.preventDefault();
  }, true);
})();
"#;

/// Theme, settings, a menu of our own, ...: the part of `window.ferritron` that needs nothing from
/// the host (also used by the WAM page and the Android app).
pub const EXTRAS: &str = include_str!("extras.js");

/// Standard base64 (with padding), for sending binary values to the page.
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The script that hands `values` of stream `name` to the page (`ferritron.onStream`).
fn stream_script(name: &str, values: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    format!("ferritron._stream({},\"{}\");", json_str(name), base64(&bytes))
}

mod dialog;
#[cfg(windows)]
mod environment;
#[cfg(any(windows, target_os = "macos"))]
mod imp;

#[cfg(any(windows, target_os = "macos"))]
use imp::{close, open, set_size};

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn open<P: Process + PluginMeta + WebAssets>(
    _parent: *mut c_void,
    _host: *const ViewHost,
    _width: u32,
    _height: u32,
    _scale: f64,
) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn close(_view: *mut c_void) {}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn set_size(_view: *mut c_void, _width: u32, _height: u32, _scale: f64) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_stream_script_carries_little_endian_floats() {
        // 1.0f32 = 00 00 80 3F
        assert_eq!(stream_script("spectrum", &[1.0]), "ferritron._stream(\"spectrum\",\"AACAPw==\");");
    }

    #[test]
    fn json_strings_are_escaped() {
        let input = format!("a\"b{}c{}</script>", '\\', '\n');
        let escaped = json_str(&input);
        // Quotes and backslashes are escaped; no raw newline or `<` survives.
        assert!(escaped.starts_with(r#""a\"b\\c"#), "{escaped}");
        assert!(!escaped.contains('<') && !escaped.contains('\n'), "{escaped}");
        assert_eq!(json_num(f64::NAN), "0");
    }
    #[test]
    fn extras_js_behaves_in_node() {
        // The browser-side logic (theme, settings, streams, dialogs without a host) with stand-ins
        // for the browser. Skipped (loudly) when node is not installed.
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("extras.test.js");
        match std::process::Command::new("node").arg(&script).output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(out.status.success(), "{text}
{}", String::from_utf8_lossy(&out.stderr));
                assert!(text.contains("all passed"), "{text}");
            }
            Err(e) => eprintln!("node is not available ({e}): extras.test.js was not run"),
        }
    }

    #[test]
    fn the_android_bridge_carries_the_same_extras() {
        let android = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferritron-standalone/android/bridge.js");
        let text = std::fs::read_to_string(&android).expect("the Android bridge");
        assert!(text.ends_with(EXTRAS), "crates/ferritron-standalone/android/bridge.js must end with extras.js");
        assert!(text.contains("plugin: {"), "the bridge names the plugin (settings are per plugin)");
    }

    #[test]
    fn the_wam_page_loads_extras_and_ships_them() {
        let wam = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferritron-wam");
        let gui = std::fs::read_to_string(wam.join("web/gui.js")).unwrap();
        let manifest = std::fs::read_to_string(wam.join("Cargo.toml")).unwrap();
        assert!(gui.contains("extras.js") && manifest.contains("ferritron-web/src/extras.js"));
        assert!(EXTRAS.contains("f.onStream") && !EXTRAS.contains('`'), "extras.js stays free of backticks (it is pasted into templates)");
    }

    #[test]
    fn the_bridge_names_the_dialog_message_the_host_side_reads() {
        assert!(BRIDGE.contains("dialog:${n}:${kind}") && BRIDGE.contains("_dialog(n, path)"));
        assert!(BRIDGE.contains("__PLUGIN__") && BRIDGE.contains("openFile") && BRIDGE.contains("saveFile"));
    }
}
