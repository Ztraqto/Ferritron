//! Where the SDKs are: one rule for build scripts, `cargo ferritron sdk` and
//! the Ferritron Editor, so they all agree.
//!
//! Ferritron never ships an SDK. The developer gets each one (by hand, or
//! with `cargo ferritron sdk fetch` / the Editor after agreeing to its
//! license) and Ferritron finds it, in this order:
//!
//! 1. A version pinned for the build (`[sdk]` in the project file; `cargo
//!    ferritron bundle` passes it as `FERRITRON_SDK_<ID>_VERSION`): that
//!    version in the managed folder, nothing else.
//! 2. The SDK's `source` in `sdk.toml` (`<data folder>/Ferritron/sdk.toml`):
//!    - `auto` (default, and without `sdk.toml`): the environment variable if
//!      set, else the managed folder (the chosen `version`, or the newest
//!      there);
//!    - `env`: only the environment variable (Ferritron 1.1's behavior);
//!    - `managed`: only the managed folder;
//!    - `path`: the folder in `path`.
//! 3. Nothing found: the build stops and lists every place it looked.
//!
//! The managed folder is `<root>/<id>/<version>/`; the root is
//! `FERRITRON_SDK_ROOT`, else `root` in `sdk.toml`, else `C:\Ferritron\SDK`
//! (Windows) or `~/Ferritron/SDK`. A version folder (or a `path`) may hold the
//! SDK directly or in a single subfolder (as an archive unpacks).
//!
//! ```toml
//! # sdk.toml
//! root = "C:/Ferritron/SDK"
//!
//! [vst3]
//! source  = "auto"     # auto | env | managed | path
//! version = "3.8.1"
//!
//! [ara]
//! source = "path"
//! path   = "D:/work/ARA_SDK"
//! ```

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An SDK or tool Ferritron can find.
#[derive(Debug, PartialEq, Eq)]
pub struct Sdk {
    /// `vst3`, `clap`, `ara`, `aax`, `ndk`, `xclang`: the key in `sdk.toml`, `[sdk]` and
    /// the managed folder.
    pub id: &'static str,
    pub name: &'static str,
    /// The environment variable that names its folder.
    pub var: &'static str,
    /// Files (relative to the folder) that must exist for it to count.
    pub probe: &'static [&'static str],
    /// Where to get it by hand.
    pub source: &'static str,
}

pub const VST3: Sdk = Sdk {
    id: "vst3",
    name: "VST3 SDK",
    var: "VST3_SDK_DIR",
    probe: &["pluginterfaces/vst/ivstcomponent.h"],
    source: "https://github.com/steinbergmedia/vst3sdk (with its submodules)",
};

pub const CLAP: Sdk = Sdk {
    id: "clap",
    name: "CLAP",
    var: "CLAP_SDK_DIR",
    probe: &["include/clap/clap.h"],
    source: "https://github.com/free-audio/clap",
};

pub const ARA: Sdk = Sdk {
    id: "ara",
    name: "ARA SDK",
    var: "ARA_SDK_DIR",
    probe: &["ARA_API/ARAInterface.h", "ARA_Library/PlugIn/ARAPlug.cpp"],
    source: "git clone --recursive https://github.com/Celemony/ARA_SDK",
};

pub const XCLANG: Sdk = Sdk {
    id: "xclang",
    name: "xclang",
    var: "XCLANG_DIR",
    probe: &[if cfg!(windows) { "bin/xclang.exe" } else { "bin/xclang" }],
    source: "https://github.com/clice-io/xclang/releases",
};

/// Avid's AAX SDK: from Avid's developer site, after signing up (not
/// fetched by Ferritron).
pub const AAX: Sdk = Sdk {
    id: "aax",
    name: "AAX SDK",
    var: "AAX_SDK_DIR",
    probe: &["Interfaces/AAX.h"],
    source: "https://developer.avid.com (sign up for the AAX SDK)",
};

/// The Android NDK, for Android apps (`--target aarch64-linux-android`).
pub const NDK: Sdk = Sdk {
    id: "ndk",
    name: "Android NDK",
    var: "ANDROID_NDK_HOME",
    probe: &["meta/platforms.json"],
    source: "https://developer.android.com/ndk/downloads (or Android Studio's SDK Manager)",
};

/// Every SDK and tool, in the order tools list them.
pub const ALL: [&Sdk; 6] = [&VST3, &CLAP, &ARA, &AAX, &NDK, &XCLANG];

/// The SDK with `id`.
pub fn find(id: &str) -> Option<&'static Sdk> {
    ALL.into_iter().find(|s| s.id == id)
}

/// Where an SDK is looked for (`source` in `sdk.toml`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    Auto,
    Env,
    Managed,
    Path,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Env => "env",
            Self::Managed => "managed",
            Self::Path => "path",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "auto" => Self::Auto,
            "env" => Self::Env,
            "managed" => Self::Managed,
            "path" => Self::Path,
            _ => return None,
        })
    }
}

/// One SDK's table in `sdk.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub source: Source,
    /// For `auto` and `managed`: the version in the managed folder.
    pub version: Option<String>,
    /// For `path`.
    pub path: Option<PathBuf>,
}

/// `sdk.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub root: Option<PathBuf>,
    pub entries: BTreeMap<String, Entry>,
}

impl Settings {
    /// Reads `sdk.toml` text. Unknown SDK tables are kept (a newer Ferritron
    /// may know them); wrong values are errors.
    pub fn parse(text: &str) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("sdk.toml: {}", e.message()))?;
        let mut settings = Self::default();
        for (key, value) in &table {
            if key == "root" {
                let root = value.as_str().ok_or("sdk.toml: `root` must be a string (a folder)")?;
                settings.root = Some(PathBuf::from(root));
                continue;
            }
            let t = value.as_table().ok_or_else(|| format!("sdk.toml: `{key}` must be a table ([{key}])"))?;
            let text = |name: &str| -> Result<Option<String>, String> {
                match t.get(name) {
                    None => Ok(None),
                    Some(v) => v.as_str().map(|s| Some(s.to_string())).ok_or_else(|| format!("sdk.toml: `{key}.{name}` must be a string")),
                }
            };
            let source = match text("source")? {
                None => Source::Auto,
                Some(s) => Source::parse(&s).ok_or_else(|| format!("sdk.toml: `{key}.source` is \"{s}\", not one of auto, env, managed, path"))?,
            };
            let path = text("path")?.map(PathBuf::from);
            if source == Source::Path && path.is_none() {
                return Err(format!("sdk.toml: `{key}.source = \"path\"` needs `{key}.path`"));
            }
            settings.entries.insert(key.clone(), Entry { source, version: text("version")?, path });
        }
        Ok(settings)
    }

    /// `sdk.toml` text for these settings.
    pub fn to_toml(&self) -> String {
        let mut table = toml::Table::new();
        if let Some(root) = &self.root {
            table.insert("root".into(), slashes(root).into());
        }
        for (id, entry) in &self.entries {
            let mut t = toml::Table::new();
            t.insert("source".into(), entry.source.as_str().into());
            if let Some(version) = &entry.version {
                t.insert("version".into(), version.as_str().into());
            }
            if let Some(path) = &entry.path {
                t.insert("path".into(), slashes(path).into());
            }
            table.insert(id.clone(), t.into());
        }
        toml::to_string(&table).unwrap_or_default()
    }

    /// The settings in `sdk.toml`; empty (all `auto`) when there is none.
    pub fn load() -> Result<Self, String> {
        match settings_path().map(|p| std::fs::read_to_string(&p)) {
            Some(Ok(text)) => Self::parse(&text),
            _ => Ok(Self::default()),
        }
    }

    pub fn entry(&self, id: &str) -> Entry {
        self.entries.get(id).cloned().unwrap_or_default()
    }
}

fn slashes(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Ferritron's data folder on this computer (`%APPDATA%\Ferritron`, ...),
/// as plugins and the Editor use it.
pub fn data_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|p| p.join("Ferritron"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|p| p.join("Library/Application Support/Ferritron"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|p| p.join(".local/share"))).map(|p| p.join("ferritron"))
    }
}

/// `sdk.toml`.
pub fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("sdk.toml"))
}

/// The managed folder when nothing else says: `C:\Ferritron\SDK` on Windows,
/// `~/Ferritron/SDK` elsewhere.
pub fn default_root() -> Option<PathBuf> {
    if cfg!(windows) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        Some(PathBuf::from(format!("{drive}\\Ferritron\\SDK")))
    } else {
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join("Ferritron").join("SDK"))
    }
}

/// The managed folder: `FERRITRON_SDK_ROOT`, else `root` in `sdk.toml`, else the default.
pub fn root(settings: &Settings, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("FERRITRON_SDK_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| settings.root.clone()).or_else(default_root)
}

/// The environment variable that pins `sdk`'s version for a build.
pub fn pin_var(sdk: &Sdk) -> String {
    format!("FERRITRON_SDK_{}_VERSION", sdk.id.to_ascii_uppercase())
}

/// Orders version names: numeric parts as numbers (`3.10` after `3.9`).
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<(u64, String)> {
        v.split(['.', '-', '_', '+'])
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                (digits.parse().unwrap_or(0), p[digits.len()..].to_string())
            })
            .collect()
    };
    parts(a).cmp(&parts(b))
}

/// `dir` itself if it holds the SDK, or its only subfolder if that does.
pub fn holding(sdk: &Sdk, dir: &Path) -> Option<PathBuf> {
    let has = |d: &Path| sdk.probe.iter().all(|p| d.join(p).is_file());
    if has(dir) {
        return Some(dir.to_path_buf());
    }
    let subfolders: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    match subfolders.as_slice() {
        [only] if has(only) => Some(only.clone()),
        _ => None,
    }
}

/// The versions of `sdk` in the managed folder that hold it, newest first.
pub fn installed(sdk: &Sdk, root: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(root.join(sdk.id))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let version = e.file_name().to_string_lossy().into_owned();
            // Unfinished downloads live in dot folders.
            if version.starts_with('.') {
                return None;
            }
            holding(sdk, &e.path()).map(|dir| (version, dir))
        })
        .collect();
    found.sort_by(|a, b| compare_versions(&b.0, &a.0));
    found
}

/// Where a found SDK came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Pinned for this build (`[sdk]`), in the managed folder.
    Pinned(String),
    /// The environment variable.
    Env(&'static str),
    /// The managed folder, this version.
    Managed(String),
    /// `path` in `sdk.toml`.
    Path,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pinned(v) => write!(f, "pinned by the project ([sdk]), version {v}"),
            Self::Env(var) => write!(f, "environment variable {var}"),
            Self::Managed(v) => write!(f, "Ferritron's SDK folder, version {v}"),
            Self::Path => write!(f, "path in sdk.toml"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub dir: PathBuf,
    pub origin: Origin,
}

/// What the rule looks at; [`Context::current`] reads the real environment.
pub struct Context<'a> {
    pub settings: Settings,
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
}

fn real_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

impl Context<'static> {
    /// The environment and `sdk.toml` of this process.
    pub fn current() -> Result<Self, String> {
        Ok(Self { settings: Settings::load()?, env: &real_env })
    }
}

impl Context<'_> {
    pub fn root(&self) -> Option<PathBuf> {
        root(&self.settings, self.env)
    }

    /// Finds `sdk` by the rule (see the module docs), or says where it looked.
    pub fn resolve(&self, sdk: &Sdk) -> Result<Found, Vec<String>> {
        let mut tried = Vec::new();
        let root = self.root();
        let managed = |version: &str, tried: &mut Vec<String>| -> Option<PathBuf> {
            let Some(root) = &root else {
                tried.push("the managed folder (no root: set FERRITRON_SDK_ROOT)".into());
                return None;
            };
            let dir = root.join(sdk.id).join(version);
            let found = holding(sdk, &dir);
            if found.is_none() {
                tried.push(format!("{} (version {version})", dir.display()));
            }
            found
        };
        if let Some(version) = (self.env)(&pin_var(sdk)).map(|v| v.to_string_lossy().into_owned()).filter(|v| !v.is_empty()) {
            return match managed(&version, &mut tried) {
                Some(dir) => Ok(Found { dir, origin: Origin::Pinned(version) }),
                None => Err(tried),
            };
        }
        let entry = self.settings.entry(sdk.id);
        let from_env = |tried: &mut Vec<String>| -> Option<PathBuf> {
            let value = (self.env)(sdk.var).filter(|v| !v.is_empty());
            match value {
                None => {
                    tried.push(format!("{} (not set)", sdk.var));
                    None
                }
                Some(v) => {
                    let found = holding(sdk, Path::new(&v));
                    if found.is_none() {
                        tried.push(format!("{} = {} (no {})", sdk.var, Path::new(&v).display(), sdk.probe.join(", ")));
                    }
                    found
                }
            }
        };
        let from_managed = |tried: &mut Vec<String>| -> Option<(PathBuf, String)> {
            match &entry.version {
                Some(version) => managed(version, tried).map(|d| (d, version.clone())),
                None => {
                    let newest = root.as_deref().and_then(|r| installed(sdk, r).into_iter().next());
                    if newest.is_none() {
                        tried.push(match &root {
                            Some(r) => format!("{} (no version there)", r.join(sdk.id).display()),
                            None => "the managed folder (no root)".into(),
                        });
                    }
                    newest.map(|(v, d)| (d, v))
                }
            }
        };
        let found = match entry.source {
            Source::Auto => from_env(&mut tried)
                .map(|dir| Found { dir, origin: Origin::Env(sdk.var) })
                .or_else(|| from_managed(&mut tried).map(|(dir, v)| Found { dir, origin: Origin::Managed(v) })),
            Source::Env => from_env(&mut tried).map(|dir| Found { dir, origin: Origin::Env(sdk.var) }),
            Source::Managed => from_managed(&mut tried).map(|(dir, v)| Found { dir, origin: Origin::Managed(v) }),
            Source::Path => {
                let path = entry.path.clone().unwrap_or_default();
                let found = holding(sdk, &path);
                if found.is_none() {
                    tried.push(format!("{} (path in sdk.toml; no {})", path.display(), sdk.probe.join(", ")));
                }
                found.map(|dir| Found { dir, origin: Origin::Path })
            }
        };
        found.ok_or(tried)
    }
}

/// What to do when `sdk` is missing: the places tried and three ways to fix it.
pub fn missing_help(sdk: &Sdk, tried: &[String]) -> String {
    let mut text = format!("{} not found. Looked at:\n", sdk.name);
    for place in tried {
        text.push_str(&format!("  - {place}\n"));
    }
    let settings = settings_path().map_or_else(|| "sdk.toml".into(), |p| p.display().to_string());
    text.push_str(&format!(
        "Ferritron does not ship SDKs; get it once, accepting its license, and either:\n\
         \x20 - run `cargo ferritron sdk fetch {id}` (or use the Ferritron Editor's SDK settings), or\n\
         \x20 - set {var} to its folder (source: {source}), or\n\
         \x20 - name its folder in {settings} ([{id}] source = \"path\", path = \"...\").\n\
         `cargo ferritron sdk status` shows what is found where.",
        id = sdk.id,
        var = sdk.var,
        source = sdk.source,
    ));
    text
}

/// For build scripts: rebuild when anything the rule reads changes.
fn rerun_lines(sdk: &Sdk) {
    for var in [sdk.var.to_string(), pin_var(sdk), "FERRITRON_SDK_ROOT".into(), "FERRITRON_SDK_STAMP".into()] {
        println!("cargo::rerun-if-env-changed={var}");
    }
    // Only an existing file: a missing one would rebuild every time.
    if let Some(path) = settings_path().filter(|p| p.is_file()) {
        println!("cargo::rerun-if-changed={}", path.display());
    }
}

/// For build scripts: the folder of `sdk` by the rule; stops the build with
/// [`missing_help`] and `hint` when it is not found (or `sdk.toml` is broken).
pub fn require(sdk: &Sdk, hint: &str) -> PathBuf {
    rerun_lines(sdk);
    let context = Context::current().unwrap_or_else(|e| crate::fail(&e));
    match context.resolve(sdk) {
        Ok(found) => found.dir,
        Err(tried) => {
            let mut help = missing_help(sdk, &tried);
            if !hint.is_empty() {
                help.push('\n');
                help.push_str(hint);
            }
            crate::fail(&help)
        }
    }
}

/// For build scripts: the folder of `sdk` if the rule finds it.
pub fn optional(sdk: &Sdk) -> Option<PathBuf> {
    rerun_lines(sdk);
    Context::current().ok()?.resolve(sdk).ok().map(|f| f.dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake SDK folder with `sdk`'s probe files.
    fn make(dir: &Path, sdk: &Sdk) {
        for p in sdk.probe {
            let file = dir.join(p);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "").unwrap();
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferritron-build-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn resolve(settings: &str, vars: &[(&str, &Path)], root: &Path) -> Result<Found, Vec<String>> {
        let vars: Vec<(String, OsString)> = vars.iter().map(|(k, v)| (k.to_string(), v.as_os_str().to_owned())).collect();
        let root = root.as_os_str().to_owned();
        let env = move |name: &str| -> Option<OsString> {
            if name == "FERRITRON_SDK_ROOT" {
                return Some(root.clone());
            }
            vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        };
        Context { settings: Settings::parse(settings).unwrap(), env: &env }.resolve(&CLAP)
    }

    #[test]
    fn the_four_sources_resolve_as_documented() {
        let dir = temp("sources");
        let root = dir.join("root");
        let from_env = dir.join("env-clap");
        let custom = dir.join("custom");
        make(&from_env, &CLAP);
        make(&root.join("clap/1.2.9"), &CLAP);
        make(&root.join("clap/1.2.10/clap-195b42a"), &CLAP); // as an archive unpacks
        make(&custom, &CLAP);
        let env = [("CLAP_SDK_DIR", from_env.as_path())];

        // No sdk.toml (1.1): the environment variable.
        assert_eq!(resolve("", &env, &root).unwrap().origin, Origin::Env("CLAP_SDK_DIR"));
        // auto without the variable: the newest managed version (numeric order), in its subfolder.
        let found = resolve("", &[], &root).unwrap();
        assert_eq!(found.origin, Origin::Managed("1.2.10".into()));
        assert!(found.dir.ends_with("clap-195b42a"));
        // auto with a chosen version.
        assert_eq!(resolve("[clap]\nversion = \"1.2.9\"", &[], &root).unwrap().origin, Origin::Managed("1.2.9".into()));
        // env: only the variable.
        assert!(resolve("[clap]\nsource = \"env\"", &[], &root).is_err());
        // managed: ignores the variable.
        assert_eq!(resolve("[clap]\nsource = \"managed\"", &env, &root).unwrap().origin, Origin::Managed("1.2.10".into()));
        // path.
        let text = format!("[clap]\nsource = \"path\"\npath = \"{}\"", custom.display().to_string().replace('\\', "/"));
        assert_eq!(resolve(&text, &env, &root).unwrap(), Found { dir: custom.clone(), origin: Origin::Path });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pinned_version_wins_and_must_exist() {
        let dir = temp("pinned");
        let root = dir.join("root");
        make(&root.join("clap/1.2.8"), &CLAP);
        let from_env = dir.join("env-clap");
        make(&from_env, &CLAP);
        let pin = PathBuf::from("1.2.8");
        let env = [("CLAP_SDK_DIR", from_env.as_path()), ("FERRITRON_SDK_CLAP_VERSION", pin.as_path())];
        assert_eq!(resolve("", &env, &root).unwrap().origin, Origin::Pinned("1.2.8".into()));
        let missing = PathBuf::from("9.9.9");
        let tried = resolve("", &[("FERRITRON_SDK_CLAP_VERSION", missing.as_path())], &root).unwrap_err();
        assert!(tried[0].contains("9.9.9"), "{tried:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_found_lists_every_place() {
        let dir = temp("missing");
        let tried = resolve("", &[], &dir).unwrap_err();
        assert_eq!(tried.len(), 2, "{tried:?}");
        let help = missing_help(&CLAP, &tried);
        assert!(help.contains("CLAP_SDK_DIR") && help.contains("cargo ferritron sdk fetch clap"), "{help}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_roundtrip_and_reject_mistakes() {
        let text = "root = \"C:/SDK\"\n[vst3]\nsource = \"managed\"\nversion = \"3.8.1\"\n[ara]\nsource = \"path\"\npath = \"D:/ARA\"\n";
        let settings = Settings::parse(text).unwrap();
        assert_eq!(settings.entry("vst3"), Entry { source: Source::Managed, version: Some("3.8.1".into()), path: None });
        assert_eq!(Settings::parse(&settings.to_toml()).unwrap(), settings);
        assert!(Settings::parse("[vst3]\nsource = \"web\"").unwrap_err().contains("not one of"));
        assert!(Settings::parse("[vst3]\nsource = \"path\"").unwrap_err().contains("needs"));
        assert_eq!(compare_versions("3.10.0", "3.9.2"), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions("1.2.10", "1.2.9"), std::cmp::Ordering::Greater);
    }
}
