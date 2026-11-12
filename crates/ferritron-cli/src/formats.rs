//! Plugin formats, as declared by format crates in their Cargo.toml:
//!
//! ```toml
//! [package.metadata.ferritron.format]
//! name = "VST3"
//! entry-symbol = "GetPluginFactory"          # exported when the format is linked
//! root = "{name}.vst3"                       # what gets packaged and installed
//! binary = "{name}.vst3/Contents/{arch}/{name}.{ext}"
//! arch = { windows-x86_64 = "x86_64-win" }   # values for {arch}, by <os>-<arch>
//! ext = { windows = "vst3", linux = "so" }   # values for {ext}, by <os>
//! install = { windows = "{COMMONPROGRAMFILES}/VST3" }
//!
//! # Per operating system: these keys replace the ones above on macOS.
//! [package.metadata.ferritron.format.macos]
//! binary = "{name}.vst3/Contents/MacOS/{name}"
//! info-plist = "macos/Info.plist"   # template, relative to the format crate
//! ```
//!
//! `info-plist` is written to `<root>/Contents/Info.plist` with `{key}`
//! placeholders filled from the project file (see
//! `ferritron_manifest::Manifest::template_scope`) and `{executable}` /
//! `{binary}`, the binary's file name.
//!
//! More files a format may need:
//!
//! ```toml
//! # Generated: templates (relative to the format crate) filled like the
//! # Info.plist, with lists and sections (see `ferritron_manifest::render`).
//! files = { "{name}.lv2/manifest.ttl" = "lv2/manifest.ttl" }
//! # Copied as they are.
//! assets = { "{name}.wam/index.js" = "web/index.js" }
//! # The plugin's web editor folder (`[view] entry`), copied here if it has one.
//! ui = "{name}.wam/gui"
//! # An executable that loads the binary and calls the entry symbol (apps),
//! # compiled by `cargo ferritron`; "appex" for an Apple app extension.
//! launcher = "{name}/{name}.exe"
//! launcher-kind = "appex"
//! # Packaging after the files are in place: "apk" (Android).
//! package = "apk"
//! # Template values from the binary's exported symbols: the first symbol
//! # starting with the prefix, without anything up to its last `$_`
//! # (`OBJC_CLASS_$_Name` -> `Name`).
//! symbol-values = { "auv3.class" = "OBJC_CLASS_$_FerritronAUv3_" }
//! ```
//!
//! Templates also get `{file}`: the plugin's name as a file name.
//!
//! Formats may share a root (an app and the AUv3 extension inside it): a
//! root made earlier in the same run is kept.
//!
//! The CLI knows no format itself: a new format only needs a crate with this
//! table, linked into the plugin.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Format {
    pub name: String,
    pub entry_symbol: String,
    root: String,
    binary: String,
    arch: BTreeMap<String, String>,
    ext: BTreeMap<String, String>,
    install: BTreeMap<String, String>,
    /// Info.plist template (macOS bundles).
    plist: Option<PathBuf>,
    /// Generated files: destination template -> template file.
    files: BTreeMap<String, PathBuf>,
    /// Copied files: destination template -> source file.
    assets: BTreeMap<String, PathBuf>,
    /// Where the plugin's web editor folder goes.
    ui: Option<String>,
    /// The app's executable, compiled by `cargo ferritron`.
    launcher: Option<String>,
    /// `appex`: the launcher starts an Apple app extension.
    pub launcher_kind: Option<String>,
    /// A packaging step after the files are in place (`apk`).
    pub package: Option<String>,
    /// Template values from exported symbols: value name -> symbol prefix.
    symbol_values: BTreeMap<String, String>,
}

/// What a package is made from, besides the library.
pub struct Inputs<'a> {
    /// The plugin's name as a file name.
    pub name: &'a str,
    pub scope: &'a ferritron_manifest::Scope,
    /// The plugin's web editor folder, if it has one.
    pub ui: Option<&'a Path>,
    /// The root was made by another format in this run: add to it.
    pub keep_root: bool,
    /// The binary's exported symbols.
    pub exports: &'a [String],
}

/// Every format declared by a package in the plugin's dependency graph.
pub fn discover(metadata: &serde_json::Value) -> Result<Vec<Format>, String> {
    let mut formats = Vec::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let spec = &package["metadata"]["ferritron"]["format"];
        if spec.is_null() {
            continue;
        }
        let crate_name = package["name"].as_str().unwrap_or("?");
        let crate_dir = package["manifest_path"].as_str().and_then(|m| Path::new(m).parent()).map(Path::to_path_buf).unwrap_or_default();
        // Keys under `[...format.<os>]` replace the general ones on that OS (the target's).
        let over = &spec[platform().0];
        let pick = |key: &str| over[key].as_str().or(spec[key].as_str()).map(str::to_string);
        let field = |key: &str| pick(key).ok_or_else(|| format!("{crate_name}: format metadata needs `{key}`"));
        let table = |key: &str| -> BTreeMap<String, String> {
            over[key]
                .as_object()
                .or(spec[key].as_object())
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        };
        formats.push(Format {
            name: field("name")?,
            entry_symbol: field("entry-symbol")?,
            root: field("root")?,
            binary: field("binary")?,
            arch: table("arch"),
            ext: table("ext"),
            install: table("install"),
            plist: pick("info-plist").map(|p| crate_dir.join(p)),
            files: table("files").into_iter().map(|(k, v)| (k, crate_dir.join(v))).collect(),
            assets: table("assets").into_iter().map(|(k, v)| (k, crate_dir.join(v))).collect(),
            ui: pick("ui"),
            // An empty value under an OS turns a general key off there.
            launcher: pick("launcher").filter(|l| !l.is_empty()),
            launcher_kind: pick("launcher-kind"),
            package: pick("package"),
            symbol_values: table("symbol-values"),
        });
    }
    formats.sort_by(|a, b| a.name.cmp(&b.name));
    formats.dedup_by(|a, b| a.name == b.name);
    Ok(formats)
}

/// The OS and architecture bundles are made for (`--target`), this computer's by default.
static TARGET: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Bundles are made for `os` (`windows`, `macos`, `linux`) and `arch`.
pub fn set_target(os: &str, arch: &str) {
    let _ = TARGET.set((os.to_string(), arch.to_string()));
}

fn platform() -> (&'static str, String) {
    let (os, arch) = TARGET.get_or_init(|| (std::env::consts::OS.into(), std::env::consts::ARCH.into()));
    (os.as_str(), format!("{os}-{arch}"))
}

impl Format {
    fn expand(&self, template: &str, name: &str) -> Result<String, String> {
        let mut out = template.replace("{name}", name);
        if out.contains("{arch}") {
            let (_, key) = platform();
            let arch = self.arch.get(&key).ok_or_else(|| format!("{} is not supported on {key} yet", self.name))?;
            out = out.replace("{arch}", arch);
        }
        if out.contains("{ext}") {
            let (os, _) = platform();
            let ext = self.ext.get(os).ok_or_else(|| format!("{} is not supported on {os} yet", self.name))?;
            out = out.replace("{ext}", ext);
        }
        Ok(out)
    }

    /// The package's root in `out_dir`.
    pub fn root(&self, out_dir: &Path, name: &str) -> Result<PathBuf, String> {
        Ok(out_dir.join(self.expand(&self.root, name)?))
    }

    /// The launcher's path in `out_dir`, if the format has one.
    pub fn launcher(&self, out_dir: &Path, name: &str) -> Result<Option<PathBuf>, String> {
        self.launcher.as_ref().map(|l| Ok(out_dir.join(self.expand(l, name)?))).transpose()
    }

    /// Writes the package into `out_dir`; returns its root and the binary inside.
    pub fn package(&self, library: &Path, out_dir: &Path, inputs: &Inputs) -> Result<(PathBuf, PathBuf), String> {
        let name = inputs.name;
        let root = out_dir.join(self.expand(&self.root, name)?);
        let binary = out_dir.join(self.expand(&self.binary, name)?);
        if root.is_dir() && !inputs.keep_root {
            std::fs::remove_dir_all(&root).map_err(|e| format!("cannot replace {}: {e}", root.display()))?;
        }
        let parent = binary.parent().ok_or("invalid binary path")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        std::fs::copy(library, &binary).map_err(|e| format!("cannot copy {} to {}: {e}", library.display(), binary.display()))?;
        let executable = binary.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut scope = inputs.scope.clone();
        scope.text("file", name.to_string());
        for (key, prefix) in &self.symbol_values {
            let symbol = inputs.exports.iter().find(|s| s.starts_with(prefix.as_str())).ok_or_else(|| {
                format!("{}: the binary exports no symbol starting with `{prefix}` (was its native part built?)", self.name)
            })?;
            let value = symbol.rsplit_once("$_").map_or(symbol.as_str(), |(_, v)| v);
            scope.text(key, value.to_string());
        }
        scope.text("binary.uri", uri_path(&executable)).text("executable", executable.clone()).text("binary", executable);
        let mut generated: Vec<(PathBuf, &PathBuf)> =
            self.files.iter().map(|(to, from)| Ok((out_dir.join(self.expand(to, name)?), from))).collect::<Result<_, String>>()?;
        if let Some(template) = &self.plist {
            generated.push((root.join("Contents/Info.plist"), template));
        }
        for (to, template) in generated {
            let text = std::fs::read_to_string(template).map_err(|e| format!("cannot read {}: {e}", template.display()))?;
            let file_name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let filled = ferritron_manifest::render(&text, &scope, ferritron_manifest::Escape::for_file(&file_name))
                .map_err(|e| format!("{}: {e}", template.display()))?;
            write(&to, filled.as_bytes())?;
        }
        for (to, from) in &self.assets {
            let to = out_dir.join(self.expand(to, name)?);
            let bytes = std::fs::read(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
            write(&to, &bytes)?;
        }
        if let (Some(to), Some(ui)) = (&self.ui, inputs.ui) {
            let to = out_dir.join(self.expand(to, name)?);
            copy_dir(ui, &to).map_err(|e| format!("cannot copy {} to {}: {e}", ui.display(), to.display()))?;
        }
        Ok((root, binary))
    }

    /// The system folder for this format on this computer (`None`: the format has none here,
    /// such as the standalone app).
    pub fn install_folder(&self) -> Option<Result<PathBuf, String>> {
        let (os, _) = platform();
        self.install.get(os).map(|template| expand_env(template))
    }

    /// Copies a package into the system folder for this format. Sibling files
    /// of a single-file package (such as the hot reload marker) come along.
    pub fn install(&self, root: &Path, sibling_suffixes: &[&str]) -> Result<PathBuf, String> {
        let (os, _) = platform();
        let template = self.install.get(os).ok_or_else(|| format!("no install folder for {} on {os}", self.name))?;
        let folder = expand_env(template)?;
        let dest = folder.join(root.file_name().ok_or("invalid package path")?);
        let hint = |e: std::io::Error| format!("cannot install to {}: {e} (try an administrator shell)", dest.display());
        if root.is_dir() {
            if dest.exists() {
                std::fs::remove_dir_all(&dest).map_err(hint)?;
            }
            copy_dir(root, &dest).map_err(hint)?;
        } else {
            std::fs::create_dir_all(&folder).map_err(hint)?;
            std::fs::copy(root, &dest).map_err(hint)?;
            for suffix in sibling_suffixes {
                let (from, to) = (with_suffix(root, suffix), with_suffix(&dest, suffix));
                if from.exists() {
                    std::fs::copy(from, to).map_err(hint)?;
                } else {
                    let _ = std::fs::remove_file(to);
                }
            }
        }
        Ok(dest)
    }
}

/// A file name as a relative URI (`My Plugin.dll` -> `My%20Plugin.dll`).
fn uri_path(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Writes a file, creating its folder.
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Replaces `{VAR}` with environment variables.
fn expand_env(template: &str) -> Result<PathBuf, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').ok_or_else(|| format!("unclosed {{ in `{template}`"))? + start;
        let var = &rest[start + 1..end];
        out.push_str(&rest[..start]);
        out.push_str(&match var {
            "VSTPLUGINS" => vst2_folder()?,
            _ => std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?,
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    // Templates use '/'; Windows paths are shown (and written into shortcuts) with '\'.
    if cfg!(windows) {
        out = out.replace('/', "\\");
    }
    Ok(PathBuf::from(out))
}

/// The VST 2 folder (`{VSTPLUGINS}`): the `VSTPLUGINS` variable if set, else the folder hosts read
/// from the registry (`HKLM\SOFTWARE\VST`, `VSTPluginsPath`), else `%COMMONPROGRAMFILES%\VST2`.
fn vst2_folder() -> Result<String, String> {
    if let Ok(dir) = std::env::var("VSTPLUGINS") {
        return Ok(dir);
    }
    #[cfg(windows)]
    if let Ok(out) = std::process::Command::new("reg").args(["query", r"HKLM\SOFTWARE\VST", "/v", "VSTPluginsPath"]).output()
        && let Some(dir) = reg_value(&String::from_utf8_lossy(&out.stdout))
    {
        return Ok(dir);
    }
    let common = std::env::var("COMMONPROGRAMFILES").map_err(|_| "environment variable COMMONPROGRAMFILES is not set".to_string())?;
    Ok(format!(r"{common}\VST2"))
}

/// The value of a `reg query /v` line (`    VSTPluginsPath    REG_SZ    C:\...`).
#[cfg_attr(not(windows), allow(dead_code))]
fn reg_value(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.trim_start().starts_with("VSTPluginsPath"))?;
    let (_, rest) = line.split_once("REG_")?;
    let value = rest.split_once(char::is_whitespace)?.1.trim();
    (!value.is_empty()).then(|| value.trim_end_matches(['\\', '/']).to_string())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_vst2_folder_from_reg_output() {
        let text = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\VST\r\n    VSTPluginsPath    REG_SZ    C:\\Program Files\\Vstplugins\\\r\n\r\n";
        assert_eq!(reg_value(text).as_deref(), Some(r"C:\Program Files\Vstplugins"));
        assert_eq!(reg_value("ERROR: not found"), None);
    }

    fn vst3() -> serde_json::Value {
        serde_json::json!({ "packages": [
            { "name": "other", "metadata": null },
            { "name": "ferritron-vst3", "metadata": { "ferritron": { "format": {
                "name": "VST3", "entry-symbol": "GetPluginFactory", "root": "{name}.vst3",
                "binary": "{name}.vst3/Contents/{arch}/{name}.vst3",
                "arch": { "windows-x86_64": "x86_64-win", "linux-x86_64": "x86_64-linux", "macos-aarch64": "x" },
                "install": { "windows": "{TEMP}/VST3" }
            }}}}
        ]})
    }

    #[test]
    fn formats_come_from_cargo_metadata() {
        let formats = discover(&vst3()).unwrap();
        assert_eq!(formats.len(), 1);
        let f = &formats[0];
        assert_eq!(f.entry_symbol, "GetPluginFactory");
        assert_eq!(f.expand(&f.binary, "Gain").unwrap().split('/').next_back(), Some("Gain.vst3"));
        assert!(f.expand(&f.binary, "Gain").unwrap().contains("Contents/"));
    }

    #[test]
    fn missing_fields_are_reported() {
        let bad = serde_json::json!({ "packages": [ { "name": "x", "metadata": { "ferritron": { "format": { "name": "X" }}}} ]});
        assert!(discover(&bad).err().unwrap().contains("entry-symbol"));
    }

    #[test]
    fn os_keys_replace_general_ones() {
        let os = std::env::consts::OS;
        let metadata = serde_json::json!({ "packages": [
            { "name": "ferritron-x", "manifest_path": "/f/x/Cargo.toml", "metadata": { "ferritron": { "format": {
                "name": "X", "entry-symbol": "x_entry", "root": "{name}.x", "binary": "{name}.x",
                os: { "root": "{name}.xb", "binary": "{name}.xb/Contents/MacOS/{name}", "info-plist": "mac/Info.plist" }
            }}}}
        ]});
        let f = &discover(&metadata).unwrap()[0];
        assert_eq!(f.expand(&f.binary, "Gain").unwrap(), "Gain.xb/Contents/MacOS/Gain");
        assert!(f.plist.as_ref().unwrap().ends_with("mac/Info.plist"));
    }

    #[test]
    fn generated_files_assets_and_ui_are_written() {
        let dir = std::env::temp_dir().join(format!("ferritron-formats-{}", std::process::id()));
        let crate_dir = dir.join("crate");
        std::fs::create_dir_all(crate_dir.join("t")).unwrap();
        std::fs::create_dir_all(dir.join("ui/img")).unwrap();
        std::fs::write(crate_dir.join("t/info.ttl"), "<{binary}> {#params}{symbol} {/params}").unwrap();
        std::fs::write(crate_dir.join("t/run.js"), "export const x = {name};").unwrap();
        std::fs::write(dir.join("ui/img/a.svg"), "<svg/>").unwrap();
        std::fs::write(dir.join("lib.bin"), "lib").unwrap();
        let metadata = serde_json::json!({ "packages": [
            { "name": "ferritron-x", "manifest_path": crate_dir.join("Cargo.toml").to_string_lossy(), "metadata": { "ferritron": { "format": {
                "name": "X", "entry-symbol": "x", "root": "{name}.x", "binary": "{name}.x/{name}.bin",
                "files": { "{name}.x/info.ttl": "t/info.ttl" }, "assets": { "{name}.x/run.js": "t/run.js" }, "ui": "{name}.x/gui"
            }}}}
        ]});
        let f = &discover(&metadata).unwrap()[0];
        let manifest = ferritron_manifest::parse("[plugin]\nid = \"a.b\"\nname = \"G\"\n[params]\ng = { type = \"bool\" }").unwrap();
        let scope = manifest.template_scope();
        let out = dir.join("out");
        let inputs = Inputs { name: "G", scope: &scope, ui: Some(&dir.join("ui")), keep_root: false, exports: &[] };
        let (root, _) = f.package(&dir.join("lib.bin"), &out, &inputs).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("info.ttl")).unwrap(), "<G.bin> g ferritron_bypass ");
        assert_eq!(std::fs::read_to_string(root.join("run.js")).unwrap(), "export const x = {name};");
        assert!(root.join("gui/img/a.svg").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_templates_expand() {
        unsafe { std::env::set_var("FERRITRON_TEST_DIR", "C:/x") };
        assert_eq!(expand_env("{FERRITRON_TEST_DIR}/CLAP").unwrap(), PathBuf::from("C:/x/CLAP"));
        assert!(expand_env("{NO_SUCH_VAR_FERRITRON}").is_err());
    }
}
