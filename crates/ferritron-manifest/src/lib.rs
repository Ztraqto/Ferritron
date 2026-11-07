//! The project file, `<name>.frtx` (TOML inside): where to find it, its
//! types, parsing and validation. Shared by the `#[ferritron::plugin]` macro
//! (which generates code from it) and by the Ferritron Editor (which edits
//! it), so both apply the same rules.

use std::path::{Path, PathBuf};

mod template;
pub use template::{HOME_VARIABLE, Template, framework_dir, new_project};
use toml::{Table, Value};

/// An error a person can read in their own language: a stable `code` (the
/// id of its message in the translation files, Fluent), named arguments,
/// and an English template, which is also its `Display`. An error may wrap
/// the one that `cause`d it; templates show it with `{$cause}`.
#[derive(Clone, Debug, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub args: Vec<(&'static str, String)>,
    pub cause: Option<Box<Error>>,
    template: &'static str,
}

impl Error {
    pub fn new(code: &'static str, template: &'static str, args: Vec<(&'static str, String)>) -> Self {
        Self { code, args, cause: None, template }
    }

    /// The same error, caused by `cause`.
    pub fn because(mut self, cause: Error) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// The text in the language whose messages `lookup` returns (by code),
    /// falling back to English for any message it lacks.
    pub fn render(&self, lookup: &dyn Fn(&str) -> Option<String>) -> String {
        let template = lookup(self.code).unwrap_or_else(|| self.template.to_string());
        let cause = self.cause.as_ref().map(|c| c.render(lookup)).unwrap_or_default();
        fill(&template, &self.args, &cause)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render(&|_| None))
    }
}

impl std::error::Error for Error {}

/// For callers that report errors as plain (English) text.
impl From<Error> for String {
    fn from(e: Error) -> String {
        e.to_string()
    }
}

/// Replaces `{$name}` (also written `{ $name }`) with its argument.
pub fn fill(template: &str, args: &[(&str, String)], cause: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else { break };
        let name = rest[start + 1..start + len].trim().trim_start_matches('$');
        match args.iter().find(|(n, _)| *n == name) {
            Some((_, value)) => out.push_str(value),
            None if name == "cause" => out.push_str(cause),
            None => out.push_str(&rest[start..=start + len]),
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// `error!("code", "English with {$name}", name = value, ...)`.
#[macro_export]
macro_rules! error {
    ($code:literal, $template:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::Error::new($code, $template, vec![$((stringify!($name), ($value).to_string())),*])
    };
}

/// A TOML syntax error, with its line and column.
pub fn syntax_error(text: &str, e: &toml::de::Error) -> Error {
    let at = e.span().map_or(0, |s| s.start).min(text.len());
    let line = text[..at].matches('\n').count() + 1;
    let column = text[..at].rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    error!("toml-syntax", "line {$line}, column {$column}: {$detail}", line = line, column = column, detail = e.message())
}

/// Extension of project files.
pub const EXTENSION: &str = "frtx";

/// The project file's former name, still read when there is no `.frtx`.
pub const LEGACY_NAME: &str = "ferritron.toml";

/// The project file of the package in `dir` (the folder of its
/// `Cargo.toml`): its one `*.frtx` file, or else `ferritron.toml`.
pub fn locate(dir: &Path) -> Result<PathBuf, Error> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| error!("folder-unreadable", "cannot read {$path}: {$detail}", path = dir.display(), detail = e))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION)))
        .collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 if dir.join(LEGACY_NAME).is_file() => Ok(dir.join(LEGACY_NAME)),
        0 => Err(error!("no-project-file", "no project file (<name>.{$extension}) in {$path}", extension = EXTENSION, path = dir.display())),
        _ => {
            found.sort();
            let names: Vec<String> = found.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
            Err(error!("several-project-files", "{$path} has several project files ({$files}): keep one", path = dir.display(), files = names.join(", ")))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub instrument: bool,
    pub parallel: bool,
    pub isolate: bool,
    /// `[isolation] share = "instance"`: one process per instance instead of
    /// one per plugin type.
    pub isolate_per_instance: bool,
    /// `[isolation] pipeline = true`: blocks run one block late, in parallel
    /// with the host (reported as latency).
    pub isolate_pipeline: bool,
    /// `[ara]`: ARA 2 support (`ferritron::ara::AraRender`).
    pub ara: bool,
    /// `[mcp]`: AI clients may reach the plugin through the Ferritron MCP hub
    /// (only while the user turns its "AI Access" switch on).
    pub mcp: bool,
    /// `[quality]`: the plugin gets a "Quality" switch (Auto / Normal /
    /// Best) for `ctx.quality`; Auto picks Best while the host exports.
    pub quality: bool,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub url: String,
    pub email: String,
    pub params: Vec<Param>,
    /// Audio Unit codes (`[au]`, with defaults), used when the plugin is
    /// packaged as an AU on macOS.
    pub au: AuCodes,
}

/// The three codes an Audio Unit is registered with (`AudioComponentDescription`).
#[derive(Clone, Debug, PartialEq)]
pub struct AuCodes {
    /// `aufx` (effect), `aumu` (instrument) or `aumf` (effect that takes MIDI).
    pub kind: String,
    /// Identifies the plugin among the vendor's: 4 ASCII characters.
    pub subtype: String,
    /// Identifies the vendor: 4 ASCII characters, at least one not lowercase
    /// (all-lowercase codes are Apple's).
    pub manufacturer: String,
}

impl Manifest {
    /// The version as an integer (`major << 16 | minor << 8 | patch`), as
    /// Audio Units report it.
    pub fn version_number(&self) -> u32 {
        let mut parts = self.version.split(['.', '-', '+']).map(|p| p.parse::<u32>().unwrap_or(0));
        let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        (major.min(0xffff) << 16) | (minor.min(0xff) << 8) | patch.min(0xff)
    }

    /// Values for packaging templates (such as a format's Info.plist):
    /// `{name}`, `{id}`, `{version}`, `{version.number}`, `{vendor}`,
    /// `{url}`, `{kind}`, `{au.type}`, `{au.subtype}`, `{au.manufacturer}`,
    /// `{au.tag}`, `{au.sandbox-safe}`.
    pub fn template_values(&self) -> Vec<(String, String)> {
        [
            ("name", self.name.clone()),
            ("id", self.id.clone()),
            ("version", self.version.clone()),
            ("version.number", self.version_number().to_string()),
            ("vendor", if self.vendor.is_empty() { "Ferritron".into() } else { self.vendor.clone() }),
            ("url", self.url.clone()),
            ("kind", if self.instrument { "instrument" } else { "effect" }.into()),
            ("au.type", self.au.kind.clone()),
            ("au.subtype", self.au.subtype.clone()),
            ("au.manufacturer", self.au.manufacturer.clone()),
            ("au.tag", if self.instrument { "Synthesizer" } else { "Effects" }.into()),
            // Sandboxed hosts cannot start the process an isolated plugin runs in.
            ("au.sandbox-safe", if self.isolate { "false" } else { "true" }.into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

/// A 4-character code from `[au]`, or `default`.
fn four_cc(table: Option<&Table>, key: &str, default: String) -> Result<String, Error> {
    match table.and_then(|t| t.get(key)) {
        None => Ok(default),
        Some(v) => {
            let code = v.as_str().ok_or_else(|| not_string(&format!("au.{key}")))?;
            if code.len() != 4 || !code.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                return Err(error!("bad-au-code", "{$key} must be exactly 4 ASCII characters, such as \"Gain\"", key = format!("au.{key}")));
            }
            Ok(code.to_string())
        }
    }
}

/// Default codes: the subtype from the plugin ID (stable, so hosts keep
/// finding the plugin), the manufacturer from the vendor's name.
fn au_codes(au: Option<&Table>, id: &str, vendor: &str, instrument: bool) -> Result<AuCodes, Error> {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in id.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let subtype: String = (0..4).map(|i| ALPHABET[((hash >> (i * 8)) % ALPHABET.len() as u64) as usize] as char).collect();
    let letters: Vec<char> = vendor.chars().filter(char::is_ascii_alphanumeric).take(4).collect();
    let mut manufacturer: String =
        letters.iter().enumerate().map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect();
    if manufacturer.is_empty() {
        manufacturer = "Frtn".into();
    }
    while manufacturer.len() < 4 {
        manufacturer.push('x');
    }
    if let Some(t) = au {
        reject_unknown(t, "au.", &["type", "subtype", "manufacturer"])?;
    }
    let kind = match au.and_then(|t| t.get("type")) {
        None => if instrument { "aumu" } else { "aufx" }.to_string(),
        Some(v) => match v.as_str() {
            Some(code @ ("aufx" | "aumu" | "aumf")) => code.to_string(),
            _ => return Err(not_one_of("au.type", &v.to_string(), "aufx, aumu, aumf")),
        },
    };
    let codes = AuCodes { kind, subtype: four_cc(au, "subtype", subtype)?, manufacturer: four_cc(au, "manufacturer", manufacturer)? };
    if codes.manufacturer.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(error!("au-manufacturer-lowercase", "au.manufacturer needs at least one character that is not a lowercase letter (all-lowercase codes are reserved by Apple)"));
    }
    Ok(codes)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub key: String,
    pub name: String,
    pub unit: String,
    pub kind: Kind,
    pub default: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Float { min: f64, max: f64 },
    Int { min: i64, max: i64 },
    Bool,
    Enum { choices: Vec<String> },
}

pub fn parse(text: &str) -> Result<Manifest, Error> {
    let root: Table = text.parse().map_err(|e: toml::de::Error| syntax_error(text, &e))?;
    reject_unknown(&root, "", &["plugin", "params", "view", "isolation", "mcp", "quality", "ara", "au"])?;
    let ara = match root.get("ara") {
        None => false,
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[ara]"))?;
            reject_unknown(t, "ara.", &["enabled"])?;
            match t.get("enabled") {
                None => true,
                Some(v) => v.as_bool().ok_or_else(|| not_bool("ara.enabled"))?,
            }
        }
    };
    let mcp = match root.get("mcp") {
        None => false,
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[mcp]"))?;
            reject_unknown(t, "mcp.", &["enabled"])?;
            match t.get("enabled") {
                None => true,
                Some(v) => v.as_bool().ok_or_else(|| not_bool("mcp.enabled"))?,
            }
        }
    };
    let quality = match root.get("quality") {
        None => false,
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[quality]"))?;
            reject_unknown(t, "quality.", &["enabled"])?;
            match t.get("enabled") {
                None => true,
                Some(v) => v.as_bool().ok_or_else(|| not_bool("quality.enabled"))?,
            }
        }
    };
    let (isolate, isolate_per_instance, isolate_pipeline) = match root.get("isolation") {
        None => (false, false, false),
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[isolation]"))?;
            reject_unknown(t, "isolation.", &["mode", "share", "pipeline"])?;
            let isolate = match t.get("mode").and_then(Value::as_str) {
                None | Some("guarded") => false,
                Some("process") => true,
                Some(other) => return Err(not_one_of("isolation.mode", other, "guarded, process")),
            };
            let per_instance = match t.get("share") {
                None => false,
                Some(v) => match v.as_str() {
                    Some("plugin") => false,
                    Some("instance") => true,
                    _ => return Err(not_one_of("isolation.share", &v.to_string(), "plugin, instance")),
                },
            };
            let pipeline = match t.get("pipeline") {
                None => false,
                Some(v) => v.as_bool().ok_or_else(|| not_bool("isolation.pipeline"))?,
            };
            (isolate, per_instance, pipeline)
        }
    };

    let plugin = root.get("plugin").and_then(Value::as_table).ok_or_else(|| error!("missing-plugin", "the [plugin] section is missing"))?;
    reject_unknown(plugin, "plugin.", &["id", "name", "kind", "vendor", "version", "url", "email", "parallel"])?;
    let string = |key: &str, default: Option<String>| -> Result<String, Error> {
        match plugin.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(not_string(&format!("plugin.{key}"))),
            None => default.ok_or_else(|| required(&format!("plugin.{key}"))),
        }
    };
    let id = string("id", None)?;
    if id.is_empty() || id.contains(char::is_whitespace) {
        return Err(error!("bad-plugin-id", "plugin.id must be a non-empty reverse-domain identifier such as \"com.example.gain\""));
    }

    let mut params = Vec::new();
    if let Some(table) = root.get("params") {
        let table = table.as_table().ok_or_else(|| not_table("[params]"))?;
        for (key, value) in table {
            params.push(parse_param(key, value)?);
        }
    }

    let instrument = match string("kind", Some("effect".into()))?.as_str() {
        "effect" => false,
        "instrument" => true,
        other => return Err(not_one_of("plugin.kind", other, "effect, instrument")),
    };

    let parallel = match plugin.get("parallel") {
        Some(v) => v.as_bool().ok_or_else(|| not_bool("plugin.parallel"))?,
        None => false,
    };

    let url = string("url", Some(String::new()))?;
    if ara && url.is_empty() {
        // ARA hosts show it and reject factories without one.
        return Err(error!("ara-needs-url", "[ara] needs plugin.url (a web page about the plugin, required by ARA)"));
    }

    let vendor = string("vendor", Some(String::new()))?;
    let au_table = match root.get("au") {
        None => None,
        Some(v) => Some(v.as_table().ok_or_else(|| not_table("[au]"))?),
    };
    let au = au_codes(au_table, &id, &vendor, instrument)?;

    Ok(Manifest {
        instrument,
        parallel,
        isolate,
        isolate_per_instance,
        isolate_pipeline,
        ara,
        mcp,
        quality,
        name: string("name", None)?,
        vendor,
        version: string("version", Some(std::env::var("CARGO_PKG_VERSION").unwrap_or_default()))?,
        url,
        email: string("email", Some(String::new()))?,
        id,
        params,
        au,
    })
}

fn parse_param(key: &str, value: &Value) -> Result<Param, Error> {
    let at = format!("params.{key}");
    let valid_ident = key.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_ident {
        return Err(error!("bad-param-key", "{$key}: parameter keys must be snake_case identifiers (lowercase letters, digits and _)", key = key));
    }
    let t = value.as_table().ok_or_else(|| not_table(&at))?;
    reject_unknown(t, &format!("{at}."), &["type", "name", "unit", "range", "default", "choices"])?;

    let ty = t.get("type").and_then(Value::as_str).ok_or_else(|| required(&format!("{at}.type")))?;
    let name = match t.get("name") {
        Some(v) => v.as_str().ok_or_else(|| not_string(&format!("{at}.name")))?.to_string(),
        None => title_case(key),
    };
    let unit = match t.get("unit") {
        Some(v) => v.as_str().ok_or_else(|| not_string(&format!("{at}.unit")))?.to_string(),
        None => String::new(),
    };
    let default = t.get("default");

    let (kind, default) = match ty {
        "float" => {
            let (min, max) = range(t, &at, Value::as_float_lossy)?;
            let d = match default {
                Some(v) => v.as_float_lossy().ok_or_else(|| error!("not-number", "{$at} must be a number", at = format!("{at}.default")))?,
                None => min,
            };
            (Kind::Float { min, max }, d)
        }
        "int" => {
            let (min, max) = range(t, &at, Value::as_integer)?;
            let d = match default {
                Some(v) => v.as_integer().ok_or_else(|| error!("not-integer", "{$at} must be an integer", at = format!("{at}.default")))?,
                None => min,
            };
            if min < i32::MIN as i64 || max > i32::MAX as i64 {
                return Err(error!("range-too-wide", "{$at} must fit in 32-bit integers", at = format!("{at}.range")));
            }
            (Kind::Int { min, max }, d as f64)
        }
        "bool" => {
            let d = match default {
                Some(v) => v.as_bool().ok_or_else(|| not_bool(&format!("{at}.default")))?,
                None => false,
            };
            (Kind::Bool, if d { 1.0 } else { 0.0 })
        }
        "enum" => {
            let choices: Vec<String> = t
                .get("choices")
                .and_then(Value::as_array)
                .ok_or_else(|| error!("choices-required", "{$at} is required for enum parameters", at = format!("{at}.choices")))?
                .iter()
                .map(|c| c.as_str().map(str::to_string))
                .collect::<Option<_>>()
                .ok_or_else(|| error!("choices-not-strings", "{$at} must be strings", at = format!("{at}.choices")))?;
            if choices.is_empty() {
                return Err(error!("choices-empty", "{$at} must not be empty", at = format!("{at}.choices")));
            }
            let d = match default {
                Some(Value::String(s)) => {
                    choices.iter().position(|c| c == s).ok_or_else(|| error!("default-not-a-choice", "{$at}: \"{$value}\" is not one of the choices", at = format!("{at}.default"), value = s))?
                }
                Some(_) => return Err(error!("default-not-a-choice-name", "{$at} must be one of the choices", at = format!("{at}.default"))),
                None => 0,
            };
            (Kind::Enum { choices }, d as f64)
        }
        other => return Err(not_one_of(&format!("{at}.type"), other, "float, int, bool, enum")),
    };

    let (lo, hi) = match &kind {
        Kind::Float { min, max } => (*min, *max),
        Kind::Int { min, max } => (*min as f64, *max as f64),
        _ => (default, default),
    };
    if !(lo..=hi).contains(&default) {
        return Err(error!("default-out-of-range", "{$at} must be inside the range", at = format!("{at}.default")));
    }
    Ok(Param { key: key.to_string(), name, unit, kind, default })
}

fn range<T: PartialOrd + Copy>(t: &Table, at: &str, get: impl Fn(&Value) -> Option<T>) -> Result<(T, T), Error> {
    let err = || error!("bad-range", "{$at} must be [min, max]", at = format!("{at}.range"));
    let r = t.get("range").and_then(Value::as_array).ok_or_else(err)?;
    if r.len() != 2 {
        return Err(err());
    }
    let (min, max) = (get(&r[0]).ok_or_else(err)?, get(&r[1]).ok_or_else(err)?);
    // Values are finite here, so `>=` is the exact negation of `<`.
    if min >= max {
        return Err(error!("range-min-max", "{$at}: min must be smaller than max", at = format!("{at}.range")));
    }
    Ok((min, max))
}

trait FloatLossy {
    fn as_float_lossy(&self) -> Option<f64>;
}

impl FloatLossy for Value {
    fn as_float_lossy(&self) -> Option<f64> {
        let v = self.as_float().or_else(|| self.as_integer().map(|i| i as f64))?;
        v.is_finite().then_some(v)
    }
}

fn reject_unknown(t: &Table, prefix: &str, allowed: &[&str]) -> Result<(), Error> {
    match t.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(error!("unknown-key", "unknown key `{$key}` (allowed: {$allowed})", key = format!("{prefix}{k}"), allowed = allowed.join(", "))),
        None => Ok(()),
    }
}

// Messages several checks share.
fn not_table(at: &str) -> Error {
    error!("not-table", "{$at} must be a table", at = at)
}
fn not_bool(at: &str) -> Error {
    error!("not-bool", "{$at} must be true or false", at = at)
}
fn not_string(at: &str) -> Error {
    error!("not-string", "{$at} must be a string", at = at)
}
fn required(at: &str) -> Error {
    error!("required", "{$at} is required", at = at)
}
fn not_one_of(at: &str, value: &str, allowed: &str) -> Error {
    error!("not-one-of", "{$at}: \"{$value}\" is not one of {$allowed}", at = at, value = value, allowed = allowed)
}

fn title_case(key: &str) -> String {
    key.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAIN: &str = r#"
        [plugin]
        id = "com.example.gain"
        name = "Gain"

        [params]
        gain = { type = "float", range = [-60, 12], default = 0, unit = "dB" }
        mode = { type = "enum", choices = ["Clean", "Warm"], default = "Warm" }
        bypass_all = { type = "bool" }
    "#;

    #[test]
    fn errors_render_in_another_language() {
        let e = parse("[plugin]\nname = \"x\"").unwrap_err();
        assert_eq!((e.code, e.to_string().as_str()), ("required", "plugin.id is required"));
        let ja = |code: &str| (code == "required").then(|| "{ $at } は必須です".to_string());
        assert_eq!(e.render(&ja), "plugin.id は必須です");
        let syntax = parse("[plugin]\nid = \"a\"\nname = = 1").unwrap_err();
        assert_eq!(syntax.code, "toml-syntax");
        assert!(syntax.to_string().starts_with("line 3, column "), "{syntax}");
        let wrapped = error!("in-file", "{$file}: {$cause}", file = "a.frtx").because(syntax);
        assert!(wrapped.to_string().starts_with("a.frtx: line 3"), "{wrapped}");
    }

    #[test]
    fn finds_the_project_file() {
        let dir = std::env::temp_dir().join(format!("ferritron-manifest-locate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(locate(&dir).unwrap_err().to_string().contains("no project file"));
        std::fs::write(dir.join(LEGACY_NAME), GAIN).unwrap();
        assert_eq!(locate(&dir).unwrap(), dir.join(LEGACY_NAME));
        std::fs::write(dir.join("gain.frtx"), GAIN).unwrap();
        assert_eq!(locate(&dir).unwrap(), dir.join("gain.frtx"), ".frtx wins over the legacy name");
        std::fs::write(dir.join("other.frtx"), GAIN).unwrap();
        assert!(locate(&dir).unwrap_err().to_string().contains("several project files"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_a_valid_manifest() {
        let m = parse(GAIN).unwrap();
        assert_eq!(m.id, "com.example.gain");
        assert_eq!(m.params.len(), 3);
        assert_eq!(m.params[1].default, 1.0);
        assert_eq!(m.params[2].name, "Bypass All");
        assert!(!m.isolate && !m.isolate_per_instance && !m.isolate_pipeline && !m.ara);
        assert_eq!(parse(&format!("{GAIN}\n[ara]")).err().unwrap().code, "ara-needs-url");
        let with_url = GAIN.replace("name = \"Gain\"", "name = \"Gain\"\nurl = \"https://example.com\"");
        assert!(parse(&format!("{with_url}\n[ara]")).unwrap().ara);
        assert!(!parse(&format!("{GAIN}\n[ara]\nenabled = false")).unwrap().ara);
        // Audio Unit codes: defaults from the ID and vendor, or given.
        let au = parse(GAIN).unwrap().au;
        assert_eq!(au.kind, "aufx");
        assert_eq!(au.subtype.len(), 4);
        assert_eq!(au.subtype, parse(GAIN).unwrap().au.subtype, "stable");
        let given = parse(&format!("{GAIN}\n[au]\nsubtype = \"Gain\"\nmanufacturer = \"Ztrq\"")).unwrap();
        assert_eq!((given.au.subtype.as_str(), given.au.manufacturer.as_str()), ("Gain", "Ztrq"));
        assert_eq!(parse(&format!("{GAIN}\n[au]\nsubtype = \"toolong\"")).err().unwrap().code, "bad-au-code");
        assert_eq!(parse(&format!("{GAIN}\n[au]\nmanufacturer = \"abcd\"")).err().unwrap().code, "au-manufacturer-lowercase");
        assert!(parse(&format!("{GAIN}\n[au]\ntype = \"aumf\"")).is_ok());
    }

    #[test]
    fn reads_isolation_options() {
        let m = parse(&format!("{GAIN}\n[isolation]\nmode = \"process\"\nshare = \"instance\"\npipeline = true")).unwrap();
        assert!(m.isolate && m.isolate_per_instance && m.isolate_pipeline);
        let m = parse(&format!("{GAIN}\n[isolation]\nmode = \"process\"\nshare = \"plugin\"")).unwrap();
        assert!(m.isolate && !m.isolate_per_instance && !m.isolate_pipeline);
        assert_eq!(parse(&format!("{GAIN}\n[isolation]\nshare = \"all\"")).err().unwrap().code, "not-one-of");
        assert_eq!(parse(&format!("{GAIN}\n[isolation]\npipeline = 1")).err().unwrap().code, "not-bool");
    }

    #[test]
    fn rejects_mistakes_with_clear_messages() {
        let err = |src: &str| parse(src).err().unwrap().to_string();
        assert!(err("[plugin]\nname = \"x\"").contains("plugin.id is required"));
        assert!(err("[plugin]\nid = \"a\"\nname = \"x\"\nnmae = 1").contains("unknown key `plugin.nmae`"));
        assert!(err("[plugin]\nid=\"a\"\nname=\"x\"\n[params]\ng={type=\"float\",range=[1,0]}").contains("min must be smaller"));
        assert!(err("[plugin]\nid=\"a\"\nname=\"x\"\n[params]\ng={type=\"float\",range=[0,1],default=5}").contains("inside the range"));
        assert!(err("[plugin]\nid=\"a\"\nname=\"x\"\n[params]\nGain={type=\"bool\"}").contains("snake_case"));
    }

}
