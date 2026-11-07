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
//! `ferritron_manifest::Manifest::template_values`) and `{executable}`, the
//! binary's file name.
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
        // Keys under `[...format.<os>]` replace the general ones on that OS.
        let over = &spec[std::env::consts::OS];
        let pick = |key: &str| over[key].as_str().or(spec[key].as_str()).map(str::to_string);
        let field = |key: &str| pick(key).ok_or_else(|| format!("{crate_name}: format metadata needs `{key}`"));
        let table = |key: &str| -> BTreeMap<String, String> {
            spec[key]
                .as_object()
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
        });
    }
    formats.sort_by(|a, b| a.name.cmp(&b.name));
    formats.dedup_by(|a, b| a.name == b.name);
    Ok(formats)
}

fn platform() -> (&'static str, String) {
    let os = std::env::consts::OS;
    (os, format!("{os}-{}", std::env::consts::ARCH))
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

    /// Writes the package into `out_dir`; returns its root and the binary
    /// inside. `values` fill the Info.plist template.
    pub fn package(&self, library: &Path, out_dir: &Path, name: &str, values: &[(String, String)]) -> Result<(PathBuf, PathBuf), String> {
        let root = out_dir.join(self.expand(&self.root, name)?);
        let binary = out_dir.join(self.expand(&self.binary, name)?);
        if root.is_dir() {
            std::fs::remove_dir_all(&root).map_err(|e| format!("cannot replace {}: {e}", root.display()))?;
        }
        let parent = binary.parent().ok_or("invalid binary path")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        std::fs::copy(library, &binary).map_err(|e| format!("cannot copy {} to {}: {e}", library.display(), binary.display()))?;
        if let Some(template) = &self.plist {
            let text = std::fs::read_to_string(template).map_err(|e| format!("cannot read {}: {e}", template.display()))?;
            let executable = binary.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut values = values.to_vec();
            values.push(("executable".into(), executable));
            let plist = root.join("Contents/Info.plist");
            std::fs::write(&plist, fill_xml(&text, &values)?).map_err(|e| format!("cannot write {}: {e}", plist.display()))?;
        }
        Ok((root, binary))
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

/// Fills `{key}` placeholders in an XML template, escaping the values.
fn fill_xml(template: &str, values: &[(String, String)]) -> Result<String, String> {
    let escape = |v: &str| v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').ok_or("unclosed { in an Info.plist template")? + start;
        let key = &rest[start + 1..end];
        let value = values.iter().find(|(k, _)| k == key).map(|(_, v)| v).ok_or_else(|| format!("unknown placeholder {{{key}}} in an Info.plist template"))?;
        out.push_str(&rest[..start]);
        out.push_str(&escape(value));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
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
        out.push_str(&std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(PathBuf::from(out))
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
    fn plist_values_are_escaped() {
        let values = vec![("name".to_string(), "A & <B>".to_string())];
        assert_eq!(fill_xml("<string>{name}</string>", &values).unwrap(), "<string>A &amp; &lt;B&gt;</string>");
        assert!(fill_xml("{nope}", &values).is_err());
    }

    #[test]
    fn env_templates_expand() {
        unsafe { std::env::set_var("FERRITRON_TEST_DIR", "C:/x") };
        assert_eq!(expand_env("{FERRITRON_TEST_DIR}/CLAP").unwrap(), PathBuf::from("C:/x/CLAP"));
        assert!(expand_env("{NO_SUCH_VAR_FERRITRON}").is_err());
    }
}
