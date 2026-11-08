//! `ferritron.describe.v1`: the plugin described as JSON, for adapters that
//! show it by other means than the C ABI's numbers: a WAM in the browser,
//! the standalone apps on phones. Parameters carry their keys (stable, as in
//! the project file), kinds, ranges and localized names; `messages` are the
//! editor's translations (`ferritron.t`).
//!
//! ```json
//! { "id": "dev.ferritron.examples.gain", "name": "Ferritron Gain", "vendor": "Ztraqto",
//!   "version": "1.0.0", "kind": "effect", "language": "ja",
//!   "params": [ { "key": "gain", "id": 123, "name": "ゲイン", "unit": "dB", "kind": "float",
//!                 "min": -60, "max": 12, "default": 0, "steps": 0, "flags": 0 },
//!               { "key": "mode", ..., "kind": "enum", "choices": ["A", "B"] } ],
//!   "messages": { "ui-title": "..." } }
//! ```

use crate::__private::PluginMeta;
use crate::abi::{ParamInfoC, all_params};
use crate::{ParamKind, Process};

pub(crate) fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn number(v: f64) -> String {
    if v.is_finite() { format!("{v}") } else { "0".into() }
}

/// The description of plugin `P`.
pub(crate) fn json<P: Process + PluginMeta>() -> String {
    let localized = P::localized();
    let info = P::INFO;
    let mut params = Vec::new();
    for (index, p) in all_params::<P>() {
        let mut c = std::mem::MaybeUninit::<ParamInfoC>::uninit();
        // SAFETY: `index` is below param_count.
        let flags = if unsafe { crate::abi::param_info_of::<P>(index as u32, c.as_mut_ptr()) } { unsafe { c.assume_init() }.flags } else { 0 };
        let (kind, min, max, choices) = match p.kind {
            ParamKind::Float { min, max } => ("float", min, max, String::new()),
            ParamKind::Int { min, max } => ("int", min as f64, max as f64, String::new()),
            ParamKind::Bool => ("bool", 0.0, 1.0, String::new()),
            ParamKind::Enum { choices } => {
                let names: Vec<String> = (0..choices.len()).map(|n| string(&localized.format(index, p, n as f64))).collect();
                ("enum", 0.0, choices.len().saturating_sub(1) as f64, format!(",\"choices\":[{}]", names.join(",")))
            }
        };
        let name = if p.key == "ferritron.mcp" { p.name } else { localized.name_of(index, p) };
        params.push(format!(
            "{{\"key\":{},\"id\":{},\"name\":{},\"unit\":{},\"kind\":\"{kind}\",\"min\":{},\"max\":{},\"default\":{},\"steps\":{},\"flags\":{flags}{choices}}}",
            string(p.key),
            p.id,
            string(name),
            string(p.unit),
            number(min),
            number(max),
            number(p.default),
            p.step_count(),
        ));
    }
    let messages: Vec<String> = localized.ui_messages().iter().map(|(k, v)| format!("{}:{}", string(k), string(v))).collect();
    format!(
        "{{\"id\":{},\"name\":{},\"vendor\":{},\"version\":{},\"kind\":\"{}\",\"language\":{},\"params\":[{}],\"messages\":{{{}}}}}",
        string(info.id),
        string(info.name),
        string(info.vendor),
        string(info.version),
        if matches!(info.kind, crate::PluginKind::Instrument) { "instrument" } else { "effect" },
        string(&localized.language),
        params.join(","),
        messages.join(","),
    )
}
