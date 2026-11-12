//! What packaging needs from the project file: the parameters as hosts see
//! them, and the template language format crates write their generated
//! files in (an LV2 bundle's `.ttl`, a WAM's `descriptor.json`, an
//! Info.plist).
//!
//! # Templates
//!
//! - `{key}`: a value, escaped for the file it goes into.
//! - `{#key}...{/key}`: a list repeats the text once per item (inside it,
//!   the item's own keys come first); a flag shows the text when it is on;
//!   a value shows it when it is not empty.
//! - `{^key}...{/key}`: the text when the list is empty, the flag off or the
//!   value empty.
//!
//! A tag is `{`, an optional `#`, `^` or `/`, a name of lowercase letters,
//! digits, `.`, `-` and `_`, then `}`. Every other `{` is text, so JSON and
//! Turtle need no escaping. An unknown name is an error: a typo never ships
//! as an empty string.

use crate::{Kind, Manifest};
use std::collections::BTreeMap;

/// Stable parameter ID from its key: the same function as
/// `ferritron::__private::param_id` (pinned by a test in `ferritron-core`).
pub const fn param_id(key: &str) -> u32 {
    let bytes = key.as_bytes();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    ((hash ^ (hash >> 32)) as u32) & 0x7fff_ffff
}

/// Parameter IDs Ferritron keeps for itself start here (1.3.1). A plugin's own
/// parameters, hashed or given with `id = ...`, are always below it.
pub const RESERVED_BASE: u32 = 0x4000_0000;

/// ID of a parameter Ferritron adds itself (`ferritron.bypass`, ...): a fixed
/// value at or above [`RESERVED_BASE`]. Other keys get the hashed ID (1.3.0 IDs
/// of the reserved keys are returned by [`legacy_reserved_id`]).
pub const fn reserved_id(key: &str) -> u32 {
    const fn is(a: &str, b: &str) -> bool {
        let (a, b) = (a.as_bytes(), b.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
    if is(key, "ferritron.bypass") {
        RESERVED_BASE
    } else if is(key, "ferritron.quality") {
        RESERVED_BASE + 1
    } else if is(key, "ferritron.mcp") {
        RESERVED_BASE + 2
    } else if is(key, "ferritron.instance.high") {
        RESERVED_BASE + 3
    } else if is(key, "ferritron.instance.low") {
        RESERVED_BASE + 4
    } else if is(key, "ferritron.editor.width") {
        RESERVED_BASE + 5
    } else if is(key, "ferritron.editor.height") {
        RESERVED_BASE + 6
    } else {
        param_id(key)
    }
}

/// The VST3 class ID of a plugin that has no `[vst3] class_id`: derived from the plugin ID.
/// The same function as `ferritron::__private::class_id` (pinned by a test in `examples/vst3-ids`),
/// so packaging can name the class in moduleinfo.json without running the plugin.
pub const fn class_id(plugin_id: &str) -> [u8; 16] {
    const fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325 ^ seed;
        let mut i = 0;
        while i < bytes.len() {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
        hash
    }
    let hi = fnv1a64(plugin_id.as_bytes(), 0x6665_7272_6974_726f).to_be_bytes();
    let lo = fnv1a64(plugin_id.as_bytes(), 0x6e00_0000_0000_0001).to_be_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 8 {
        out[i] = hi[i];
        out[i + 8] = lo[i];
        i += 1;
    }
    out
}

/// The 16 bytes of a VST3 class ID that makes Windows hosts (`FUID::toString`, COM layout) show
/// the GUID exactly as it was written in the project file (an explicit `class_id`,
/// `controller_id` or `compatibility.id`). Elsewhere the written bytes are used as they are.
/// IDs derived from the plugin's `id` are not changed: they keep what 1.x shipped.
pub fn windows_tuid(written: &[u8; 16]) -> [u8; 16] {
    let mut b = *written;
    b[0..4].reverse();
    b[4..6].reverse();
    b[6..8].reverse();
    b
}

/// The 32 hex digits a VST3 host shows for a class ID (`FUID::toString`): on Windows the first
/// three fields of the 16 bytes are read as little endian numbers (COM layout, `com = true`),
/// elsewhere the bytes are printed as they are.
pub fn vst3_uid_string(bytes: &[u8; 16], com: bool) -> String {
    let mut b = *bytes;
    if com {
        b[0..4].reverse();
        b[4..6].reverse();
        b[6..8].reverse();
    }
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// The ID a reserved parameter had in Ferritron 1.3.0 (the hash of its key):
/// hosts that support VST3 `IRemapParamID` are told to map it to the new one.
pub const fn legacy_reserved_id(key: &str) -> u32 {
    param_id(key)
}

/// A port of an LV2 plugin after its parameter (control) ports, in port order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lv2Port {
    /// Main audio input, channel `n`.
    In(u32),
    /// Sidechain audio input, channel `n`.
    Sidechain(u32),
    /// Audio output, channel `n`.
    Out(u32),
    /// MIDI input (an atom sequence).
    Midi,
    /// Transport input (an atom sequence of `time:Position`).
    Time,
    Latency,
    Freewheel,
    /// MIDI output (an atom sequence the plugin writes `ctx.midi_out` to). Always the last port.
    MidiOut,
}

/// The ports an LV2 plugin has after its parameters. Both the Turtle
/// description and the adapter (`ferritron-lv2`) use this one list.
///
/// A plugin with nothing new (stereo or mono-to-stereo, no sidechain, no MIDI
/// input beyond instruments) keeps the 1.3.0 ports, plus the MIDI output as the
/// last port (added in 2.0.0, so the earlier ports keep their indices); one that
/// uses 1.3.1's audio features also gets a Time port.
pub fn lv2_port_plan(instrument: bool, min_channels: u32, max_channels: u32, sidechain: Option<u32>, midi_input: bool) -> Vec<Lv2Port> {
    let n = 2u32.clamp(min_channels.max(1), max_channels.max(min_channels).max(1));
    let extended = midi_input || sidechain.is_some() || n != 2;
    let mut plan = Vec::new();
    if !instrument {
        plan.extend((0..n).map(Lv2Port::In));
        if let Some(sc) = sidechain {
            let m = if sc == 0 { n } else { sc };
            plan.extend((0..m).map(Lv2Port::Sidechain));
        }
    }
    plan.extend((0..n).map(Lv2Port::Out));
    if instrument || midi_input {
        plan.push(Lv2Port::Midi);
    }
    if extended {
        plan.push(Lv2Port::Time);
    }
    plan.push(Lv2Port::Latency);
    plan.push(Lv2Port::Freewheel);
    plan.push(Lv2Port::MidiOut);
    plan
}

fn lv2_channel_name(prefix: &str, label: &str, channel: u32, count: u32) -> (String, String) {
    if count == 2 {
        let side = if channel == 0 { "l" } else { "r" };
        (format!("{prefix}_{side}"), format!("{label} {}", side.to_uppercase()))
    } else {
        (format!("{prefix}_{}", channel + 1), format!("{label} {}", channel + 1))
    }
}

impl Manifest {
    /// [`lv2_port_plan`] of this plugin.
    pub fn lv2_ports(&self) -> Vec<Lv2Port> {
        lv2_port_plan(self.instrument, self.audio.min_channels, self.audio.max_channels, self.audio.sidechain, self.midi_input)
    }
}

/// One parameter as hosts see it, in host order: the plugin's own, then
/// Bypass, then AI Access (`[mcp]`), then Quality (`[quality]`).
#[derive(Clone, Debug, PartialEq)]
pub struct HostParam {
    pub index: usize,
    pub id: u32,
    pub key: String,
    /// The key as a C identifier (`ferritron.bypass` -> `ferritron_bypass`),
    /// for formats that name parameters that way (LV2 port symbols).
    pub symbol: String,
    pub name: String,
    pub unit: String,
    pub kind: Kind,
    /// Plain value.
    pub default: f64,
    pub automatable: bool,
    pub hidden: bool,
    pub bypass: bool,
}

impl HostParam {
    /// The plain range: (min, max). Choices and switches count from 0.
    pub fn range(&self) -> (f64, f64) {
        match &self.kind {
            Kind::Float { min, max } => (*min, *max),
            Kind::Int { min, max } => (*min as f64, *max as f64),
            Kind::Bool => (0.0, 1.0),
            Kind::Enum { choices } => (0.0, choices.len().saturating_sub(1) as f64),
        }
    }

    /// Discrete steps (0 = continuous), as the C ABI reports them.
    pub fn steps(&self) -> u32 {
        match &self.kind {
            Kind::Float { .. } => 0,
            Kind::Int { min, max } => (max - min).max(0) as u32,
            Kind::Bool => 1,
            Kind::Enum { choices } => choices.len().saturating_sub(1) as u32,
        }
    }

    pub fn default_normalized(&self) -> f64 {
        let (min, max) = self.range();
        if max <= min { 0.0 } else { ((self.default - min) / (max - min)).clamp(0.0, 1.0) }
    }
}

fn builtin(key: &str, name: &str, kind: Kind, automatable: bool) -> HostParam {
    HostParam {
        index: 0,
        id: reserved_id(key),
        key: key.into(),
        symbol: String::new(),
        name: name.into(),
        unit: String::new(),
        kind,
        default: 0.0,
        automatable,
        hidden: false,
        bypass: key == "ferritron.bypass",
    }
}

impl Manifest {
    /// The class ID of the plugin's component: `[vst3] class_id`, else derived from `plugin.id`.
    pub fn vst3_class_bytes(&self) -> [u8; 16] {
        self.vst3_class_id.unwrap_or_else(|| class_id(&self.id))
    }

    /// Every parameter the plugin shows hosts, in the order of the C ABI.
    pub fn host_params(&self) -> Vec<HostParam> {
        let mut out: Vec<HostParam> = self
            .params
            .iter()
            .map(|p| HostParam {
                index: 0,
                id: p.id,
                key: p.key.clone(),
                symbol: String::new(),
                name: p.name.clone(),
                unit: p.unit.clone(),
                kind: p.kind.clone(),
                default: p.default,
                automatable: p.automatable,
                hidden: p.hidden,
                bypass: false,
            })
            .collect();
        out.push(builtin("ferritron.bypass", "Bypass", Kind::Bool, true));
        if self.mcp {
            out.push(builtin("ferritron.mcp", "AI Access", Kind::Bool, false));
        }
        if self.quality {
            let choices = ["Auto", "Normal", "Best"].map(String::from).to_vec();
            out.push(builtin("ferritron.quality", "Quality", Kind::Enum { choices }, false));
        }
        for (i, p) in out.iter_mut().enumerate() {
            p.index = i;
            p.symbol = p.key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        }
        out
    }

    /// Everything packaging templates can use: [`Manifest::template_values`]
    /// as values, flags (`instrument`, `effect`, `midi-input`, `view`,
    /// `isolated`, `ara`), the list `params` and `{after.N}` (the parameter
    /// count plus N, for indices that follow the parameters).
    ///
    /// Each item of `params`: `index`, `id`, `key`, `symbol`, `name`, `unit`,
    /// `kind` (float, int, bool, enum), `min`, `max`, `default`,
    /// `default.normalized`, `steps`, the flags `float`, `int`, `bool`,
    /// `enum`, `bypass`, `automatable`, `hidden`, `last`, and the list
    /// `choices` (each: `index`, `name`, `last`).
    pub fn template_scope(&self) -> Scope {
        let mut scope = Scope::default();
        for (k, v) in self.template_values() {
            scope.text(&k, v);
        }
        scope.flag("instrument", self.instrument);
        scope.flag("effect", !self.instrument);
        scope.flag("midi-input", self.instrument || self.au.kind == "aumf");
        scope.flag("view", self.view);
        // VST3 classes (moduleinfo.json). `.com` is the text a Windows host shows for the bytes,
        // `.plain` the one other systems show.
        for (name, id, explicit) in [
            ("class-id", Some(self.vst3_class_bytes()), self.vst3_class_id.is_some()),
            ("controller-id", self.vst3_controller_id, true),
            ("compat-id", self.vst3_compat.as_ref().map(|c| c.id), true),
        ] {
            let id = id.unwrap_or([0; 16]);
            // On Windows an explicit GUID is stored in COM order, so the host shows it as written.
            let on_windows = if explicit { windows_tuid(&id) } else { id };
            scope.text(&format!("vst3.{name}.com"), vst3_uid_string(&on_windows, true));
            scope.text(&format!("vst3.{name}.plain"), vst3_uid_string(&id, false));
        }
        scope.flag("vst3.controller", self.vst3_controller_id.is_some());
        scope.flag("vst3.compat", self.vst3_compat.is_some());
        let replaces = self.vst3_compat.as_ref().map(|c| c.replaces.clone()).unwrap_or_default();
        scope.flag("vst3.has-replaces", !replaces.is_empty());
        let count = replaces.len();
        scope.list(
            "vst3.replaces",
            replaces
                .iter()
                .enumerate()
                .map(|(i, old)| {
                    let mut item = Scope::default();
                    // As the old plug-in's own moduleinfo.json wrote it: the digits in the order written.
                    item.text("uid", vst3_uid_string(old, false));
                    item.flag("last", i + 1 == count);
                    item
                })
                .collect(),
        );
        let subs: Vec<String> = self.vst3_sub_categories().split('|').filter(|c| !c.is_empty()).map(str::to_string).collect();
        let sub_count = subs.len();
        scope.list(
            "vst3.subcategories",
            subs.into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let mut item = Scope::default();
                    item.text("name", name);
                    item.flag("last", i + 1 == sub_count);
                    item
                })
                .collect(),
        );
        scope.text("vst3.sdk-version", "VST 3.8.0".into());
        scope.flag("isolated", self.isolate);
        scope.flag("ara", self.ara);
        let params = self.host_params();
        for n in 0..16 {
            scope.text(&format!("after.{n}"), (params.len() + n).to_string());
        }
        scope.text("params.count", params.len().to_string());
        let plan = self.lv2_ports();
        let main = plan.iter().filter(|p| matches!(p, Lv2Port::Out(_))).count() as u32;
        let side = plan.iter().filter(|p| matches!(p, Lv2Port::Sidechain(_))).count() as u32;
        let first = params.len();
        let lv2ports = plan
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut item = Scope::default();
                item.text("index", (first + i).to_string());
                let (symbol, name) = match *p {
                    Lv2Port::In(c) => lv2_channel_name("in", "In", c, main),
                    Lv2Port::Sidechain(c) => lv2_channel_name("sc", "Sidechain", c, side),
                    Lv2Port::Out(c) => lv2_channel_name("out", "Out", c, main),
                    Lv2Port::Midi => ("midi_in".into(), "MIDI In".into()),
                    Lv2Port::Time => ("time_in".into(), "Transport".into()),
                    Lv2Port::Latency => ("latency".into(), "Latency".into()),
                    Lv2Port::Freewheel => ("freewheel".into(), "Freewheel".into()),
                    Lv2Port::MidiOut => ("midi_out".into(), "MIDI Out".into()),
                };
                item.text("symbol", symbol);
                item.text("name", name);
                item.flag("input", matches!(p, Lv2Port::In(_) | Lv2Port::Sidechain(_) | Lv2Port::Midi | Lv2Port::Time | Lv2Port::Freewheel));
                item.flag("audio", matches!(p, Lv2Port::In(_) | Lv2Port::Sidechain(_) | Lv2Port::Out(_)));
                // An atom port: MIDI input (`midi_in`) or MIDI output.
                item.flag("midi", matches!(p, Lv2Port::Midi | Lv2Port::MidiOut));
                item.flag("midi_in", *p == Lv2Port::Midi);
                item.flag("time", *p == Lv2Port::Time);
                item.flag("latency", *p == Lv2Port::Latency);
                item.flag("freewheel", *p == Lv2Port::Freewheel);
                item.flag("last", i + 1 == plan.len());
                item
            })
            .collect();
        scope.list("lv2ports", lv2ports);
        let count = params.len();
        let items = params
            .into_iter()
            .map(|p| {
                let mut item = Scope::default();
                let (min, max) = p.range();
                item.text("index", p.index.to_string());
                item.text("id", p.id.to_string());
                item.text("key", p.key.clone());
                item.text("symbol", p.symbol.clone());
                item.text("name", p.name.clone());
                item.text("unit", p.unit.clone());
                item.text("min", number(min));
                item.text("max", number(max));
                item.text("default", number(p.default));
                item.text("default.normalized", number(p.default_normalized()));
                item.text("steps", p.steps().to_string());
                let kind = match &p.kind {
                    Kind::Float { .. } => "float",
                    Kind::Int { .. } => "int",
                    Kind::Bool => "bool",
                    Kind::Enum { .. } => "enum",
                };
                item.text("kind", kind.into());
                for k in ["float", "int", "bool", "enum"] {
                    item.flag(k, k == kind);
                }
                item.flag("bypass", p.bypass);
                item.flag("automatable", p.automatable);
                item.flag("hidden", p.hidden);
                item.flag("last", p.index + 1 == count);
                let choices = match &p.kind {
                    Kind::Enum { choices } => choices
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let mut choice = Scope::default();
                            choice.text("index", i.to_string());
                            choice.text("name", c.clone());
                            choice.flag("last", i + 1 == choices.len());
                            choice
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                item.list("choices", choices);
                item
            })
            .collect();
        scope.list("params", items);
        scope
    }
}

/// A number as templates print it: integers without a fraction.
fn number(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Flag(bool),
    List(Vec<Scope>),
}

/// Named values for a template.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scope(BTreeMap<String, Value>);

impl Scope {
    pub fn text(&mut self, key: &str, value: String) -> &mut Self {
        self.0.insert(key.into(), Value::Text(value));
        self
    }

    pub fn flag(&mut self, key: &str, on: bool) -> &mut Self {
        self.0.insert(key.into(), Value::Flag(on));
        self
    }

    pub fn list(&mut self, key: &str, items: Vec<Scope>) -> &mut Self {
        self.0.insert(key.into(), Value::List(items));
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }
}

/// How values are escaped, by the kind of file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    /// XML and HTML (`.plist`, `.xml`, `.html`).
    Xml,
    /// Inside a double-quoted string of JSON, JavaScript or Turtle.
    Quoted,
    None,
}

impl Escape {
    /// The escaping for a file name.
    pub fn for_file(name: &str) -> Self {
        match name.rsplit('.').next().unwrap_or("") {
            "plist" | "xml" | "html" | "htm" | "svg" => Self::Xml,
            "json" | "js" | "mjs" | "ttl" => Self::Quoted,
            _ => Self::None,
        }
    }

    fn apply(self, text: &str, out: &mut String) {
        for c in text.chars() {
            match (self, c) {
                (Self::Xml, '&') => out.push_str("&amp;"),
                (Self::Xml, '<') => out.push_str("&lt;"),
                (Self::Xml, '>') => out.push_str("&gt;"),
                (Self::Xml, '"') => out.push_str("&quot;"),
                (Self::Quoted, '"') => out.push_str("\\\""),
                (Self::Quoted, '\\') => out.push_str("\\\\"),
                (Self::Quoted, '\n') => out.push_str("\\n"),
                (Self::Quoted, '\r') => out.push_str("\\r"),
                (Self::Quoted, '\t') => out.push_str("\\t"),
                (Self::Quoted, '<') => out.push_str("\\u003c"),
                (Self::Quoted, c) if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                (_, c) => out.push(c),
            }
        }
    }
}

/// A tag at the start of `text`: (sigil, name, length).
fn tag(text: &str) -> Option<(Option<char>, &str, usize)> {
    let rest = text.strip_prefix('{')?;
    let (sigil, body) = match rest.chars().next()? {
        c @ ('#' | '^' | '/') => (Some(c), &rest[1..]),
        _ => (None, rest),
    };
    let end = body.find('}')?;
    let name = &body[..end];
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
    valid.then(|| (sigil, name, 1 + sigil.is_some() as usize + end + 1))
}

/// Fills a template (see the module documentation).
pub fn render(template: &str, scope: &Scope, escape: Escape) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    render_into(template, &[scope], escape, &mut out)?;
    Ok(out)
}

fn lookup<'a>(stack: &[&'a Scope], name: &str) -> Option<&'a Value> {
    stack.iter().rev().find_map(|s| s.get(name))
}

/// The end of the section `name` that starts at `text` (after its opening tag): (body length, closing tag length).
fn section_end(text: &str, name: &str) -> Result<(usize, usize), String> {
    let mut depth = 0;
    let mut at = 0;
    while let Some(off) = text[at..].find('{') {
        let i = at + off;
        if let Some((sigil, n, len)) = tag(&text[i..])
            && n == name
        {
            match sigil {
                Some('#' | '^') => depth += 1,
                Some('/') if depth == 0 => return Ok((i, len)),
                Some('/') => depth -= 1,
                _ => {}
            }
        }
        at = i + 1;
    }
    Err(format!("{{#{name}}} has no {{/{name}}}"))
}

fn render_into(template: &str, stack: &[&Scope], escape: Escape, out: &mut String) -> Result<(), String> {
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let Some((sigil, name, len)) = tag(&rest[i..]) else {
            out.push('{');
            rest = &rest[i + 1..];
            continue;
        };
        let after = &rest[i + len..];
        match sigil {
            None => match lookup(stack, name) {
                Some(Value::Text(v)) => escape.apply(v, out),
                Some(Value::Flag(v)) => out.push_str(if *v { "true" } else { "false" }),
                Some(Value::List(l)) => out.push_str(&l.len().to_string()),
                None => return Err(format!("unknown placeholder {{{name}}}")),
            },
            Some('/') => return Err(format!("{{/{name}}} without {{#{name}}}")),
            Some(s) => {
                let (body_len, close_len) = section_end(after, name)?;
                let body = &after[..body_len];
                let value = lookup(stack, name).ok_or_else(|| format!("unknown section {{{s}{name}}}"))?;
                let shown = match value {
                    Value::Text(v) => !v.is_empty(),
                    Value::Flag(v) => *v,
                    Value::List(l) => !l.is_empty(),
                };
                if s == '^' {
                    if !shown {
                        render_into(body, stack, escape, out)?;
                    }
                } else if let Value::List(items) = value {
                    for item in items {
                        let mut inner = stack.to_vec();
                        inner.push(item);
                        render_into(body, &inner, escape, out)?;
                    }
                } else if shown {
                    render_into(body, stack, escape, out)?;
                }
                rest = &after[body_len + close_len..];
                continue;
            }
        }
        rest = after;
    }
    out.push_str(rest);
    Ok(())
}

#[cfg(test)]
mod vst3_tests {
    use super::*;

    #[test]
    fn host_strings_of_a_class_id() {
        let bytes = crate::parse_guid("3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11").unwrap();
        assert_eq!(vst3_uid_string(&bytes, false), "3E6B2A904C174D619D5B1F7A0C2E8B11");
        // FUID::toString on Windows: Data1, Data2 and Data3 are little endian in memory
        assert_eq!(vst3_uid_string(&bytes, true), "902A6B3E174C614D9D5B1F7A0C2E8B11");
    }

    #[test]
    fn the_derived_class_id_is_stable() {
        // These bytes are in users' projects (hosts store the class ID): never change them.
        assert_eq!(class_id("dev.ferritron.examples.gain"), class_id("dev.ferritron.examples.gain"));
        assert_ne!(class_id("a.b"), class_id("a.c"));
    }

    #[test]
    fn moduleinfo_templates_see_the_classes() {
        let m = crate::parse(
            "[plugin]\nid = \"a.b\"\nname = \"X\"\ncategories = [\"EQ\"]\n[vst3]\nclass_id = \"3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11\"\ncontroller_id = \"A1C4F7E2-6B38-4C90-8F2D-5E7B3A91C640\"\ncompatibility = { id = \"0D9E5B70-2A46-4F83-B1C7-8E4A6F2D9035\", replaces = [\"11111111-2222-3333-4444-555555555555\", \"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\"] }",
        )
        .unwrap();
        let scope = m.template_scope();
        let t = "{vst3.class-id.plain}|{vst3.controller-id.com}|{#vst3.controller}C{/vst3.controller}{^vst3.compat}-{/vst3.compat}|{#vst3.replaces}{uid}{^last},{/last}{/vst3.replaces}|{#vst3.subcategories}{name}{^last}/{/last}{/vst3.subcategories}";
        assert_eq!(
            render(t, &scope, Escape::None).unwrap(),
            "3E6B2A904C174D619D5B1F7A0C2E8B11|A1C4F7E26B384C908F2D5E7B3A91C640|C|11111111222233334444555555555555,AAAAAAAABBBBCCCCDDDDEEEEEEEEEEEE|Fx/EQ"
        );
        let plain = crate::parse("[plugin]\nid = \"a.b\"\nname = \"X\"").unwrap();
        let none = render("{#vst3.controller}C{/vst3.controller}{^vst3.has-replaces}n{/vst3.has-replaces}", &plain.template_scope(), Escape::None).unwrap();
        assert_eq!(none, "n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "[plugin]\nid = \"com.example.x\"\nname = \"X\"\nvendor = \"Z\"\n[params]\n\
                       gain = { type = \"float\", range = [-60, 12], default = 0, unit = \"dB\" }\n\
                       mode = { type = \"enum\", choices = [\"A\", \"B\"], default = \"B\" }\n[quality]\n";

    #[test]
    fn host_params_follow_the_abi_order() {
        let m = crate::parse(SRC).unwrap();
        let p = m.host_params();
        let keys: Vec<&str> = p.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["gain", "mode", "ferritron.bypass", "ferritron.quality"]);
        assert!(p[2].bypass && p[2].automatable && !p[3].automatable);
        assert_eq!(p[2].symbol, "ferritron_bypass");
        assert_eq!(p[1].default, 1.0);
        assert_eq!(p[1].steps(), 1);
        assert!((p[0].default_normalized() - 60.0 / 72.0).abs() < 1e-12);
    }

    #[test]
    fn templates_fill_values_lists_and_flags() {
        let m = crate::parse(SRC).unwrap();
        let scope = m.template_scope();
        let t = "{name}:{#params}[{index}={symbol}{#enum}({#choices}{name}{^last},{/last}{/choices}){/enum}]{/params} {after.2}{#instrument}I{/instrument}{^instrument}E{/instrument}";
        assert_eq!(render(t, &scope, Escape::None).unwrap(), "X:[0=gain][1=mode(A,B)][2=ferritron_bypass][3=ferritron_quality(Auto,Normal,Best)] 6E");
        // JSON braces and Turtle stay text.
        assert_eq!(render("{\"a\": {name}} {not a tag}", &scope, Escape::Quoted).unwrap(), "{\"a\": X} {not a tag}");
        assert!(render("{nope}", &scope, Escape::None).is_err());
        assert!(render("{#params}x", &scope, Escape::None).is_err());
        let mut s = Scope::default();
        s.text("v", "a\"<b>".into());
        assert_eq!(render("{v}", &s, Escape::Xml).unwrap(), "a&quot;&lt;b&gt;");
        assert_eq!(render("{v}", &s, Escape::Quoted).unwrap(), "a\\\"\\u003cb>");
    }
}
