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
//! ```
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
  const send = (m) => window.ipc.postMessage(m);
  const id = (key) => {
    const p = params[key] ?? builtins[key];
    if (!p) throw new Error(`ferritron: unknown parameter "${key}"`);
    return p.id;
  };
  window.ferritron = {
    params,
    builtins,
    language: __LANGUAGE__,
    nativeContextMenu: false,
    t(key, args = {}) {
      const text = messages[key] ?? key;
      return text.replace(/\{\$(\w+)\}/g, (m, name) => (name in args ? String(args[name]) : m));
    },
    onChange(fn) { listeners.push(fn); },
    instance: __INSTANCE__,
    onInstanceChange(fn) { instanceListeners.push(fn); },
    send(message) { send(`msg:${JSON.stringify(message)}`); },
    onMessage(fn) { messageListeners.push(fn); },
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
    fn json_strings_are_escaped() {
        let input = format!("a\"b{}c{}</script>", '\\', '\n');
        let escaped = json_str(&input);
        // Quotes and backslashes are escaped; no raw newline or `<` survives.
        assert!(escaped.starts_with(r#""a\"b\\c"#), "{escaped}");
        assert!(!escaped.contains('<') && !escaped.contains('\n'), "{escaped}");
        assert_eq!(json_num(f64::NAN), "0");
    }
}
