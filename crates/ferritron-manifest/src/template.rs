//! New plugin projects: the templates behind `cargo ferritron new` and the
//! Ferritron Editor, and finding the Ferritron framework they depend on.

use crate::{EXTENSION, Error, error, parse};
use std::path::{Path, PathBuf};

/// Project templates.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    /// An effect with a gain parameter.
    Effect,
    /// An instrument: a small polyphonic synthesizer.
    Instrument,
}

/// The environment variable naming the Ferritron framework folder.
pub const HOME_VARIABLE: &str = "FERRITRON_HOME";

fn is_framework(dir: &Path) -> bool {
    dir.join("crates").join("ferritron").join("Cargo.toml").is_file()
}

/// The Ferritron framework folder (the one containing `crates/ferritron`):
/// `FERRITRON_HOME` if set, else the nearest folder above the running
/// program that is the framework or holds it as `ferritron/` (a checkout's
/// `target/` folder, or a starter kit's `bin/` next to `ferritron/`).
pub fn framework_dir() -> Result<PathBuf, Error> {
    let home = std::env::var_os(HOME_VARIABLE).filter(|h| !h.is_empty()).map(PathBuf::from);
    find_framework(home, std::env::current_exe().ok().as_deref())
}

fn find_framework(home: Option<PathBuf>, program: Option<&Path>) -> Result<PathBuf, Error> {
    if let Some(home) = home {
        return if is_framework(&home) {
            Ok(home)
        } else {
            Err(error!(
                "ferritron-home-invalid",
                "{$variable} is {$path}, which does not contain crates/ferritron",
                variable = HOME_VARIABLE,
                path = home.display()
            ))
        };
    }
    for dir in program.iter().flat_map(|p| p.ancestors().skip(1)) {
        for candidate in [dir.to_path_buf(), dir.join("ferritron")] {
            if is_framework(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(error!(
        "ferritron-not-found",
        "cannot find the Ferritron framework: set {$variable} to its folder (the one containing crates/ferritron)",
        variable = HOME_VARIABLE
    ))
}

/// Creates a project from a template in `dir/name`, depending on the
/// framework found by [`framework_dir`]; returns its folder.
pub fn new_project(dir: &Path, name: &str, template: Template, id: Option<&str>, vendor: Option<&str>) -> Result<PathBuf, Error> {
    create(dir, name, template, id, vendor, &framework_dir()?)
}

fn create(dir: &Path, name: &str, template: Template, id: Option<&str>, vendor: Option<&str>, framework: &Path) -> Result<PathBuf, Error> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return Err(error!("bad-package-name", "\"{$name}\" is not a valid package name (use lowercase letters, digits, - and _)", name = name));
    }
    let root = dir.join(name);
    if root.exists() {
        return Err(error!("already-exists", "{$path} already exists", path = root.display()));
    }
    // Ferritron is not on crates.io: projects depend on the framework by path.
    let ferritron = framework.join("crates").join("ferritron");
    let ferritron = std::fs::canonicalize(&ferritron).unwrap_or(ferritron);
    let ferritron = ferritron.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    let title: String =
        name.split(['-', '_']).filter(|w| !w.is_empty()).map(|w| w[..1].to_ascii_uppercase() + &w[1..]).collect::<Vec<_>>().join(" ");
    let ident = title.replace(' ', "");
    let id = id.map(str::to_string).unwrap_or_else(|| format!("com.example.{}", name.replace('-', "_")));
    let vendor = vendor.unwrap_or("Example");
    let (kind, params, ja, code) = match template {
        Template::Effect => (
            "",
            "gain = { type = \"float\", range = [-60, 12], default = 0, unit = \"dB\" }\n",
            "param-gain = ゲイン\n",
            format!(
                r#"use ferritron::prelude::*;

#[derive(Default)]
struct {ident};

#[ferritron::plugin]
impl Process for {ident} {{
    fn process(&mut self, ctx: &mut Context) {{
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
    }}
}}
"#
            ),
        ),
        Template::Instrument => (
            "kind   = \"instrument\"\n",
            "volume = { type = \"float\", range = [-60, 0], default = -12, unit = \"dB\" }\n",
            "param-volume = 音量\n",
            format!(
                r#"use ferritron::NoteKind;
use ferritron::prelude::*;

/// One voice per key; a sine with a short release.
#[derive(Default)]
struct {ident} {{
    voices: Vec<(u8, f32, f32, bool)>, // key, phase, level, held
}}

#[ferritron::plugin]
impl Process for {ident} {{
    fn process(&mut self, ctx: &mut Context) {{
        let rate = ctx.sample_rate as f32;
        let volume = db_to_gain(ctx.params.volume.value());
        let notes = ctx.notes;
        let out = ctx.io.channel_mut(0);
        let mut next = 0;
        for (i, sample) in out.iter_mut().enumerate() {{
            while let Some(note) = notes.get(next).filter(|n| n.offset as usize == i) {{
                match note.kind {{
                    NoteKind::On => self.voices.push((note.key, 0.0, note.velocity, true)),
                    NoteKind::Off => self.voices.iter_mut().filter(|v| v.0 == note.key).for_each(|v| v.3 = false),
                }}
                next += 1;
            }}
            let mut sum = 0.0;
            for (key, phase, level, held) in self.voices.iter_mut() {{
                if !*held {{
                    *level *= 0.999;
                }}
                sum += (*phase * std::f32::consts::TAU).sin() * *level;
                *phase = (*phase + 440.0 * 2f32.powf((*key as f32 - 69.0) / 12.0) / rate).fract();
            }}
            self.voices.retain(|v| v.2 > 1e-4);
            *sample = sum * volume;
        }}
        for ch in 1..ctx.io.num_channels() {{
            ctx.io.copy_channel(0, ch);
        }}
    }}
}}
"#
            ),
        ),
    };
    let project_file = format!("{name}.{EXTENSION}");
    let files = [
        ("Cargo.toml", format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\nferritron = {{ path = \"{ferritron}\", version = \"{version}\" }}\n", version = env!("CARGO_PKG_VERSION"))),
        (
            project_file.as_str(),
            format!("[plugin]\nid     = \"{id}\"   # reverse-domain ID: never change it after release\nname   = \"{title}\"\nvendor = \"{vendor}\"\n{kind}\n[params]\n{params}"),
        ),
        ("src/lib.rs", code),
        ("locales/ja/main.ftl", ja.to_string()),
        (
            ".cargo/config.toml",
            "# Ferritron does not ship plugin-format SDKs; point these at your own copies.\n# [env]\n# VST3_SDK_DIR = \"C:/SDKs/vst3sdk\"   # https://github.com/steinbergmedia/vst3sdk\n# CLAP_SDK_DIR = \"C:/SDKs/clap\"      # https://github.com/free-audio/clap\n".to_string(),
        ),
        (".gitignore", "/target\n".to_string()),
    ];
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|e| error!("folder-uncreatable", "cannot create {$path}: {$detail}", path = path.display(), detail = e))?;
        std::fs::write(&path, content).map_err(|e| error!("file-unwritable", "cannot write {$path}: {$detail}", path = path.display(), detail = e))?;
    }
    // The template must pass the build's own checks.
    let file = root.join(&project_file);
    let text = std::fs::read_to_string(&file).map_err(|e| error!("file-unreadable", "cannot read {$path}: {$detail}", path = file.display(), detail = e))?;
    parse(&text).map_err(|e| error!("template-invalid", "template error: {$cause}").because(e))?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This checkout: the framework folder.
    fn checkout() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
    }

    fn temp(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferritron-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_framework_is_found_from_the_variable_a_checkout_or_a_starter_kit() {
        let framework = checkout();
        // FERRITRON_HOME wins, and must hold the framework.
        assert_eq!(find_framework(Some(framework.clone()), None).unwrap(), framework);
        let wrong = find_framework(Some(std::env::temp_dir()), Some(&framework.join("target/debug/x.exe"))).unwrap_err();
        assert_eq!(wrong.code, "ferritron-home-invalid");
        // A program built in the checkout.
        assert_eq!(find_framework(None, Some(&framework.join("target/release/cargo-ferritron.exe"))).unwrap(), framework);
        // A starter kit: bin/ next to ferritron/.
        let kit = temp("kit");
        std::fs::create_dir_all(kit.join("ferritron/crates/ferritron")).unwrap();
        std::fs::write(kit.join("ferritron/crates/ferritron/Cargo.toml"), "").unwrap();
        assert_eq!(find_framework(None, Some(&kit.join("bin/ferritron-editor.exe"))).unwrap(), kit.join("ferritron"));
        // Nowhere.
        assert_eq!(find_framework(None, None).unwrap_err().code, "ferritron-not-found");
        let _ = std::fs::remove_dir_all(&kit);
    }

    #[test]
    fn templates_pass_the_build_checks_and_depend_on_the_framework() {
        let framework = checkout();
        let dir = temp("template");
        for (name, template) in [("my-gain", Template::Effect), ("my-synth", Template::Instrument)] {
            let root = create(&dir, name, template, None, Some("Me"), &framework).unwrap();
            let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
            assert!(cargo.contains("crates/ferritron\""), "{cargo}");
            assert!(create(&dir, name, template, None, None, &framework).is_err(), "existing folder");
        }
        assert!(create(&dir, "Bad Name", Template::Effect, None, None, &framework).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
