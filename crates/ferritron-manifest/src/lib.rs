//! The project file, `<name>.ferritroject` (TOML inside; `<name>.frtx` before 1.2): where to find it, its
//! types, parsing and validation. Shared by the `#[ferritron::plugin]` macro
//! (which generates code from it) and by the Ferritron Editor (which edits
//! it), so both apply the same rules.

use std::path::{Path, PathBuf};

mod package;
mod template;
pub use package::{windows_tuid, Lv2Port, lv2_port_plan, class_id, vst3_uid_string, Escape, HostParam, Scope, Value as TemplateValue, param_id, render, reserved_id, legacy_reserved_id, RESERVED_BASE};
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

mod group;
mod streams;
mod presets;
pub use presets::Preset;
pub use streams::{MAX_STREAM_LEN, Stream};

/// A TOML syntax error, with its line and column.
pub fn syntax_error(text: &str, e: &toml::de::Error) -> Error {
    let at = e.span().map_or(0, |s| s.start).min(text.len());
    let line = text[..at].matches('\n').count() + 1;
    let column = text[..at].rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    error!("toml-syntax", "line {$line}, column {$column}: {$detail}", line = line, column = column, detail = e.message())
}

/// Extension of project files: what new projects get.
pub const EXTENSION: &str = "ferritroject";

/// The extension project files had before 1.2. Ferritron 2.0 no longer reads it: [`locate`]
/// stops and points to `cargo ferritron migrate-frtx` ([`migrate_frtx`] renames the file).
pub const OLD_EXTENSION: &str = "frtx";

/// Every extension a project file may have, the default first. (`is_project_file` still
/// recognises `.frtx`, so tools that move or lock project files find old ones too.)
pub const EXTENSIONS: [&str; 2] = [EXTENSION, OLD_EXTENSION];

/// The project file's former name, still read when there is no project file.
pub const LEGACY_NAME: &str = "ferritron.toml";

/// Whether `path` is named like a project file (`.ferritroject` or the old `.frtx`).
pub fn is_project_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn is_old_project_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(OLD_EXTENSION))
}

/// The project file of the package in `dir` (the folder of its
/// `Cargo.toml`): its one `*.ferritroject` file, or else `ferritron.toml`. Two project
/// files are an error: which one the build used would not be clear.
///
/// A `*.frtx` file (before 1.2) is an error too, which names `cargo ferritron
/// migrate-frtx`: Ferritron 2.0 no longer reads that extension. Tools that must still
/// find such a file (to rename or lock it) use [`locate_any`].
pub fn locate(dir: &Path) -> Result<PathBuf, Error> {
    let found = locate_any(dir)?;
    if is_old_project_file(&found) {
        return Err(frtx_error(&found));
    }
    Ok(found)
}

fn frtx_error(file: &Path) -> Error {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let new = file.with_extension(EXTENSION).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    error!(
        "frtx-not-read",
        "{$file} has the old .frtx extension, which Ferritron 2.0 no longer reads: run `cargo ferritron migrate-frtx <package>` to rename it to {$new}. / 古い拡張子 .frtx は Ferritron 2.0 では読み込みません。`cargo ferritron migrate-frtx <package>` で {$new} に名前を変更してください",
        file = name,
        new = new
    )
}

/// Like [`locate`], but a `.frtx` file (before 1.2) is found too.
pub fn locate_any(dir: &Path) -> Result<PathBuf, Error> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| error!("folder-unreadable", "cannot read {$path}: {$detail}", path = dir.display(), detail = e))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_project_file(path))
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

/// `cargo ferritron migrate-frtx`: renames the package's `<name>.frtx` to `<name>.ferritroject`
/// (a plain rename; the content is the same). Returns (old path, new path).
pub fn migrate_frtx(dir: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let old = match locate_any(dir) {
        Ok(path) if is_old_project_file(&path) => path,
        Ok(_) | Err(_) => {
            let any = std::fs::read_dir(dir).ok().into_iter().flatten().flatten().map(|e| e.path()).find(|p| p.is_file() && is_old_project_file(p));
            any.ok_or_else(|| error!("no-frtx-file", "{$path} has no .frtx file to rename", path = dir.display()))?
        }
    };
    let new = old.with_extension(EXTENSION);
    if new.exists() {
        return Err(error!("frtx-target-exists", "{$new} exists already; keep one of {$old} and {$new} and delete the other", old = old.display(), new = new.display()));
    }
    std::fs::rename(&old, &new).map_err(|e| error!("frtx-rename-failed", "cannot rename {$old}: {$detail}", old = old.display(), detail = e))?;
    Ok((old, new))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub instrument: bool,
    pub parallel: bool,
    /// `[process] automation`: 0 `"control"` (default), 1 `"sample"`, 2 `"events"`.
    pub automation: u8,
    /// `[process] control_block`: the control period in samples (default 32).
    pub control_block: u32,
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
    /// Best) for `ctx.quality()`; Auto picks Best while the host exports.
    pub quality: bool,
    /// `[view]`: the plugin has an editor.
    pub view: bool,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub url: String,
    pub email: String,
    pub params: Vec<Param>,
    /// The repeated groups of `[params]` (`[params.<group>] repeat = N`); their expanded
    /// parameters are in `params`, copy by copy.
    pub groups: Vec<Group>,
    /// `[plugin] midi_input = true`: an effect that also takes MIDI (`ctx.midi()`).
    pub midi_input: bool,
    /// `[streams]`: data streams for editors and taps for workers (`ctx.streams()`).
    pub streams: Vec<Stream>,
    /// `[presets.<id>]`: factory presets (parameter values by name).
    pub presets: Vec<Preset>,
    /// `[audio]`: channel counts, layouts and the sidechain.
    pub audio: AudioSpec,
    /// `[vst3] class_id`: the 16 bytes of the VST3 class ID, if given (else
    /// derived from `id`, as in 1.3.0). Never change it after release.
    pub vst3_class_id: Option<[u8; 16]>,
    /// `[vst3] controller_id`: the edit controller is registered as a class of its own
    /// (the component's `getControllerClassId` names it). Without it the one object is both
    /// component and controller, as in 1.x. Never change it after release.
    pub vst3_controller_id: Option<[u8; 16]>,
    /// `[vst3] compatibility = { id, replaces }`: older class IDs this plugin takes over.
    pub vst3_compat: Option<Vst3Compat>,
    /// `[vst2] unique_id = "ABCD"`: the VST 2 plugin ID (4 characters), if given
    /// (else derived from the plugin ID). Never change it after release.
    pub vst2_id: Option<u32>,
    /// `[plugin] categories`: Ferritron's category names (see [`CATEGORIES`]).
    pub categories: Vec<String>,
    /// `[vst3] subcategories`: the VST3 sub-categories verbatim (`Fx|EQ`),
    /// overriding what `categories` give.
    pub vst3_subcategories: Option<String>,
    /// `[clap] features`: the CLAP features verbatim, overriding `categories`.
    pub clap_features: Option<Vec<String>>,
    /// Audio Unit codes (`[au]`, with defaults), used when the plugin is
    /// packaged as an AU on macOS.
    pub au: AuCodes,
    /// `[sdk]`: SDK versions pinned for this project (`vst3 = "3.8.1"`), by
    /// SDK id. `cargo ferritron bundle` builds with exactly these versions
    /// from Ferritron's SDK folder.
    pub sdk: std::collections::BTreeMap<String, String>,
}

/// `[vst3] compatibility = { id = "...", replaces = ["<old class ID>", ...] }` (VST 3.7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vst3Compat {
    /// The ID of the "Plugin Compatibility Class" the factory offers (for hosts that find no
    /// moduleinfo.json), written like `class_id`.
    pub id: [u8; 16],
    /// The class IDs of the plug-ins this one replaces, as the hosts know them (the 32 hex digits
    /// of the old plug-in's own moduleinfo.json / validator output), in the order written.
    pub replaces: Vec<[u8; 16]>,
}

/// The SDK ids `[sdk]` may pin.
pub const SDK_IDS: [&str; 7] = ["vst3", "clap", "vst2", "ara", "aax", "ndk", "xclang"];

/// `[audio]` of the project file.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpec {
    /// Fewest and most channels of the main bus (default 1 and 2).
    pub min_channels: u32,
    pub max_channels: u32,
    /// `layouts`: names the plugin accepts; empty = any layout with a channel count in range.
    pub layouts: Vec<String>,
    /// `sidechain = { channels = "main" | <n> }`: `Some(0)` = as many as the main bus.
    pub sidechain: Option<u32>,
    /// `unsupported = "passthrough"`: for layouts out of range, pass the audio through
    /// instead of refusing them.
    pub passthrough: bool,
    /// `[midi] vst3_cc = "host"`: VST3 controllers are left to the host's own mapping
    /// (no `IMidiMapping`); `"events"` (default) delivers them as `ctx.midi()`.
    pub host_cc: bool,
}

impl Default for AudioSpec {
    fn default() -> Self {
        Self { min_channels: 1, max_channels: 2, layouts: Vec::new(), sidechain: None, passthrough: false, host_cc: false }
    }
}

/// Most channels a bus may have.
pub const MAX_CHANNELS: u32 = 32;

/// Layout names `[audio] layouts` knows, with their channel counts.
pub const LAYOUTS: &[(&str, u32)] = &[
    ("mono", 1), ("stereo", 2), ("lcr", 3), ("quad", 4), ("5.0", 5), ("5.1", 6), ("6.1", 7), ("7.1", 8),
    ("7.1.2", 10), ("7.1.4", 12), ("9.1.6", 16), ("22.2", 24),
];

fn parse_audio(root: &Table) -> Result<AudioSpec, Error> {
    let mut spec = AudioSpec::default();
    let Some(v) = root.get("audio") else { return Ok(spec) };
    let t = v.as_table().ok_or_else(|| not_table("[audio]"))?;
    reject_unknown(t, "audio.", &["channels", "layouts", "sidechain", "unsupported"])?;
    if let Some(c) = t.get("channels") {
        let ct = c.as_table().ok_or_else(|| not_table("audio.channels"))?;
        reject_unknown(ct, "audio.channels.", &["min", "max"])?;
        let get = |k: &str, d: u32| -> Result<u32, Error> {
            match ct.get(k) {
                None => Ok(d),
                Some(v) => v.as_integer().filter(|n| (1..=i64::from(MAX_CHANNELS)).contains(n)).map(|n| n as u32).ok_or_else(|| {
                    error!("bad-channels", "{$at} must be an integer from 1 to {$max}", at = format!("audio.channels.{k}"), max = MAX_CHANNELS)
                }),
            }
        };
        spec.min_channels = get("min", 1)?;
        spec.max_channels = get("max", spec.min_channels.max(2))?;
        if spec.min_channels > spec.max_channels {
            return Err(error!("channels-min-max", "audio.channels: min must not be larger than max"));
        }
    }
    match t.get("layouts") {
        None => {}
        Some(Value::String(s)) if s == "any" => {}
        Some(v) => {
            let names = v.as_array().and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
                .ok_or_else(|| error!("not-string-list", "{$at} must be a list of strings", at = "audio.layouts"))?;
            for n in &names {
                if !LAYOUTS.iter().any(|(l, _)| l == n) {
                    return Err(not_one_of("audio.layouts", n, &LAYOUTS.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", ")));
                }
            }
            if t.get("channels").is_none() {
                let counts = names.iter().filter_map(|n| LAYOUTS.iter().find(|(l, _)| l == n).map(|(_, c)| *c));
                spec.min_channels = counts.clone().min().unwrap_or(1);
                spec.max_channels = counts.max().unwrap_or(2);
            }
            spec.layouts = names;
        }
    }
    if let Some(sc) = t.get("sidechain") {
        let st = sc.as_table().ok_or_else(|| not_table("audio.sidechain"))?;
        reject_unknown(st, "audio.sidechain.", &["channels"])?;
        spec.sidechain = Some(match st.get("channels") {
            None => 0,
            Some(Value::String(s)) if s == "main" => 0,
            Some(v) => v.as_integer().filter(|n| (1..=i64::from(MAX_CHANNELS)).contains(n)).map(|n| n as u32).ok_or_else(|| {
                error!("bad-channels", "{$at} must be an integer from 1 to {$max}", at = "audio.sidechain.channels", max = MAX_CHANNELS)
            })?,
        });
    }
    spec.passthrough = match t.get("unsupported") {
        None => false,
        Some(Value::String(s)) if s == "reject" => false,
        Some(Value::String(s)) if s == "passthrough" => true,
        Some(v) => return Err(not_one_of("audio.unsupported", &v.to_string(), "reject, passthrough")),
    };
    Ok(spec)
}

/// Category names `[plugin] categories` accepts, with the VST3 sub-category
/// and CLAP feature each stands for. A name not in the table is passed
/// through (as written for VST3, lowercase for CLAP), so formats' own words
/// still work; `[vst3] subcategories` / `[clap] features` override all of it.
pub const CATEGORIES: &[(&str, &str, &str)] = &[
    ("EQ", "EQ", "equalizer"),
    ("Dynamics", "Dynamics", "compressor"),
    ("Compressor", "Dynamics", "compressor"),
    ("Limiter", "Dynamics", "limiter"),
    ("Gate", "Dynamics", "gate"),
    ("Distortion", "Distortion", "distortion"),
    ("Filter", "Filter", "filter"),
    ("Delay", "Delay", "delay"),
    ("Reverb", "Reverb", "reverb"),
    ("Modulation", "Modulation", "chorus"),
    ("Chorus", "Modulation", "chorus"),
    ("Flanger", "Modulation", "flanger"),
    ("Phaser", "Modulation", "phaser"),
    ("Pitch", "Pitch Shift", "pitch-correction"),
    ("Analyzer", "Analyzer", "analyzer"),
    ("Multi-band", "Tools", "multi-effects"),
    ("Tools", "Tools", "utility"),
    ("Utility", "Tools", "utility"),
    ("Mastering", "Mastering", "mastering"),
    ("Spatial", "Spatial", "surround"),
    ("Restoration", "Restoration", "restoration"),
    ("Synth", "Synth", "synthesizer"),
    ("Sampler", "Sampler", "sampler"),
    ("Drum", "Drum", "drum-machine"),
    ("Generator", "Generator", "synthesizer"),
];

impl Manifest {
    /// The VST3 sub-categories (`Fx|EQ|Analyzer`), or `Fx` / `Instrument`.
    pub fn vst3_sub_categories(&self) -> String {
        if let Some(s) = &self.vst3_subcategories {
            return s.clone();
        }
        let mut out = String::from(if self.instrument { "Instrument" } else { "Fx" });
        let mut seen: Vec<String> = Vec::new();
        for c in &self.categories {
            let v = CATEGORIES.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(c)).map_or_else(|| c.clone(), |(_, v, _)| v.to_string());
            if !seen.contains(&v) {
                out.push('|');
                out.push_str(&v);
                seen.push(v);
            }
        }
        out
    }

    /// The CLAP features: `audio-effect` (or `instrument`), `stereo`, and one
    /// per category.
    pub fn clap_feature_list(&self) -> Vec<String> {
        if let Some(f) = &self.clap_features {
            return f.clone();
        }
        let mut out: Vec<String> = vec![if self.instrument { "instrument" } else { "audio-effect" }.into(), "stereo".into()];
        for c in &self.categories {
            let f = CATEGORIES
                .iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(c))
                .map_or_else(|| c.to_ascii_lowercase().replace(' ', "-"), |(_, _, f)| f.to_string());
            if !out.contains(&f) {
                out.push(f);
            }
        }
        out
    }
}

/// `3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11` -> 16 bytes (in the order written).
pub fn parse_guid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-' && *c != '{' && *c != '}').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
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
            ("email", self.email.clone()),
            ("kind", if self.instrument { "instrument" } else { "effect" }.into()),
            ("au.type", self.au.kind.clone()),
            ("au.subtype", self.au.subtype.clone()),
            ("au.manufacturer", self.au.manufacturer.clone()),
            ("au.tag", if self.instrument { "Synthesizer" } else { "Effects" }.into()),
            // Sandboxed hosts cannot start the process an isolated plugin runs in.
            ("au.sandbox-safe", if self.isolate { "false" } else { "true" }.into()),
            // The ID as a Java package / bundle identifier (Android, iOS).
            ("id.package", java_package(&self.id)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

/// `dev.ferritron.examples.sine-synth` -> `dev.ferritron.examples.sine_synth`:
/// every part a Java identifier (Android application IDs, iOS bundle IDs).
pub fn java_package(id: &str) -> String {
    id.split('.')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let clean: String = part.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            if clean.starts_with(|c: char| c.is_ascii_digit()) { format!("_{clean}") } else { clean }
        })
        .collect::<Vec<_>>()
        .join(".")
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
    /// `automatable = false`: hosts do not offer it for automation.
    pub automatable: bool,
    /// `hidden = true`: hosts do not list it (the plugin's editor may still show it).
    pub hidden: bool,
    /// `poly = true` (floats only): hosts may modulate it per note (CLAP polyphonic modulation);
    /// the plugin reads the per-voice value with `ctx.value_for`.
    pub poly: bool,
    /// The ID hosts, automation and MIDI maps know it by: `id = <n>` in the
    /// project file (below [`RESERVED_BASE`]), else the hash of the key (as in 1.3.0).
    pub id: u32,
    /// `id = <n>` was written (the ID does not follow the key).
    pub explicit_id: bool,
    /// `initial = <v>`: the value of a new instance that has no saved state.
    /// The default the host is told stays `default`. `None` = `default`.
    pub initial: Option<f64>,
    /// `scale = ...`: how the plain value maps to the normalized value hosts see.
    pub scale: Scale,
    /// `text = "custom"`: the plugin writes and reads this parameter's text
    /// (`Process::format_param_text` / `parse_param_text`).
    pub custom_text: bool,
    /// `display = "db"` (or `format = "%.1f dB"`): a built-in display and text input; the unit
    /// is part of the text and hosts are told an empty one.
    pub display: Option<DisplaySpec>,
    /// `smooth = "20ms"` or `{ time = "10ms", kind = "log" }` (float only): the
    /// framework ramps the value the plugin reads towards the host's value.
    pub smooth: Option<Smooth>,
    /// Set for the parameters a repeated group expanded into.
    pub group: Option<GroupRef>,
}

/// Where a parameter came from in a repeated group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupRef {
    /// Index into [`Manifest::groups`].
    pub group: usize,
    /// The copy, from 0 (it is `{n}` = `start + instance`).
    pub instance: usize,
    /// The field, in the order written.
    pub field: usize,
}

/// A repeated parameter group, `[params.<group>] repeat = N`. Its parameters sit in
/// `Manifest::params` from index `first`, copy after copy: the parameter of copy `i`,
/// field `f` is `params[first + i * fields.len() + f]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// The group's name; the generated `Params` has a field of this name, an array.
    pub name: String,
    pub repeat: usize,
    /// The value of `{n}` in the first copy.
    pub start: i64,
    pub first: usize,
    /// Field names (the parameter key without the group's key prefix).
    pub fields: Vec<String>,
}

/// How a smoothed parameter reaches a new value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Smooth {
    pub seconds: f64,
    /// 0 linear (a straight line in plain units, arrives after `seconds`),
    /// 1 log (a straight line in the ratio; frequencies), 2 exp (one pole).
    pub kind: u8,
}

/// How a float parameter's plain value maps to its normalized value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    Linear,
    Log,
    Skew { center: f64 },
    /// `Process::param_normalize` / `param_denormalize`.
    Custom,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Float { min: f64, max: f64 },
    Int { min: i64, max: i64 },
    Bool,
    Enum { choices: Vec<String> },
}

/// What process isolation carries: `MAX_PARAMS` in ferritron-core's isolation/shm.rs,
/// less the built-in parameters (bypass, AI Access, Quality, Program).
const ISOLATION_MAX_PARAMS: usize = 1024 - 4;

pub fn parse(text: &str) -> Result<Manifest, Error> {
    let root: Table = text.parse().map_err(|e: toml::de::Error| syntax_error(text, &e))?;
    reject_unknown(&root, "", &["plugin", "params", "view", "isolation", "mcp", "quality", "ara", "au", "sdk", "vst3", "clap", "audio", "midi", "vst2", "process", "streams", "presets", "companion", "bundle"])?;
    bundle_settings_of(&root)?;
    let vst2_id = match root.get("vst2") {
        None => None,
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[vst2]"))?;
            reject_unknown(t, "vst2.", &["unique_id"])?;
            match t.get("unique_id") {
                None => None,
                Some(v) => {
                    let text = v.as_str().filter(|s| s.len() == 4 && s.is_ascii()).ok_or_else(|| {
                        error!("bad-vst2-id", "{$at} must be exactly 4 ASCII characters, such as \"Gain\"", at = "vst2.unique_id")
                    })?;
                    Some(u32::from_be_bytes(text.as_bytes().try_into().unwrap()))
                }
            }
        }
    };
    let mut audio = parse_audio(&root)?;
    if let Some(v) = root.get("midi") {
        let t = v.as_table().ok_or_else(|| not_table("[midi]"))?;
        reject_unknown(t, "midi.", &["vst3_cc"])?;
        audio.host_cc = match t.get("vst3_cc") {
            None => false,
            Some(Value::String(s)) if s == "events" => false,
            Some(Value::String(s)) if s == "host" => true,
            Some(v) => return Err(not_one_of("midi.vst3_cc", &v.to_string(), "events, host")),
        };
    }
    let (vst3_class_id, vst3_subcategories, vst3_controller_id, vst3_compat) = match root.get("vst3") {
        None => (None, None, None, None),
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[vst3]"))?;
            reject_unknown(t, "vst3.", &["class_id", "subcategories", "controller_id", "compatibility"])?;
            let guid = |name: &str| -> Result<Option<[u8; 16]>, Error> {
                match t.get(name) {
                    None => Ok(None),
                    Some(v) => {
                        let at = format!("vst3.{name}");
                        let text = v.as_str().ok_or_else(|| not_string(&at))?;
                        Ok(Some(parse_guid(text).ok_or_else(|| bad_guid(&at))?))
                    }
                }
            };
            let class_id = guid("class_id")?;
            let controller_id = guid("controller_id")?;
            let compat = match t.get("compatibility") {
                None => None,
                Some(v) => {
                    let c = v.as_table().ok_or_else(|| not_table("vst3.compatibility"))?;
                    reject_unknown(c, "vst3.compatibility.", &["id", "replaces"])?;
                    let id = match c.get("id") {
                        None => return Err(required("vst3.compatibility.id")),
                        Some(v) => parse_guid(v.as_str().ok_or_else(|| not_string("vst3.compatibility.id"))?).ok_or_else(|| bad_guid("vst3.compatibility.id"))?,
                    };
                    let replaces = match c.get("replaces") {
                        None => Vec::new(),
                        Some(v) => v
                            .as_array()
                            .ok_or_else(|| error!("not-string-list", "{$at} must be a list of strings", at = "vst3.compatibility.replaces"))?
                            .iter()
                            .map(|x| {
                                x.as_str()
                                    .and_then(parse_guid)
                                    .ok_or_else(|| bad_guid("vst3.compatibility.replaces"))
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    };
                    Some(Vst3Compat { id, replaces })
                }
            };
            // The component, the controller and the compatibility class are three classes of one factory.
            let named: Vec<(&str, [u8; 16])> = [
                ("vst3.class_id", class_id),
                ("vst3.controller_id", controller_id),
                ("vst3.compatibility.id", compat.as_ref().map(|c| c.id)),
            ]
            .into_iter()
            .filter_map(|(n, g)| g.map(|g| (n, g)))
            .collect();
            for (i, (a, ga)) in named.iter().enumerate() {
                if let Some((b, _)) = named[i + 1..].iter().find(|(_, gb)| gb == ga) {
                    return Err(error!("vst3-id-duplicate", "{$a} and {$b} are the same GUID; every class of the plugin needs its own", a = a, b = b));
                }
            }
            let sub = match t.get("subcategories") {
                None => None,
                Some(v) => Some(v.as_str().ok_or_else(|| not_string("vst3.subcategories"))?.to_string()),
            };
            (class_id, sub, controller_id, compat)
        }
    };
    let clap_features = match root.get("clap") {
        None => None,
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[clap]"))?;
            reject_unknown(t, "clap.", &["features"])?;
            match t.get("features") {
                None => None,
                Some(v) => Some(
                    v.as_array()
                        .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
                        .ok_or_else(|| error!("not-string-list", "{$at} must be a list of strings", at = "clap.features"))?,
                ),
            }
        }
    };
    let mut sdk = std::collections::BTreeMap::new();
    if let Some(v) = root.get("sdk") {
        let t = v.as_table().ok_or_else(|| not_table("[sdk]"))?;
        reject_unknown(t, "sdk.", &SDK_IDS)?;
        for (id, version) in t {
            let version = version.as_str().filter(|v| !v.trim().is_empty()).ok_or_else(|| not_string(&format!("sdk.{id}")))?;
            sdk.insert(id.clone(), version.trim().to_string());
        }
    }
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
    let (automation, control_block) = match root.get("process") {
        None => (0u8, 32u32),
        Some(v) => {
            let t = v.as_table().ok_or_else(|| not_table("[process]"))?;
            reject_unknown(t, "process.", &["automation", "control_block"])?;
            let automation = match t.get("automation") {
                None => 0,
                Some(v) => match v.as_str().ok_or_else(|| not_string("process.automation"))? {
                    "control" => 0,
                    "sample" => 1,
                    "events" => 2,
                    other => return Err(not_one_of("process.automation", other, "control, sample, events")),
                },
            };
            let block = match t.get("control_block") {
                None => 32,
                Some(v) => match v.as_integer() {
                    Some(n) if (1..=4096).contains(&n) => n as u32,
                    _ => return Err(error!("bad-control-block", "process.control_block must be an integer from 1 to 4096")),
                },
            };
            (automation, block)
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
    reject_unknown(plugin, "plugin.", &["id", "name", "kind", "vendor", "version", "url", "email", "parallel", "categories", "midi_input"])?;
    let midi_input = match plugin.get("midi_input") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| not_bool("plugin.midi_input"))?,
    };
    let categories = match plugin.get("categories") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
            .ok_or_else(|| error!("not-string-list", "{$at} must be a list of strings", at = "plugin.categories"))?,
    };
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
    let mut groups = Vec::new();
    if let Some(table) = root.get("params") {
        let table = table.as_table().ok_or_else(|| not_table("[params]"))?;
        for (key, value) in table {
            match value.as_table() {
                // `repeat` or `fields` (no parameter has either) makes it a group.
                Some(t) if t.contains_key("repeat") || t.contains_key("fields") => group::parse_group(key, t, &mut groups, &mut params)?,
                _ => params.push(parse_param(key, value)?),
            }
        }
    }
    check_ids(&params, &groups)?;
    // `params` holds the parameters after `repeat` is expanded: one per parameter the host sees.
    if isolate && params.len() > ISOLATION_MAX_PARAMS {
        return Err(error!(
            "isolation-too-many-params",
            "[isolation] mode = \"process\" carries at most {$max} parameters; this plugin has {$count}",
            max = ISOLATION_MAX_PARAMS,
            count = params.len()
        ));
    }
    let presets = presets::parse_presets(&root, &params)?;

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
        automation,
        control_block,
        isolate,
        isolate_per_instance,
        isolate_pipeline,
        ara,
        mcp,
        quality,
        view: root.contains_key("view"),
        name: string("name", None)?,
        vendor,
        version: string("version", Some(std::env::var("CARGO_PKG_VERSION").unwrap_or_default()))?,
        url,
        email: string("email", Some(String::new()))?,
        id,
        params,
        groups,
        midi_input,
        streams: streams::parse_streams(&root)?,
        presets,
        audio,
        vst2_id,
        vst3_class_id,
        vst3_controller_id,
        vst3_compat,
        categories,
        vst3_subcategories,
        clap_features,
        au,
        sdk,
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
    reject_unknown(t, &format!("{at}."), &["type", "name", "unit", "range", "default", "choices", "automatable", "hidden", "poly", "id", "initial", "scale", "text", "smooth", "display", "format", "digits"])?;
    let (id, explicit_id) = match t.get("id") {
        None => (package::param_id(key), false),
        Some(v) => {
            let n = v.as_integer().filter(|n| (0..i64::from(RESERVED_BASE)).contains(n)).ok_or_else(|| {
                error!(
                    "bad-param-id",
                    "{$at} must be an integer from 0 to {$max} (IDs from {$base} up are Ferritron's)",
                    at = format!("{at}.id"),
                    max = RESERVED_BASE - 1,
                    base = format!("0x{RESERVED_BASE:08X}")
                )
            })?;
            (n as u32, true)
        }
    };
    let flag = |name: &str, default: bool| -> Result<bool, Error> {
        match t.get(name) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| not_bool(&format!("{at}.{name}"))),
        }
    };
    let automatable = flag("automatable", true)?;
    let hidden = flag("hidden", false)?;
    let poly = flag("poly", false)?;
    if poly && t.get("type").and_then(Value::as_str) != Some("float") {
        return Err(error!("poly-not-float", "{$at}.poly: only float parameters can be modulated per note", at = at));
    }

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
    let initial = match t.get("initial") {
        None => None,
        Some(v) => {
            let value = match (&kind, v) {
                (Kind::Float { .. }, v) => v.as_float_lossy().ok_or_else(|| error!("not-number", "{$at} must be a number", at = format!("{at}.initial")))?,
                (Kind::Int { .. }, v) => v.as_integer().ok_or_else(|| error!("not-integer", "{$at} must be an integer", at = format!("{at}.initial")))? as f64,
                (Kind::Bool, v) => {
                    if v.as_bool().ok_or_else(|| not_bool(&format!("{at}.initial")))? {
                        1.0
                    } else {
                        0.0
                    }
                }
                (Kind::Enum { choices }, Value::String(s)) => choices.iter().position(|c| c == s).ok_or_else(|| {
                    error!("default-not-a-choice", "{$at}: \"{$value}\" is not one of the choices", at = format!("{at}.initial"), value = s)
                })? as f64,
                (Kind::Enum { .. }, _) => {
                    return Err(error!("default-not-a-choice-name", "{$at} must be one of the choices", at = format!("{at}.initial")));
                }
            };
            if matches!(kind, Kind::Float { .. } | Kind::Int { .. }) && !(lo..=hi).contains(&value) {
                return Err(error!("default-out-of-range", "{$at} must be inside the range", at = format!("{at}.initial")));
            }
            Some(value)
        }
    };
    let scale = match t.get("scale") {
        None => Scale::Linear,
        Some(v) => {
            let scale = match v {
                Value::String(s) => match s.as_str() {
                    "linear" => Scale::Linear,
                    "log" => Scale::Log,
                    "custom" => Scale::Custom,
                    other => return Err(not_one_of(&format!("{at}.scale"), other, "linear, log, custom, { skew_center = <value> }")),
                },
                Value::Table(st) => {
                    reject_unknown(st, &format!("{at}.scale."), &["skew_center"])?;
                    let center = st.get("skew_center").and_then(Value::as_float_lossy).ok_or_else(|| required(&format!("{at}.scale.skew_center")))?;
                    Scale::Skew { center }
                }
                _ => return Err(not_one_of(&format!("{at}.scale"), &v.to_string(), "linear, log, custom, { skew_center = <value> }")),
            };
            match (&kind, scale) {
                (Kind::Float { .. }, _) | (_, Scale::Linear) => {}
                _ => return Err(error!("scale-needs-float", "{$at}: only float parameters can have a scale", at = format!("{at}.scale"))),
            }
            if let Kind::Float { min, max } = &kind {
                match scale {
                    Scale::Log if *min <= 0.0 => {
                        return Err(error!("log-needs-positive", "{$at}: a log scale needs a range above 0", at = format!("{at}.range")));
                    }
                    Scale::Skew { center } if !(*min < center && center < *max) => {
                        return Err(error!("skew-center-outside", "{$at} must be inside the range", at = format!("{at}.scale.skew_center")));
                    }
                    _ => {}
                }
            }
            scale
        }
    };
    let custom_text = match t.get("text") {
        None => false,
        Some(Value::String(s)) if s == "custom" => true,
        Some(Value::String(s)) if s == "default" => false,
        Some(v) => return Err(not_one_of(&format!("{at}.text"), &v.to_string(), "default, custom")),
    };
    let display = parse_display(t, &at, &kind, custom_text)?;
    let smooth = match t.get("smooth") {
        None => None,
        Some(v) => {
            if !matches!(kind, Kind::Float { .. }) {
                return Err(error!("smooth-needs-float", "{$at} is for float parameters only", at = format!("{at}.smooth")));
            }
            let (time, kind_name) = match v {
                Value::Table(tt) => {
                    reject_unknown(tt, &format!("{at}.smooth."), &["time", "kind"])?;
                    (tt.get("time"), tt.get("kind").and_then(Value::as_str).unwrap_or("linear"))
                }
                other => (Some(other), "linear"),
            };
            let seconds = match time {
                Some(Value::String(text)) => parse_duration(text),
                Some(n) => n.as_float_lossy().map(|ms| ms / 1000.0),
                None => None,
            }
            .filter(|s| *s > 0.0 && *s <= 10.0)
            .ok_or_else(|| error!("bad-smooth-time", "{$at} must be a time such as \"20ms\" or \"0.5s\" (up to 10 s)", at = format!("{at}.smooth")))?;
            let kind = match kind_name {
                "linear" => 0,
                "log" => 1,
                "exp" => 2,
                other => return Err(not_one_of(&format!("{at}.smooth.kind"), other, "linear, log, exp")),
            };
            Some(Smooth { seconds, kind })
        }
    };
    Ok(Param { key: key.to_string(), name, unit, kind, default, automatable, hidden, poly, id, explicit_id, initial, scale, custom_text, display, smooth, group: None })
}

/// The built-in display of a parameter (`display = ...` or `format = ...`).
#[derive(Clone, Debug, PartialEq)]
pub struct DisplaySpec {
    pub kind: DisplayKind,
    /// `digits = N`: decimals of `"db"`, significant digits of the others. 0 = the format's default.
    pub digits: u8,
}

/// The built-in display formats (see `ferritron::display`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    Db,
    Hz,
    Percent,
    Ms,
    Note,
    Ratio,
    /// `format = "%.1f dB"`
    Template(String),
}

/// The names `display` accepts.
pub const DISPLAYS: &str = "db, hz, percent, ms, note, ratio";

fn parse_display(t: &Table, at: &str, kind: &Kind, custom_text: bool) -> Result<Option<DisplaySpec>, Error> {
    let display = t.get("display");
    let format = t.get("format");
    if display.is_none() && format.is_none() {
        if t.contains_key("digits") {
            return Err(error!("digits-needs-display", "{$at} needs display or format", at = format!("{at}.digits")));
        }
        return Ok(None);
    }
    if display.is_some() && format.is_some() {
        return Err(error!("display-and-format", "{$at}: give display or format, not both", at = at));
    }
    if custom_text {
        return Err(error!("display-and-custom-text", "{$at}: display / format and text = \"custom\" cannot be used together", at = at));
    }
    if !matches!(kind, Kind::Float { .. } | Kind::Int { .. }) {
        return Err(error!("display-needs-number", "{$at} is for float and int parameters only", at = format!("{at}.display")));
    }
    let digits = match t.get("digits") {
        None => 0,
        Some(v) => match v.as_integer() {
            Some(n) if (0..=12).contains(&n) => n as u8,
            _ => return Err(error!("bad-digits", "{$at} must be an integer from 0 to 12", at = format!("{at}.digits"))),
        },
    };
    let kind = match (display, format) {
        (Some(v), _) => match v.as_str().ok_or_else(|| not_string(&format!("{at}.display")))? {
            "db" => DisplayKind::Db,
            "hz" => DisplayKind::Hz,
            "percent" => DisplayKind::Percent,
            "ms" => DisplayKind::Ms,
            "note" => DisplayKind::Note,
            "ratio" => DisplayKind::Ratio,
            other => return Err(not_one_of(&format!("{at}.display"), other, DISPLAYS)),
        },
        (None, Some(v)) => {
            let s = v.as_str().ok_or_else(|| not_string(&format!("{at}.format")))?;
            if !valid_template(s) {
                return Err(error!("bad-format", "{$at} must contain exactly one %.Nf, %f or %d (and %% for a percent sign)", at = format!("{at}.format")));
            }
            DisplayKind::Template(s.to_string())
        }
        (None, None) => unreachable!("checked above"),
    };
    Ok(Some(DisplaySpec { kind, digits }))
}

/// `%.Nf`, `%f` or `%d` exactly once; `%%` is a percent sign. (The same rule as `ferritron::display`.)
fn valid_template(s: &str) -> bool {
    let b = s.as_bytes();
    let (mut i, mut numbers) = (0, 0);
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        match b.get(i) {
            Some(b'%') => {}
            Some(b'd') | Some(b'f') => numbers += 1,
            Some(b'.') => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == start || b.get(j) != Some(&b'f') {
                    return false;
                }
                numbers += 1;
                i = j;
            }
            _ => return false,
        }
        i += 1;
    }
    numbers == 1
}

/// IDs must be unique, given or hashed, and must not equal one of Ferritron's
/// reserved IDs. (Hashed IDs are 31 bit, so half of them are numerically at or
/// above [`RESERVED_BASE`]; they stay as in 1.3.0 so saved projects keep
/// working. Only an exact clash with a reserved ID is an error.)
fn check_ids(params: &[Param], groups: &[Group]) -> Result<(), Error> {
    const RESERVED: [&str; 8] = [
        "ferritron.bypass",
        "ferritron.quality",
        "ferritron.mcp",
        "ferritron.instance.high",
        "ferritron.instance.low",
        "ferritron.editor.width",
        "ferritron.editor.height",
        "ferritron.program",
    ];
    // VST3 carries the MIDI controllers to the plugin as hidden parameters here: 16 channels x 256
    // numbers (`kMidiParamBase` in the VST3 shim). A parameter in this range would be read as one.
    const MIDI_CONTROLLERS: std::ops::Range<u32> = 0x4000_1000..0x4000_2000;
    let reserved: Vec<u32> = RESERVED.iter().map(|k| reserved_id(k)).collect();
    // Hash maps: 951 parameters (or more) must not cost a quadratic scan.
    let mut keys = std::collections::HashMap::with_capacity(params.len());
    let mut ids = std::collections::HashMap::with_capacity(params.len());
    for p in params {
        if reserved.contains(&p.id) || MIDI_CONTROLLERS.contains(&p.id) {
            return Err(error!("id-reserved", "{$key}: its ID {$id} is one Ferritron keeps for itself; give it an `id = ...` below 0x40000000", key = p.key, id = p.id));
        }
        if keys.insert(p.key.as_str(), ()).is_some() {
            return Err(error!("key-duplicate", "the parameter key {$key} is made twice (check the `key` of the repeated groups)", key = p.key));
        }
        if let Some(q) = ids.insert(p.id, p.key.as_str()) {
            return Err(error!(
                "id-duplicate",
                "{$a} and {$b} have the same parameter ID ({$id}); give one an explicit `id = ...`",
                a = q,
                b = p.key,
                id = p.id
            ));
        }
    }
    // The generated `Params` has one array field per group, named like the group.
    for g in groups {
        if keys.contains_key(g.name.as_str()) || groups.iter().filter(|h| h.name == g.name).count() > 1 {
            return Err(error!("group-name-clash", "the group {$name} has the same name as a parameter or another group", name = g.name));
        }
    }
    Ok(())
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

/// `[bundle]`: what `cargo ferritron bundle` does after packaging. Read by the tool, not built
/// into the plugin (so it is not part of [`Manifest`]).
///
/// ```toml
/// [bundle]
/// install = true          # copy each format to its install folder every time (as `--install`)
/// shortcuts = true        # shortcuts to each format's install folder in target/bundled
/// shortcut_prefix = "_"   # the start of each shortcut's name (default "_")
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSettings {
    /// `install`: after every bundle for this computer, copy each format to its install folder.
    pub install: bool,
    /// `shortcuts`: put a shortcut to each format's install folder next to the bundles
    /// (`<prefix><format> Plugins`: a `.lnk` on Windows, a symbolic link elsewhere).
    pub shortcuts: bool,
    /// `shortcut_prefix`: the start of each shortcut's name.
    pub shortcut_prefix: String,
}

impl Default for BundleSettings {
    fn default() -> Self {
        Self { install: false, shortcuts: false, shortcut_prefix: "_".into() }
    }
}

/// The `[bundle]` section of a project file (defaults when it has none).
pub fn bundle_settings(text: &str) -> Result<BundleSettings, Error> {
    let root: Table = text.parse().map_err(|e: toml::de::Error| syntax_error(text, &e))?;
    bundle_settings_of(&root)
}

fn bundle_settings_of(root: &Table) -> Result<BundleSettings, Error> {
    let mut out = BundleSettings::default();
    let Some(v) = root.get("bundle") else { return Ok(out) };
    let t = v.as_table().ok_or_else(|| not_table("[bundle]"))?;
    reject_unknown(t, "bundle.", &["install", "shortcuts", "shortcut_prefix"])?;
    if let Some(v) = t.get("install") {
        out.install = v.as_bool().ok_or_else(|| not_bool("bundle.install"))?;
    }
    if let Some(v) = t.get("shortcuts") {
        out.shortcuts = v.as_bool().ok_or_else(|| not_bool("bundle.shortcuts"))?;
    }
    if let Some(v) = t.get("shortcut_prefix") {
        let prefix = v.as_str().ok_or_else(|| not_string("bundle.shortcut_prefix"))?;
        if prefix.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c)) {
            return Err(error!("bad-file-name-part", "{$at} cannot contain < > : \" / \\ | ? * or control characters", at = "bundle.shortcut_prefix"));
        }
        out.shortcut_prefix = prefix.to_string();
    }
    Ok(out)
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
fn bad_guid(at: &str) -> Error {
    error!("bad-guid", "{$at} must be a GUID such as \"3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11\"", at = at)
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
        // Ferritron 2.0 does not read .frtx (before 1.2) any more: stop and name the migration command.
        let old = locate(&dir).unwrap_err();
        assert_eq!(old.code, "frtx-not-read");
        let text = old.to_string();
        assert!(text.contains("cargo ferritron migrate-frtx") && text.contains("gain.frtx") && text.contains("gain.ferritroject"), "{text}");
        assert!(text.contains("Ferritron 2.0") && text.contains("古い拡張子"), "English and Japanese: {text}");
        assert_eq!(locate_any(&dir).unwrap(), dir.join("gain.frtx"), "tools that rename or lock old projects still find it");
        std::fs::remove_file(dir.join("gain.frtx")).unwrap();
        std::fs::write(dir.join("gain.ferritroject"), GAIN).unwrap();
        assert_eq!(locate(&dir).unwrap(), dir.join("gain.ferritroject"));
        // Both extensions at once: the build would not know which.
        std::fs::write(dir.join("gain.frtx"), GAIN).unwrap();
        let both = locate(&dir).unwrap_err().to_string();
        assert!(both.contains("several project files") && both.contains("gain.ferritroject") && both.contains("gain.frtx"), "{both}");
        std::fs::remove_file(dir.join("gain.frtx")).unwrap();
        std::fs::write(dir.join("other.ferritroject"), GAIN).unwrap();
        assert!(locate(&dir).unwrap_err().to_string().contains("several project files"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn migrate_frtx_renames_the_file() {
        let dir = std::env::temp_dir().join(format!("ferritron-manifest-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(migrate_frtx(&dir).unwrap_err().code, "no-frtx-file");
        std::fs::write(dir.join("gain.ferritroject"), GAIN).unwrap();
        assert_eq!(migrate_frtx(&dir).unwrap_err().code, "no-frtx-file", "nothing to do");
        std::fs::write(dir.join("gain.frtx"), GAIN).unwrap();
        assert_eq!(migrate_frtx(&dir).unwrap_err().code, "frtx-target-exists", "never overwrites");
        assert!(dir.join("gain.frtx").exists());
        std::fs::remove_file(dir.join("gain.ferritroject")).unwrap();
        let (old, new) = migrate_frtx(&dir).unwrap();
        assert_eq!((old, new.clone()), (dir.join("gain.frtx"), dir.join("gain.ferritroject")));
        assert!(!dir.join("gain.frtx").exists());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), GAIN, "same content");
        assert_eq!(locate(&dir).unwrap(), new);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sdk_versions_may_be_pinned() {
        let m = parse(&format!("{GAIN}
[sdk]
vst3 = \"3.8.1\"
clap = \"1.2.10\"")).unwrap();
        assert_eq!(m.sdk.get("vst3").map(String::as_str), Some("3.8.1"));
        assert_eq!(m.sdk.len(), 2);
        assert!(parse(GAIN).unwrap().sdk.is_empty());
        assert_eq!(parse(&format!("{GAIN}
[sdk]
vst9 = \"1\"")).err().unwrap().code, "unknown-key");
        assert_eq!(parse(&format!("{GAIN}
[sdk]
vst3 = 3")).err().unwrap().code, "not-string");
    }

    #[test]
    fn parameters_may_be_hidden_or_not_automatable() {
        let text = format!("{GAIN}").replace("[params]", "[params]
link = { type = \"int\", range = [0, 100], automatable = false, hidden = true }");
        let m = parse(&text).unwrap();
        let link = m.params.iter().find(|p| p.key == "link").unwrap();
        assert!(!link.automatable && link.hidden);
        assert!(m.params.iter().filter(|p| p.key != "link").all(|p| p.automatable && !p.hidden), "defaults");
        let bad = text.replace("hidden = true", "hidden = \"yes\"");
        assert_eq!(parse(&bad).err().unwrap().code, "not-bool");
    }

    #[test]
    fn ids_may_be_given_and_are_checked() {
        let with = |extra: &str| GAIN.replace("[params]", &format!("[params]\n{extra}"));
        let m = parse(&with("a = { type = \"bool\", id = 7 }\nb = { type = \"bool\", id = 0 }")).unwrap();
        assert_eq!((m.params[0].id, m.params[0].explicit_id), (7, true));
        assert_eq!(m.params.iter().find(|p| p.key == "a").unwrap().id, 7);
        // Without `id` the hash of 1.3.0 stays.
        let plain = parse(GAIN).unwrap();
        assert_eq!(plain.params[0].id, param_id("gain"));
        assert!(!plain.params[0].explicit_id);
        let code = |src: String| parse(&src).err().unwrap().code;
        assert_eq!(code(with("a = { type = \"bool\", id = 7 }\nb = { type = \"bool\", id = 7 }")), "id-duplicate");
        assert_eq!(code(with("a = { type = \"bool\", id = 1073741824 }")), "bad-param-id");
        assert_eq!(code(with("a = { type = \"bool\", id = -1 }")), "bad-param-id");
        assert_eq!(code(with("a = { type = \"bool\", id = \"x\" }")), "bad-param-id");
    }

    #[test]
    fn display_and_format_are_parsed_and_checked() {
        let src = |params: &str| format!("[plugin]
id = \"a.b\"
name = \"X\"
[params]
{params}");
        let m = parse(&src("cut = { type = \"float\", range = [20, 20000], display = \"hz\" }
level = { type = \"float\", range = [-60, 6], display = \"db\", digits = 2 }
mix = { type = \"float\", range = [0, 1], format = \"%.0f%%\" }
plain = { type = \"float\", range = [0, 1] }")).unwrap();
        let d = |k: &str| m.params.iter().find(|p| p.key == k).unwrap().display.clone();
        assert_eq!(d("cut"), Some(DisplaySpec { kind: DisplayKind::Hz, digits: 0 }));
        assert_eq!(d("level"), Some(DisplaySpec { kind: DisplayKind::Db, digits: 2 }));
        assert_eq!(d("mix"), Some(DisplaySpec { kind: DisplayKind::Template("%.0f%%".into()), digits: 0 }));
        assert_eq!(d("plain"), None);
        let code = |params: &str| parse(&src(params)).err().map(|e| e.code);
        assert_eq!(code("x = { type = \"float\", range = [0, 1], display = \"loud\" }"), Some("not-one-of".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], display = \"db\", format = \"%d\" }"), Some("display-and-format".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], display = \"db\", text = \"custom\" }"), Some("display-and-custom-text".into()));
        assert_eq!(code("x = { type = \"bool\", display = \"db\" }"), Some("display-needs-number".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], format = \"no number\" }"), Some("bad-format".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], format = \"%d %d\" }"), Some("bad-format".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], digits = 2 }"), Some("digits-needs-display".into()));
        assert_eq!(code("x = { type = \"float\", range = [0, 1], display = \"db\", digits = 99 }"), Some("bad-digits".into()));
    }

    #[test]
    fn scales_and_custom_text() {
        let m = parse(&GAIN.replace("gain = { type = \"float\", range = [-60, 12], default = 0, unit = \"dB\" }",
            "gain = { type = \"float\", range = [-60, 12], default = 0, scale = { skew_center = 0 }, text = \"custom\" }
f = { type = \"float\", range = [10, 30000], default = 1000, scale = \"log\" }
c = { type = \"float\", range = [0, 1], scale = \"custom\" }")).unwrap();
        assert_eq!(m.params.iter().find(|p| p.key == "gain").map(|p| (p.scale, p.custom_text)), Some((Scale::Skew { center: 0.0 }, true)));
        assert_eq!(m.params.iter().find(|p| p.key == "f").unwrap().scale, Scale::Log);
        assert_eq!(m.params.iter().find(|p| p.key == "c").unwrap().scale, Scale::Custom);
        let code = |extra: &str| parse(&GAIN.replace("[params]", &format!("[params]
{extra}"))).err().unwrap().code;
        assert_eq!(code("a = { type = \"float\", range = [0, 1], scale = \"log\" }"), "log-needs-positive");
        assert_eq!(code("a = { type = \"float\", range = [0, 1], scale = { skew_center = 3 } }"), "skew-center-outside");
        assert_eq!(code("a = { type = \"bool\", scale = \"log\" }"), "scale-needs-float");
        assert_eq!(code("a = { type = \"float\", range = [1, 2], scale = \"zigzag\" }"), "not-one-of");
    }

    #[test]
    fn audio_buses_and_midi_input() {
        let d = parse(GAIN).unwrap();
        assert_eq!((d.audio.min_channels, d.audio.max_channels, d.audio.sidechain, d.midi_input), (1, 2, None, false));
        let m = parse(&format!("{GAIN}\n[audio]\nchannels = {{ min = 1, max = 32 }}\nsidechain = {{ channels = \"main\" }}")).unwrap();
        assert_eq!((m.audio.max_channels, m.audio.sidechain), (32, Some(0)));
        let m = parse(&format!("{GAIN}\n[audio]\nlayouts = [\"stereo\", \"5.1\", \"7.1.4\"]\nsidechain = {{ channels = 2 }}\nunsupported = \"passthrough\"")).unwrap();
        assert_eq!((m.audio.min_channels, m.audio.max_channels, m.audio.sidechain, m.audio.passthrough), (2, 12, Some(2), true));
        let code = |t: &str| parse(&format!("{GAIN}\n[audio]\n{t}")).err().unwrap().code;
        assert_eq!(code("channels = { min = 3, max = 2 }"), "channels-min-max");
        assert_eq!(code("channels = { max = 64 }"), "bad-channels");
        assert_eq!(code("layouts = [\"hexa\"]"), "not-one-of");
        assert!(parse(&GAIN.replace("name = \"Gain\"", "name = \"Gain\"\nmidi_input = true")).unwrap().midi_input);
        // LV2 keeps the 1.3.0 ports for plugins that use nothing new; the MIDI output is always the last port.
        use Lv2Port::*;
        assert_eq!(parse(GAIN).unwrap().lv2_ports(), [In(0), In(1), Out(0), Out(1), Latency, Freewheel, MidiOut]);
        let inst = parse(&GAIN.replace("name = \"Gain\"", "name = \"Gain\"
kind = \"instrument\"")).unwrap();
        assert_eq!(inst.lv2_ports(), [Out(0), Out(1), Midi, Latency, Freewheel, MidiOut]);
        // The rendered Turtle declares the MIDI output after the parameters, as the last port.
        let gain = parse(GAIN).unwrap();
        let ttl = render(include_str!("../../ferritron-lv2/lv2/plugin.ttl"), &gain.template_scope(), Escape::None).unwrap();
        let symbol = ttl.find("lv2:symbol \"midi_out\"").expect("midi_out port");
        let block = &ttl[ttl[..symbol].rfind('[').unwrap()..];
        let block = &block[..block.find(']').unwrap()];
        let index = gain.host_params().len() + 6; // after the 6 ports above
        assert!(block.contains(&format!("lv2:index {index} ;")), "{block}");
        for want in ["a lv2:OutputPort", "atom:AtomPort", "atom:bufferType atom:Sequence", "atom:supports midi:MidiEvent", "lv2:name \"MIDI Out\""] {
            assert!(block.contains(want), "{want} missing: {block}");
        }
        assert!(!block.contains("lv2:InputPort") && !block.contains("lv2:designation"), "{block}");
        assert!(ttl.trim_end().ends_with("] ."), "the MIDI output is the last port");
        assert!(!parse(GAIN).unwrap().audio.host_cc);
        assert!(parse(&format!("{GAIN}\n[midi]\nvst3_cc = \"host\"")).unwrap().audio.host_cc);
        assert_eq!(parse(&format!("{GAIN}\n[midi]\nvst3_cc = \"x\"")).err().unwrap().code, "not-one-of");
    }

    #[test]
    fn vst2_id_is_four_characters() {
        assert_eq!(parse(GAIN).unwrap().vst2_id, None);
        assert_eq!(parse(&format!("{GAIN}\n[vst2]\nunique_id = \"Gain\"")).unwrap().vst2_id, Some(u32::from_be_bytes(*b"Gain")));
        assert_eq!(parse(&format!("{GAIN}\n[vst2]\nunique_id = \"Gai\"")).err().unwrap().code, "bad-vst2-id");
    }

    #[test]
    fn program_and_midi_controller_ids_are_reserved() {
        // An explicit `id = 0x40001005` never gets this far (see `bad-param-id`), so the check is
        // exercised on a parameter whose ID comes from the hash, as a real clash would.
        assert_eq!(parse(&GAIN.replace("default = 0", "default = 0, id = 0x40001005")).err().unwrap().code, "bad-param-id");
        let gain = parse(GAIN).unwrap().params[0].clone();
        for id in [0x4000_1000, 0x4000_1005, 0x4000_1FFF, reserved_id("ferritron.program")] {
            let p = Param { id, ..gain.clone() };
            assert_eq!(check_ids(std::slice::from_ref(&p), &[]).err().unwrap().code, "id-reserved", "id {id:#x}");
        }
        let after = Param { id: 0x4000_2000, ..gain };
        assert!(check_ids(std::slice::from_ref(&after), &[]).is_ok());
    }

    #[test]
    fn reserved_ids_are_fixed_and_high() {
        assert_eq!(reserved_id("ferritron.bypass"), RESERVED_BASE);
        assert!(["ferritron.quality", "ferritron.mcp", "ferritron.instance.high", "ferritron.editor.width"].iter().all(|k| reserved_id(k) > RESERVED_BASE));
        assert_eq!(reserved_id("gain"), param_id("gain"));
        let m = parse(GAIN).unwrap();
        let bypass = m.host_params().into_iter().find(|p| p.bypass).unwrap();
        assert_eq!(bypass.id, RESERVED_BASE);
    }

    #[test]
    fn initial_values_class_ids_and_categories() {
        let m = parse(&GAIN.replace("default = 0", "default = 0, initial = -1")).unwrap();
        assert_eq!((m.params[0].default, m.params[0].initial), (0.0, Some(-1.0)));
        assert_eq!(parse(&GAIN.replace("default = 0", "default = 0, initial = 99")).err().unwrap().code, "default-out-of-range");
        let m = parse(&GAIN.replace("default = \"Warm\"", "default = \"Warm\", initial = \"Clean\"")).unwrap();
        assert_eq!(m.params[1].initial, Some(0.0));
        assert!(parse(GAIN).unwrap().params.iter().all(|p| p.initial.is_none()));

        let m = parse(&format!("{GAIN}\n[vst3]\nclass_id = \"3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11\"")).unwrap();
        assert_eq!(m.vst3_class_id.unwrap()[..4], [0x3E, 0x6B, 0x2A, 0x90]);
        assert_eq!(parse(&format!("{GAIN}\n[vst3]\nclass_id = \"nope\"")).err().unwrap().code, "bad-guid");
        assert!(parse(GAIN).unwrap().vst3_class_id.is_none());

        let cat = |c: &str| parse(&GAIN.replace("name = \"Gain\"", &format!("name = \"Gain\"\ncategories = {c}"))).unwrap();
        let m = cat("[\"EQ\", \"Analyzer\", \"Dynamics\"]");
        assert_eq!(m.vst3_sub_categories(), "Fx|EQ|Analyzer|Dynamics");
        assert_eq!(m.clap_feature_list(), ["audio-effect", "stereo", "equalizer", "analyzer", "compressor"]);
        let m = cat("[\"Distortion\", \"Multi-band\"]");
        assert_eq!(m.vst3_sub_categories(), "Fx|Distortion|Tools");
        assert!(m.clap_feature_list().contains(&"multi-effects".to_string()));
        assert_eq!(parse(GAIN).unwrap().vst3_sub_categories(), "Fx");
        assert_eq!(parse(GAIN).unwrap().clap_feature_list(), ["audio-effect", "stereo"]);
        let raw = parse(&format!("{GAIN}\n[vst3]\nsubcategories = \"Fx|Whatever\"\n[clap]\nfeatures = [\"audio-effect\"]")).unwrap();
        assert_eq!((raw.vst3_sub_categories().as_str(), raw.clap_feature_list()), ("Fx|Whatever", vec!["audio-effect".to_string()]));
    }

    const HEAD: &str = "[plugin]
id = \"com.example.eq\"
name = \"Eq\"
";

    #[test]
    fn repeated_groups_expand_in_order_with_stable_keys_and_ids() {
        let m = parse(&format!(
            "{HEAD}[params]
master = {{ type = \"float\", range = [-12, 12] }}

[params.band]
repeat = 3

[params.band.fields]
freq = {{ type = \"float\", range = [20, 20000], default = 1000, scale = \"log\", unit = \"Hz\", smooth = \"10ms\" }}
mode = {{ type = \"enum\", choices = [\"Bell\", \"Shelf\"], hidden = true }}
"
        ))
        .unwrap();
        let keys: Vec<&str> = m.params.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["master", "band_1_freq", "band_1_mode", "band_2_freq", "band_2_mode", "band_3_freq", "band_3_mode"]);
        assert_eq!(m.groups.len(), 1);
        let g = &m.groups[0];
        assert_eq!((g.name.as_str(), g.repeat, g.start, g.first, g.fields.clone()), ("band", 3, 1, 1, vec!["freq".to_string(), "mode".to_string()]));
        let p = &m.params[3];
        assert_eq!(p.group, Some(GroupRef { group: 0, instance: 1, field: 0 }));
        assert_eq!(p.id, param_id("band_2_freq"));
        assert!(!p.explicit_id);
        assert_eq!((p.unit.as_str(), p.scale, p.default, p.smooth.map(|s| s.kind)), ("Hz", Scale::Log, 1000.0, Some(0)));
        assert_eq!(p.name, "Band 2 Freq");
        assert!(m.params[4].hidden && matches!(&m.params[4].kind, Kind::Enum { choices } if choices.len() == 2));
        assert!(m.params[0].group.is_none());
        // the same text gives the same IDs
        let again = parse(&format!("{HEAD}[params]
master = {{ type = \"float\", range = [-12, 12] }}

[params.band]
repeat = 3

[params.band.fields]
freq = {{ type = \"float\", range = [20, 20000], default = 1000, scale = \"log\", unit = \"Hz\", smooth = \"10ms\" }}
mode = {{ type = \"enum\", choices = [\"Bell\", \"Shelf\"], hidden = true }}
")).unwrap();
        assert_eq!(m.params.iter().map(|p| p.id).collect::<Vec<_>>(), again.params.iter().map(|p| p.id).collect::<Vec<_>>());
    }

    #[test]
    fn group_ids_from_a_base_and_a_stride_or_an_expression() {
        let m = parse(&format!(
            "{HEAD}[params.band]
repeat = 6
key = \"band{{n}}_\"
name = \"Band {{n}} \"
id_base = 7
id_stride = 17
[params.band.fields]
feedback_amount = {{ type = \"float\", range = [0, 1], offset = 0 }}
feedback_freq = {{ type = \"float\", range = [0, 1], offset = 1 }}
"
        ))
        .unwrap();
        assert_eq!(m.params.len(), 12);
        assert_eq!(m.params[0].key, "band1_feedback_amount");
        assert_eq!(m.params[0].name, "Band 1 Feedback Amount");
        assert_eq!((m.params[0].id, m.params[1].id, m.params[2].id, m.params[11].id), (7, 8, 24, 7 + 5 * 17 + 1));
        assert!(m.params.iter().all(|p| p.explicit_id));

        // Kiwami-QX style: 24 bands, zero padded keys, an expression, a zero based count
        let kq = parse(&format!(
            "{HEAD}[params.band]
repeat = 24
prefix = \"b{{n:02}}_\"
id_base = \"2000 + (n-1)*64\"
[params.band.fields]
freq = {{ type = \"float\", range = [10, 30000], scale = \"log\", offset = 2 }}
gain = {{ type = \"float\", range = [-30, 30], unit = \"dB\", offset = 3 }}
"
        ))
        .unwrap();
        assert_eq!(kq.params.len(), 48);
        let by_key = |k: &str| kq.params.iter().find(|p| p.key == k).unwrap().id;
        assert_eq!((by_key("b01_freq"), by_key("b01_gain"), by_key("b24_gain")), (2002, 2003, 2000 + 23 * 64 + 3));
        let zero = parse(&format!(
            "{HEAD}[params.xy]
repeat = 2
start = 0
key = \"xy{{n}}_\"
id_base = 100
id_stride = 4
[params.xy.fields]
x = {{ type = \"float\", range = [0, 1], offset = 0 }}
"
        ))
        .unwrap();
        assert_eq!(zero.params.iter().map(|p| (p.key.as_str(), p.id)).collect::<Vec<_>>(), [("xy0_x", 100), ("xy1_x", 104)]);
    }

    #[test]
    fn group_mistakes_are_reported() {
        let code = |extra: &str| parse(&format!("{HEAD}{extra}")).unwrap_err().code;
        let fields = "[params.g.fields]
a = { type = \"bool\" }
";
        assert_eq!(code(&format!("[params.g]
repeat = 0
{fields}")), "bad-group-repeat");
        assert_eq!(code(&format!("[params.g]
repeat = 2000
{fields}")), "bad-group-repeat");
        assert_eq!(code("[params.g]
repeat = 2
"), "group-fields-required");
        assert_eq!(code(&format!("[params.g]
repeat = 2
key = \"a_\"
{fields}")), "key-duplicate");
        assert_eq!(code(&format!("[params.g]
repeat = 2
key = \"a{{n}}\"
prefix = \"b{{n}}\"
{fields}")), "group-key-twice");
        assert_eq!(code(&format!("[params.g]
repeat = 2
key = \"a{{m}}\"
{fields}")), "bad-group-template");
        assert_eq!(code(&format!("[params.g]
repeat = 2
id_base = 5
[params.g.fields]
a = {{ type = \"bool\", offset = 0 }}
")), "group-needs-stride");
        assert_eq!(code(&format!("[params.g]
repeat = 2
id_base = 5
id_stride = 2
{fields}")), "group-offset-required");
        assert_eq!(code("[params.g]
repeat = 2
[params.g.fields]
a = { type = \"bool\", offset = 1 }
"), "group-offset-needs-base");
        assert_eq!(code("[params.g]
repeat = 2
[params.g.fields]
a = { type = \"bool\", id = 1 }
"), "group-field-id");
        assert_eq!(code("[params.g]
repeat = 2
id_base = \"n *\"
[params.g.fields]
a = { type = \"bool\", offset = 0 }
"), "bad-group-expression");
        assert_eq!(code("[params.g]
repeat = 2
id_base = 1
id_stride = 1
[params.g.fields]
a = { type = \"bool\", offset = 0 }
b = { type = \"bool\", offset = 1 }
"), "id-duplicate");
        assert_eq!(code("[params.g]
repeat = 1
id_base = 1073741820
[params.g.fields]
a = { type = \"bool\", offset = 9 }
"), "bad-param-id");
        assert_eq!(code(&format!("[params]
g = {{ type = \"bool\" }}
[params.g2]
repeat = 1
key = \"\"
[params.g2.fields]
g = {{ type = \"bool\" }}
")), "key-duplicate");
        // a field is checked like any parameter
        assert_eq!(code(&format!("[params.g]
repeat = 2
[params.g.fields]
a = {{ type = \"float\", range = [1, 0] }}
")), "range-min-max");
        assert_eq!(code("[params.g]
repeat = 2
bogus = 1
[params.g.fields]
a = { type = \"bool\" }
"), "unknown-key");
    }

    #[test]
    fn nine_hundred_and_fifty_one_parameters_parse_quickly() {
        let mut text = String::from(HEAD);
        text.push_str("[params]
");
        for i in 0..15 {
            text.push_str(&format!("g{i} = {{ type = \"float\", range = [0, 1] }}
"));
        }
        text.push_str("[params.b]
repeat = 24
key = \"b{n:02}_\"
id_base = \"2000 + (n-1)*64\"
[params.b.fields]
");
        for f in 0..39 {
            text.push_str(&format!("f{f:02} = {{ type = \"float\", range = [0, 1], offset = {f} }}
"));
        }
        let t = std::time::Instant::now();
        let m = parse(&text).unwrap();
        assert_eq!(m.params.len(), 15 + 24 * 39);
        assert_eq!(m.params.len(), 951);
        assert!(t.elapsed().as_millis() < 2000, "{:?}", t.elapsed());
    }

    #[test]
    fn vst3_controller_and_compatibility_classes() {
        let a = "3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11";
        let b = "A1C4F7E2-6B38-4C90-8F2D-5E7B3A91C640";
        let c = "0D9E5B70-2A46-4F83-B1C7-8E4A6F2D9035";
        let old = "11111111-2222-3333-4444-555555555555";
        let m = parse(&format!("{GAIN}\n[vst3]\nclass_id = \"{a}\"\ncontroller_id = \"{b}\"\ncompatibility = {{ id = \"{c}\", replaces = [\"{old}\"] }}")).unwrap();
        assert_eq!(m.vst3_controller_id.unwrap()[..4], [0xA1, 0xC4, 0xF7, 0xE2]);
        let compat = m.vst3_compat.unwrap();
        assert_eq!(compat.id[..4], [0x0D, 0x9E, 0x5B, 0x70]);
        assert_eq!(compat.replaces, vec![parse_guid(old).unwrap()]);
        let plain = parse(GAIN).unwrap();
        assert!(plain.vst3_controller_id.is_none() && plain.vst3_compat.is_none());
        // compatibility without replaces is fine; id is required
        assert!(parse(&format!("{GAIN}\n[vst3]\ncompatibility = {{ id = \"{c}\" }}")).unwrap().vst3_compat.unwrap().replaces.is_empty());
        let code = |extra: &str| parse(&format!("{GAIN}\n[vst3]\n{extra}")).unwrap_err().code;
        assert_eq!(code("compatibility = { replaces = [] }"), "required");
        assert_eq!(code("controller_id = \"nope\""), "bad-guid");
        assert_eq!(code(&format!("compatibility = {{ id = \"{c}\", replaces = [\"x\"] }}")), "bad-guid");
        assert_eq!(code(&format!("compatibility = {{ id = \"{c}\", other = 1 }}")), "unknown-key");
        assert_eq!(code(&format!("class_id = \"{a}\"\ncontroller_id = \"{a}\"")), "vst3-id-duplicate");
        assert_eq!(code(&format!("controller_id = \"{b}\"\ncompatibility = {{ id = \"{b}\" }}")), "vst3-id-duplicate");
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
    fn isolated_plugins_carry_at_most_1020_parameters() {
        // `n` plain parameters, then the isolation table.
        let plain = |n: usize| {
            let mut text = String::from("[plugin]\nid = \"com.example.many\"\nname = \"Many\"\n[params]\n");
            for i in 0..n {
                text.push_str(&format!("p{i} = {{ type = \"bool\" }}\n"));
            }
            text
        };
        let isolated = |text: &str| format!("{text}\n[isolation]\nmode = \"process\"\n");
        let e = parse(&isolated(&plain(1021))).err().unwrap();
        assert_eq!(e.code, "isolation-too-many-params");
        assert!(e.to_string().contains("this plugin has 1021"), "{e}");
        assert!(parse(&isolated(&plain(1020))).is_ok());
        assert!(parse(&plain(1021)).is_ok(), "without process isolation there is no such limit");
        // A repeated group counts once per copy of each field.
        let grouped = "[plugin]\nid = \"com.example.many\"\nname = \"Many\"\n[params.band]\nrepeat = 1021\n[params.band.fields]\nfreq = { type = \"bool\" }\n[isolation]\nmode = \"process\"\n";
        assert_eq!(parse(grouped).err().unwrap().code, "isolation-too-many-params");
        assert!(parse(&grouped.replace("repeat = 1021", "repeat = 1020")).is_ok());
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

/// "20ms" or "0.5s" in seconds.
fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim();
    if let Some(ms) = text.strip_suffix("ms") {
        return ms.trim().parse::<f64>().ok().map(|v| v / 1000.0);
    }
    text.strip_suffix('s')?.trim().parse::<f64>().ok()
}

#[cfg(test)]
mod readme_example {
    /// `[bundle]` (read by `cargo ferritron bundle`) is allowed and checked.
    #[test]
    fn bundle_section_is_read_and_checked() {
        use super::{BundleSettings, bundle_settings, parse};
        let err = |src: &str| parse(src).unwrap_err().to_string();
        let base = "[plugin]\nid = \"a\"\nname = \"x\"\n";
        assert_eq!(bundle_settings(base).unwrap(), BundleSettings::default());
        let on = format!("{base}[bundle]\ninstall = true\nshortcuts = true\nshortcut_prefix = \"~ \"\n");
        assert!(parse(&on).is_ok());
        assert_eq!(bundle_settings(&on).unwrap(), BundleSettings { install: true, shortcuts: true, shortcut_prefix: "~ ".into() });
        assert!(err(&format!("{base}[bundle]\ninstal = true")).contains("unknown key `bundle.instal`"));
        assert!(err(&format!("{base}[bundle]\nshortcuts = 1")).contains("bundle.shortcuts must be true or false"));
        assert!(err(&format!("{base}[bundle]\nshortcut_prefix = \"a/b\"")).contains("bundle.shortcut_prefix cannot contain"));
    }

    /// `[companion] files` (read by `cargo ferritron bundle`, e.g. `examples/csharp-gain`) is allowed.
    #[test]
    fn companion_files_are_allowed() {
        let src = "[plugin]
id = \"a.b\"
name = \"X\"
[companion]
files = [\"managed/X.dll\"]
";
        super::parse(src).unwrap_or_else(|e| panic!("{e:?}"));
    }

    /// The repeated-parameter example of the README parses without `key`, `name` or `id_base`.
    #[test]
    fn minimal_repeat_parses() {
        let src = "[plugin]\nid = \"a.b\"\nname = \"X\"\n[params]\ncutoff = { type = \"float\", range = [20, 20000], default = 1000, scale = \"log\", smooth = \"20ms\" }\n[params.band]\nrepeat = 8\n[params.band.fields]\nfreq = { type = \"float\", range = [20, 20000], default = 1000 }\n";
        let m = super::parse(src).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.params.len(), 1 + 8);
    }
}
