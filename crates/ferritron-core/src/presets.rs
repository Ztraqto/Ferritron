//! Factory presets (N4): `[presets.<id>]` in the project file.
//!
//! ```toml
//! [presets.warm]
//! name   = "Warm Pad"        # shown by hosts and editors (default: the id)
//! cutoff = 800               # plain values; enums by choice name, bools true / false
//! mix    = 0.35
//!
//! [presets.bright]
//! cutoff = 12000
//! ```
//!
//! A preset sets the parameters it lists and leaves the others as they are. Loading one is a
//! bulk change ([`InstanceHandle::load_values`](crate::InstanceHandle::load_values)): the audio
//! thread switches to all the values at the start of one block, **jumps the smoothers to the
//! targets** (no gliding from the old sound) and the host is told the values changed. It is not
//! an automation gesture, so hosts record no automation for it.
//!
//! ```ignore
//! ferritron::presets::load::<MyPlugin>(&instance, "warm");    // by id or name
//! let h = Harness::<MyPlugin>::new(48_000.0, 512);
//! h.load_preset("warm");                                       // in tests
//! ```
//!
//! Hosts see the presets too. A plugin with presets gets a built-in, hidden, not automatable
//! **"Program"** parameter (an enum of the preset names, saved with the project) that the shims
//! turn into the format's preset list: VST3 a program list (`IUnitInfo`, `kIsProgramChange`),
//! CLAP `preset-load` (location kind *plugin*, load key = the preset id), VST2 programs. When
//! the host sets it (from any thread, the audio thread included) the preset is loaded on the
//! main thread, as [`load_index`] does. [`current`] says which preset was chosen last.
//!
//! **User presets** are files: [`save_user`], [`user_list`], [`load_user`], [`delete_user`]
//! (see below for where they live).

use crate::__private::PluginMeta;
use crate::InstanceHandle;

/// One factory preset, from the project file.
#[derive(Debug)]
pub struct Preset {
    /// The key of the `[presets.<id>]` table.
    pub id: &'static str,
    /// The name to show (`name = "..."`, else the id).
    pub name: &'static str,
    /// `(parameter index, plain value)` for each parameter the preset sets; the index is the
    /// position in the project file.
    pub values: &'static [(usize, f64)],
}

/// The presets of plugin `P`, in project-file order.
pub fn list<P: PluginMeta>() -> &'static [Preset] {
    P::presets()
}

/// The preset of `P` whose id or name is `name` (ignoring case).
pub fn find<P: PluginMeta>(name: &str) -> Option<&'static Preset> {
    P::presets().iter().find(|p| p.id.eq_ignore_ascii_case(name) || p.name.eq_ignore_ascii_case(name))
}

/// Loads preset `index` into `instance` (see the [module docs](self)). Returns `false` for an
/// index out of range. Any thread except the audio thread.
pub fn load_index<P: PluginMeta>(instance: &InstanceHandle, index: usize) -> bool {
    match P::presets().get(index) {
        Some(preset) => {
            instance.0.set_program(index as f64);
            instance.load_values(preset.values);
            true
        }
        None => false,
    }
}

/// Loads the preset named `name` (id or name) into `instance`. Returns `false` if there is none.
pub fn load<P: PluginMeta>(instance: &InstanceHandle, name: &str) -> bool {
    match P::presets().iter().position(|p| p.id.eq_ignore_ascii_case(name) || p.name.eq_ignore_ascii_case(name)) {
        Some(index) => load_index::<P>(instance, index),
        None => false,
    }
}

/// The preset chosen last (by the host's program list or [`load`] / [`load_index`]); `None`
/// for a plugin without presets. A new instance says preset 0 although nothing was loaded.
pub fn current<P: PluginMeta>(instance: &InstanceHandle) -> Option<usize> {
    (!P::presets().is_empty()).then(|| instance.0.program.get().round().max(0.0) as usize)
}

// ---- User presets --------------------------------------------------------------------------
//
// Presets the user saves: one JSON file per preset in `<Ferritron's data folder>/presets/<plugin
// id>/` (`%APPDATA%\Ferritron` on Windows, `~/Library/Application Support/Ferritron` on macOS,
// `$XDG_DATA_HOME/ferritron` elsewhere; `FERRITRON_PRESETS_DIR` replaces `<data>/presets`).
// A file holds every parameter by key (`{"name": "...", "values": {"cutoff": 800, ...}}`), so it
// survives parameters being added or reordered; keys it does not know are ignored. Not for the
// audio thread (files).

use std::path::PathBuf;

/// The folder of `P`'s user presets (it may not exist yet).
pub fn user_dir<P: PluginMeta>() -> Option<PathBuf> {
    let root = match std::env::var_os("FERRITRON_PRESETS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => crate::i18n::data_dir()?.join("presets"),
    };
    let id: String = P::INFO.id.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    Some(root.join(id))
}

/// A preset name as a file name: characters no file system takes become `_`.
fn file_name(name: &str) -> String {
    let cleaned: String = name.trim().chars().map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() { "_".to_string() } else { cleaned }
}

fn user_file<P: PluginMeta>(name: &str) -> std::io::Result<PathBuf> {
    let dir = user_dir::<P>().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no data folder"))?;
    Ok(dir.join(format!("{}.json", file_name(name))))
}

/// The names of `P`'s user presets, sorted.
pub fn user_list<P: PluginMeta>() -> Vec<String> {
    let Some(dir) = user_dir::<P>() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            let stem = e.path().file_stem()?.to_string_lossy().into_owned();
            Some(json.get("name").and_then(|n| n.as_str()).map(str::to_string).unwrap_or(stem))
        })
        .collect();
    names.sort();
    names
}

/// Saves the instance's parameter values as user preset `name` (replacing one of that name).
/// Returns the file written.
pub fn save_user<P: PluginMeta>(instance: &InstanceHandle, name: &str) -> std::io::Result<PathBuf> {
    let path = user_file::<P>(name)?;
    let mut values = serde_json::Map::new();
    for info in instance.param_infos() {
        if let Some(v) = instance.value(info.key) {
            values.insert(info.key.to_string(), serde_json::json!(v));
        }
    }
    let json = serde_json::json!({ "name": name.trim(), "plugin": P::INFO.id, "values": values });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written beside and renamed, so a crash never leaves half a preset.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&json).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Loads user preset `name` into the instance, as a factory preset loads (all values at the
/// start of one block, smoothers jump, the host is told).
pub fn load_user<P: PluginMeta>(instance: &InstanceHandle, name: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(user_file::<P>(name)?)?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let infos = instance.param_infos();
    let values: Vec<(usize, f64)> = json
        .get("values")
        .and_then(|v| v.as_object())
        .map(|map| map.iter().filter_map(|(key, v)| Some((infos.iter().position(|i| i.key == key)?, v.as_f64()?))).collect())
        .unwrap_or_default();
    instance.load_values(&values);
    Ok(())
}

/// Deletes user preset `name`.
pub fn delete_user<P: PluginMeta>(name: &str) -> std::io::Result<()> {
    std::fs::remove_file(user_file::<P>(name)?)
}
