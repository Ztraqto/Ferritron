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
        id: param_id(key),
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
    /// Every parameter the plugin shows hosts, in the order of the C ABI.
    pub fn host_params(&self) -> Vec<HostParam> {
        let mut out: Vec<HostParam> = self
            .params
            .iter()
            .map(|p| HostParam {
                index: 0,
                id: param_id(&p.key),
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
        scope.flag("isolated", self.isolate);
        scope.flag("ara", self.ara);
        let params = self.host_params();
        for n in 0..16 {
            scope.text(&format!("after.{n}"), (params.len() + n).to_string());
        }
        scope.text("params.count", params.len().to_string());
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
