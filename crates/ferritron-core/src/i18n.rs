//! Localization of host-visible strings (design doc, chapter 11).
//!
//! Translations are Fluent files. The plugin's own files are embedded at
//! build time (`locales/<lang>/*.ftl`); users and communities can add or
//! override languages with packs in
//! `<data dir>/Ferritron/locales/<plugin id>/<lang>.ftl`.
//!
//! Keys: `param-<key>` names a parameter, `param-<key>-<index>` names a
//! choice of an enum parameter.
//!
//! The display language is the first of: `FERRITRON_LANGUAGE`, the per-plugin
//! setting, the global setting (`<data dir>/Ferritron/settings.toml`), the OS
//! language. Missing strings fall back along `ja-JP` -> `ja` -> `en` -> the
//! name in the project file (`.ferritroject`).

use crate::{ParamInfo, ParamKind};
use fluent_syntax::ast::{Entry, Expression, InlineExpression, PatternElement};
use std::collections::HashMap;
use std::path::PathBuf;

/// Strings Ferritron itself shows, used when the plugin has none.
const BUILTIN: &[(&str, &str)] = &[
    (
        "en",
        "ferritron-on = On\nferritron-off = Off\nferritron-bypass = Bypass\n\
         ferritron-quality = Quality\nferritron-quality-0 = Auto\nferritron-quality-1 = Normal\nferritron-quality-2 = Best\n",
    ),
    (
        "ja",
        "ferritron-on = オン\nferritron-off = オフ\nferritron-bypass = バイパス\n\
         ferritron-quality = 音質\nferritron-quality-0 = 自動\nferritron-quality-1 = 標準\nferritron-quality-2 = 最高\n",
    ),
];

/// Key of the built-in "Quality" switch (`[quality]`).
pub(crate) const QUALITY_KEY: &str = "ferritron.quality";

/// Host-visible strings of one plugin in the chosen language.
pub struct Localized {
    pub language: String,
    names: Vec<&'static str>,
    choices: Vec<Vec<&'static str>>,
    on: &'static str,
    off: &'static str,
    bypass: &'static str,
    quality: &'static str,
    quality_choices: [&'static str; 3],
    /// Every other message, for editors (`ferritron.t()` in web views).
    ui: Vec<(String, String)>,
}

impl Localized {
    /// `embedded` pairs a language code with the concatenated `.ftl` sources for it.
    pub fn new(plugin_id: &str, infos: &'static [ParamInfo], embedded: &[(&str, &str)]) -> Self {
        let language = preferred_language(plugin_id).unwrap_or_else(|| "en".into());
        Self::with_language(&language, plugin_id, infos, embedded)
    }

    fn with_language(language: &str, plugin_id: &str, infos: &'static [ParamInfo], embedded: &[(&str, &str)]) -> Self {
        let mut table: HashMap<String, String> = HashMap::new();
        for lang in fallback_chain(language) {
            // Earlier sources win: the user's pack, then the plugin, then Ferritron.
            let pack = pack_path(plugin_id, &lang).and_then(|p| std::fs::read_to_string(p).ok());
            let own = embedded.iter().filter(|(l, _)| canonical(l) == lang).map(|(_, s)| s.to_string());
            let builtin = BUILTIN.iter().filter(|(l, _)| *l == lang).map(|(_, s)| s.to_string());
            for source in pack.into_iter().chain(own).chain(builtin) {
                for (key, text) in messages(&source) {
                    table.entry(key).or_insert(text);
                }
            }
        }
        let mut take = |key: String, fallback: &'static str| -> &'static str {
            table.remove(&key).map_or(fallback, |s| Box::leak(s.into_boxed_str()))
        };
        let names = infos.iter().map(|i| take(format!("param-{}", i.key), i.name)).collect();
        let choices = infos
            .iter()
            .map(|i| match i.kind {
                ParamKind::Enum { choices } => {
                    choices.iter().enumerate().map(|(n, c)| take(format!("param-{}-{n}", i.key), c)).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        let on = take("ferritron-on".into(), "On");
        let off = take("ferritron-off".into(), "Off");
        let bypass = take("ferritron-bypass".into(), "Bypass");
        let quality = take("ferritron-quality".into(), "Quality");
        let quality_choices = [
            take("ferritron-quality-0".into(), "Auto"),
            take("ferritron-quality-1".into(), "Normal"),
            take("ferritron-quality-2".into(), "Best"),
        ];
        let mut ui: Vec<(String, String)> = table.into_iter().filter(|(k, _)| !k.starts_with("ferritron-")).collect();
        ui.sort();
        Self { language: language.to_string(), names, choices, on, off, bypass, quality, quality_choices, ui }
    }

    /// Messages for the plugin's editor, sorted by key. Variables appear as
    /// `{$name}` for the editor to fill in.
    pub fn ui_messages(&self) -> &[(String, String)] {
        &self.ui
    }

    /// Name of parameter `index`; one past the plugin's parameters is the bypass.
    pub fn param_name(&self, index: usize) -> &'static str {
        self.names.get(index).copied().unwrap_or(self.bypass)
    }

    /// Name of any parameter, Ferritron's built-in switches included.
    pub fn name_of(&self, index: usize, info: &ParamInfo) -> &'static str {
        match info.key {
            "ferritron.bypass" => self.bypass,
            QUALITY_KEY => self.quality,
            _ if index < self.names.len() => self.names[index],
            _ => info.name,
        }
    }

    /// Display names of an enum's choices (built-in switches included).
    fn choices_of(&self, index: usize, info: &ParamInfo) -> &[&'static str] {
        match info.kind {
            _ if info.key == QUALITY_KEY => &self.quality_choices,
            _ if index < self.choices.len() => &self.choices[index],
            ParamKind::Enum { choices } => choices,
            _ => &[],
        }
    }

    /// Display text of a plain value, without the unit.
    pub fn format(&self, index: usize, info: &ParamInfo, plain: f64) -> String {
        let plain = info.sanitize(plain);
        match info.kind {
            ParamKind::Bool => if plain >= 0.5 { self.on } else { self.off }.to_string(),
            ParamKind::Enum { .. } => self.choices_of(index, info).get(plain as usize).copied().unwrap_or("").to_string(),
            _ => info.format(plain),
        }
    }

    /// Parses localized or English display text.
    pub fn parse(&self, index: usize, info: &ParamInfo, text: &str) -> Option<f64> {
        let trimmed = text.trim();
        match info.kind {
            ParamKind::Bool if trimmed == self.on => return Some(1.0),
            ParamKind::Bool if trimmed == self.off => return Some(0.0),
            ParamKind::Enum { .. } => {
                if let Some(n) = self.choices_of(index, info).iter().position(|c| *c == trimmed) {
                    return Some(n as f64);
                }
            }
            _ => {}
        }
        info.parse(text)
    }
}

/// Text messages of a Fluent source. Variable references are kept as
/// `{$name}`; messages with other placeables (selectors, functions, term
/// references) and unparsable entries are skipped.
pub fn messages(source: &str) -> Vec<(String, String)> {
    let resource = match fluent_syntax::parser::parse(source) {
        Ok(resource) => resource,
        Err((resource, _errors)) => resource,
    };
    resource
        .body
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Message(message) => {
                let mut text = String::new();
                for element in message.value?.elements {
                    match element {
                        PatternElement::TextElement { value } => text.push_str(value),
                        PatternElement::Placeable {
                            expression: Expression::Inline(InlineExpression::StringLiteral { value }),
                        } => text.push_str(value),
                        PatternElement::Placeable {
                            expression: Expression::Inline(InlineExpression::VariableReference { id }),
                        } => {
                            text.push_str("{$");
                            text.push_str(id.name);
                            text.push('}');
                        }
                        PatternElement::Placeable { .. } => return None,
                    }
                }
                Some((message.id.name.to_string(), text.trim().to_string()))
            }
            _ => None,
        })
        .collect()
}

/// Normalizes `ja_jp`, `JA-jp.UTF-8` etc. to `ja-JP`.
fn canonical(tag: &str) -> String {
    let tag = tag.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    let mut parts = tag.split('-').filter(|p| !p.is_empty());
    let mut out = parts.next().unwrap_or("").to_ascii_lowercase();
    for part in parts {
        out.push('-');
        out.push_str(&if part.len() == 2 { part.to_ascii_uppercase() } else { part.to_string() });
    }
    out
}

/// `ja-JP` -> `["ja-JP", "ja", "en"]`.
fn fallback_chain(language: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut tag = canonical(language);
    while !tag.is_empty() {
        chain.push(tag.clone());
        tag = tag.rsplit_once('-').map(|(head, _)| head.to_string()).unwrap_or_default();
    }
    if !chain.iter().any(|t| t == "en") {
        chain.push("en".into());
    }
    chain
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|p| p.join("Ferritron"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|p| p.join("Library/Application Support/Ferritron"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|p| p.join(".local/share"))).map(|p| p.join("ferritron"))
    }
}

fn pack_path(plugin_id: &str, language: &str) -> Option<PathBuf> {
    Some(data_dir()?.join("locales").join(plugin_id).join(format!("{language}.ftl")))
}

fn preferred_language(plugin_id: &str) -> Option<String> {
    if let Ok(lang) = std::env::var("FERRITRON_LANGUAGE") {
        return Some(lang);
    }
    let settings = data_dir().and_then(|d| std::fs::read_to_string(d.join("settings.toml")).ok());
    settings.and_then(|s| settings_language(&s, plugin_id)).or_else(os_language)
}

/// Reads `language` from the (deliberately tiny) settings format:
///
/// ```toml
/// language = "ja"                       # all Ferritron plugins
/// [plugins."com.example.gain"]
/// language = "en"                       # this plugin only
/// ```
fn settings_language(text: &str, plugin_id: &str) -> Option<String> {
    let wanted = format!("plugins.\"{plugin_id}\"");
    let (mut section, mut global, mut specific) = (String::new(), None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.trim_start_matches('[').trim_end_matches(']').trim().to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            if key.trim() != "language" {
                continue;
            }
            let value = value.split('#').next().unwrap_or("").trim().trim_matches('"').to_string();
            if section.is_empty() {
                global = Some(value);
            } else if section == wanted {
                specific = Some(value);
            }
        }
    }
    specific.or(global)
}

#[cfg(windows)]
fn os_language() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
    }
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    (len > 1).then(|| String::from_utf16_lossy(&buf[..len as usize - 1]))
}

#[cfg(not(windows))]
fn os_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty() && v != "C"))
}

#[cfg(test)]
mod tests {
    use super::*;

    static INFOS: [ParamInfo; 3] = [
        ParamInfo { id: 1, key: "gain", name: "Gain", unit: "dB", kind: ParamKind::Float { min: 0.0, max: 1.0 }, default: 0.0 },
        ParamInfo { id: 2, key: "mode", name: "Mode", unit: "", kind: ParamKind::Enum { choices: &["Clean", "Warm"] }, default: 0.0 },
        ParamInfo { id: 3, key: "on", name: "Enabled", unit: "", kind: ParamKind::Bool, default: 1.0 },
    ];

    const JA: &str = "param-gain = ゲイン\nparam-mode-1 = ウォーム\n";

    #[test]
    fn chain_and_tags() {
        assert_eq!(fallback_chain("ja_JP.UTF-8"), ["ja-JP", "ja", "en"]);
        assert_eq!(fallback_chain("en-US"), ["en-US", "en"]);
        assert_eq!(canonical("zh-hant-tw"), "zh-hant-TW");
    }

    #[test]
    fn translates_with_fallbacks() {
        let l = Localized::with_language("ja-JP", "test.none", &INFOS, &[("ja", JA)]);
        assert_eq!(l.param_name(0), "ゲイン");
        assert_eq!(l.param_name(1), "Mode"); // untranslated: the project file's name
        assert_eq!(l.format(1, &INFOS[1], 1.0), "ウォーム");
        assert_eq!(l.format(1, &INFOS[1], 0.0), "Clean");
        assert_eq!(l.format(2, &INFOS[2], 1.0), "オン"); // Ferritron's built-in Japanese
        assert_eq!(l.parse(1, &INFOS[1], "ウォーム"), Some(1.0));
        assert_eq!(l.parse(1, &INFOS[1], "warm"), Some(1.0)); // English still accepted
        assert_eq!(l.parse(2, &INFOS[2], "オフ"), Some(0.0));
        assert_eq!(l.param_name(INFOS.len()), "バイパス");
        let quality = ParamInfo {
            id: 9,
            key: QUALITY_KEY,
            name: "Quality",
            unit: "",
            kind: ParamKind::Enum { choices: &["Auto", "Normal", "Best"] },
            default: 0.0,
        };
        assert_eq!(l.name_of(INFOS.len() + 1, &quality), "音質");
        assert_eq!(l.format(INFOS.len() + 1, &quality, 2.0), "最高");
        assert_eq!(l.parse(INFOS.len() + 1, &quality, "自動"), Some(0.0));

        let ui = Localized::with_language("ja", "test.none", &INFOS, &[("ja", "ui-hello = こんにちは { $who }\n"), ("en", "ui-hello = Hello { $who }\nui-only-en = English\n")]);
        assert_eq!(ui.ui_messages(), [("ui-hello".into(), "こんにちは {$who}".into()), ("ui-only-en".into(), "English".into())]);

        let en = Localized::with_language("en", "test.none", &INFOS, &[("ja", JA)]);
        assert_eq!(en.param_name(0), "Gain");
        assert_eq!(en.format(2, &INFOS[2], 1.0), "On");
    }

    #[test]
    fn messages_skip_placeables_and_garbage() {
        let m = messages("a = Hello\nb = Hi { $name }\n!!! broken\nc = { \"x\" }y\nd = { -term }\n");
        assert_eq!(m, [("a".into(), "Hello".into()), ("b".into(), "Hi {$name}".into()), ("c".into(), "xy".into())]);
    }

    #[test]
    fn settings_prefer_the_plugin_section() {
        let s = "language = \"ja\"\n[plugins.\"com.a\"]\nlanguage = \"de\" # comment\n";
        assert_eq!(settings_language(s, "com.a").as_deref(), Some("de"));
        assert_eq!(settings_language(s, "com.b").as_deref(), Some("ja"));
        assert_eq!(settings_language("", "com.a"), None);
    }
}
