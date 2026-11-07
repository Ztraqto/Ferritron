//! The SDKs Ferritron can fetch for the developer, and fetching them.
//!
//! Ferritron ships no SDK. This crate fetches one only when a person asks
//! and has accepted its license (the CLI asks in the terminal, the Ferritron
//! Editor in a sheet); callers must show [`Release::license`] and
//! [`Release::license_url`] and get that consent first, then call [`fetch`]
//! and [`record_consent`].
//!
//! - [`catalog`]: the versions checked with this Ferritron release
//!   (`sdk-catalog.toml`), with where to get each part and how to check it.
//! - [`fetch`]: downloads every part over HTTPS (the system's `curl`),
//!   checks it (SHA-256, or the git commit a source archive was made from),
//!   unpacks it (the system's `tar`; `.xz` decoded here), assembles
//!   submodules, checks the SDK's files, and only then moves it to
//!   `<root>/<id>/<version>`. A failure or a cancel leaves nothing behind.
//! - [`use_version`], [`set_source`], [`remove`]: `sdk.toml` and the
//!   managed folder (see `ferritron_build::sdk` for how builds find SDKs).
//! - [`fetch_macos_sdk`]: has xclang fetch Apple's macOS SDK (after the
//!   person accepted Apple's terms).

use ferritron_build::sdk::{self, Sdk, Settings, Source};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CATALOG: &str = include_str!("../sdk-catalog.toml");

/// How a downloaded part is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The file's SHA-256 (release files).
    Sha256(String),
    /// The git commit a source archive was made from (its top folder ends
    /// with the commit's first seven characters).
    Commit(String),
}

/// One download of a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// Folder inside the SDK ("" for the SDK itself, else a submodule).
    pub into: String,
    pub url: String,
    pub check: Check,
    /// `<os>-<arch>` of the computer this part is for (tools with one
    /// archive per host); `None` for every computer.
    pub host: Option<String>,
    /// Bytes, when known (progress).
    pub size: Option<u64>,
}

/// A version of an SDK Ferritron can fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub id: String,
    pub version: String,
    /// Checked with this Ferritron release: chosen by default.
    pub recommended: bool,
    pub license: String,
    pub license_url: String,
    /// The oldest Ferritron the version works with.
    pub ferritron: String,
    pub notice: Option<String>,
    pub parts: Vec<Part>,
}

impl Release {
    /// The parts for this computer.
    pub fn parts_here(&self) -> Vec<&Part> {
        let here = host();
        self.parts.iter().filter(|p| p.host.as_deref().is_none_or(|h| h == here)).collect()
    }

    /// Bytes to download, when every part's size is known.
    pub fn download_size(&self) -> Option<u64> {
        self.parts_here().iter().map(|p| p.size).sum()
    }
}

/// `<os>-<arch>` of this computer, as the catalog names hosts.
pub fn host() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn parse_catalog(text: &str) -> Result<Vec<Release>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("sdk-catalog.toml: {}", e.message()))?;
    let mut releases = Vec::new();
    for (id, list) in &table {
        for item in list.as_array().ok_or_else(|| format!("sdk-catalog.toml: {id} must be [[{id}]] tables"))? {
            let text = |key: &str| item.get(key).and_then(|v| v.as_str()).map(str::to_string);
            let need = |key: &str| text(key).ok_or_else(|| format!("sdk-catalog.toml: {id} needs `{key}`"));
            let mut parts = Vec::new();
            for part in item.get("parts").and_then(|p| p.as_array()).ok_or_else(|| format!("sdk-catalog.toml: {id} needs `parts`"))? {
                let field = |key: &str| part.get(key).and_then(|v| v.as_str()).map(str::to_string);
                let check = match (field("sha256"), field("commit")) {
                    (Some(sha), None) => Check::Sha256(sha.to_ascii_lowercase()),
                    (None, Some(commit)) => Check::Commit(commit.to_ascii_lowercase()),
                    _ => return Err(format!("sdk-catalog.toml: each part of {id} needs exactly one of `sha256`, `commit`")),
                };
                parts.push(Part {
                    into: field("into").unwrap_or_default(),
                    url: field("url").ok_or_else(|| format!("sdk-catalog.toml: a part of {id} needs `url`"))?,
                    check,
                    host: field("host"),
                    size: part.get("size").and_then(|v| v.as_integer()).map(|n| n as u64),
                });
            }
            releases.push(Release {
                id: id.clone(),
                version: need("version")?,
                recommended: item.get("recommended").and_then(|v| v.as_bool()).unwrap_or(false),
                license: need("license")?,
                license_url: need("license-url")?,
                ferritron: need("ferritron")?,
                notice: text("notice"),
                parts,
            });
        }
    }
    Ok(releases)
}

/// Every version Ferritron knows, by SDK, newest first.
pub fn catalog() -> Vec<Release> {
    let mut releases = parse_catalog(CATALOG).expect("the built-in catalog is valid");
    releases.sort_by(|a, b| a.id.cmp(&b.id).then(sdk::compare_versions(&b.version, &a.version)));
    releases
}

/// The versions of SDK `id`, recommended first, then newest first; only
/// those with a part for this computer.
pub fn releases(id: &str) -> Vec<Release> {
    let mut list: Vec<Release> = catalog().into_iter().filter(|r| r.id == id && !r.parts_here().is_empty()).collect();
    list.sort_by(|a, b| b.recommended.cmp(&a.recommended).then(sdk::compare_versions(&b.version, &a.version)));
    list
}

/// Version `version` of SDK `id`, or its recommended version.
pub fn release(id: &str, version: Option<&str>) -> Option<Release> {
    let list = releases(id);
    match version {
        Some(v) => list.into_iter().find(|r| r.version == v),
        None => list.into_iter().next(),
    }
}

/// What [`fetch`] is doing, for a progress display.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// Downloading part `index` of `count`: bytes so far, and in all if known.
    Downloading { index: usize, count: usize, bytes: u64, total: Option<u64> },
    Checking { index: usize },
    Unpacking { index: usize },
    /// Moving the finished SDK into place.
    Installing,
}

/// The managed folder to fetch into (`FERRITRON_SDK_ROOT`, `sdk.toml`, default).
pub fn managed_root() -> Result<PathBuf, String> {
    let settings = Settings::load()?;
    sdk::root(&settings, &|v| std::env::var_os(v)).ok_or_else(|| "no folder for SDKs: set FERRITRON_SDK_ROOT".into())
}

/// Removes a folder when dropped (unless disarmed): no half SDK survives an error.
struct Cleanup(Option<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The system's programs, not whatever a shell put first in `PATH` (Git's
/// GNU tar cannot read `C:\` paths).
fn system_program(name: &str) -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        let path = root.join("System32").join(format!("{name}.exe"));
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

/// Fetches `release` into `<root>/<id>/<version>` (see the crate docs) and
/// returns that folder. The caller has the person's consent to the license.
/// `cancel` stops it between steps and during downloads.
pub fn fetch(release: &Release, root: &Path, progress: &mut dyn FnMut(Progress), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let spec = sdk::find(&release.id).ok_or_else(|| format!("unknown SDK `{}`", release.id))?;
    let target = root.join(&release.id).join(&release.version);
    if target.exists() {
        return Err(format!("{} already exists; remove it first to fetch {} {} again", target.display(), spec.name, release.version));
    }
    let staging = root.join(&release.id).join(format!(".fetching-{}-{}", release.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| cannot_create(&staging, e))?;
    let cleanup = Cleanup(Some(staging.clone()));
    let stopped = || if cancel.load(Ordering::Relaxed) { Err("cancelled".to_string()) } else { Ok(()) };

    let parts = release.parts_here();
    if parts.is_empty() {
        return Err(format!("{} {} has no download for this computer ({})", spec.name, release.version, host()));
    }
    let assembled = staging.join("sdk");
    for (index, part) in parts.iter().enumerate() {
        stopped()?;
        let ext = if part.url.ends_with(".tar.xz") { "tar.xz" } else { "tar.gz" };
        let archive = staging.join(format!("part{index}.{ext}"));
        download(&part.url, &archive, part.size, &mut |bytes, total| progress(Progress::Downloading { index, count: parts.len(), bytes, total }), cancel)?;
        stopped()?;
        progress(Progress::Checking { index });
        if let Check::Sha256(expected) = &part.check {
            let actual = sha256_file(&archive)?;
            if &actual != expected {
                return Err(format!("{}: the download does not match the catalog (SHA-256 {actual}, expected {expected}); nothing was installed", part.url));
            }
        }
        progress(Progress::Unpacking { index });
        let unpacked = staging.join(format!("part{index}"));
        unpack(&archive, &unpacked)?;
        let _ = std::fs::remove_file(&archive);
        let top = single_folder(&unpacked)?;
        if let Check::Commit(commit) = &part.check {
            let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !commit.get(..7).is_some_and(|short| name.ends_with(&format!("-{short}"))) {
                return Err(format!("{}: the archive is not of commit {commit} (its folder is {name}); nothing was installed", part.url));
            }
        }
        let into = if part.into.is_empty() { assembled.clone() } else { assembled.join(&part.into) };
        // A submodule's folder exists, empty, in its parent's archive.
        if into.is_dir() {
            std::fs::remove_dir_all(&into).map_err(|e| format!("cannot replace {}: {e}", into.display()))?;
        }
        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent).map_err(|e| cannot_create(parent, e))?;
        }
        std::fs::rename(&top, &into).map_err(|e| format!("cannot move {} to {}: {e}", top.display(), into.display()))?;
    }
    stopped()?;
    if sdk::holding(spec, &assembled).is_none() {
        return Err(format!("{} {}: the download does not contain {}; nothing was installed", spec.name, release.version, spec.probe.join(", ")));
    }
    progress(Progress::Installing);
    std::fs::rename(&assembled, &target).map_err(|e| format!("cannot move the SDK to {}: {e}", target.display()))?;
    drop(cleanup);
    Ok(target)
}

fn cannot_create(dir: &Path, e: std::io::Error) -> String {
    format!("cannot create {}: {e} (choose another folder for SDKs: FERRITRON_SDK_ROOT, or `root` in sdk.toml)", dir.display())
}

/// Reads a small text resource (a GitHub API answer, a checksum list) over
/// HTTPS with the system's curl; fails after 15 seconds.
pub fn http_get(url: &str) -> Result<String, String> {
    if !url.starts_with("https://") {
        return Err(format!("{url}: only HTTPS is allowed"));
    }
    let curl = system_program("curl");
    let output = Command::new(&curl)
        .args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https", "--max-time", "15"])
        .args(["--header", "Accept: application/vnd.github+json", "--user-agent", "Ferritron"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl ({}): {e}", curl.display()))?;
    if !output.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{url}: the answer is not text"))
}

/// Downloads `url` to `file` with the system's curl (HTTPS only; honors
/// `HTTPS_PROXY`), reporting bytes as they arrive; `cancel` stops it.
pub fn download(url: &str, file: &Path, size: Option<u64>, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("{url}: only HTTPS downloads are allowed"));
    }
    let curl = system_program("curl");
    let mut child = Command::new(&curl)
        .args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https", "--retry", "2", "--output"])
        .arg(file)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl ({}): {e}", curl.display()))?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        progress(std::fs::metadata(file).map(|m| m.len()).unwrap_or(0), size);
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    progress(std::fs::metadata(file).map(|m| m.len()).unwrap_or(0), size);
                    return Ok(());
                }
                let mut message = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut err, &mut message);
                }
                return Err(format!(
                    "downloading {url} failed: {}. Offline or behind a proxy? Set HTTPS_PROXY, or put the SDK in the managed folder by hand (<root>/<id>/<version>).",
                    message.trim()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => return Err(format!("curl: {e}")),
        }
    }
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks a `.tar.gz` or `.tar.xz` into `dir` (the system's tar; `.xz` is
/// decoded here first, since not every system tar reads it).
fn unpack(archive: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| cannot_create(dir, e))?;
    let mut tar_file = archive.to_path_buf();
    if archive.to_string_lossy().ends_with(".xz") {
        tar_file = archive.with_extension("");
        let input = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        let output = std::fs::File::create(&tar_file).map_err(|e| format!("{}: {e}", tar_file.display()))?;
        lzma_rs::xz_decompress(&mut std::io::BufReader::new(input), &mut std::io::BufWriter::new(output))
            .map_err(|e| format!("{}: not a valid .xz file ({e:?})", archive.display()))?;
    }
    let tar = system_program("tar");
    let status = Command::new(&tar)
        .arg("-xf")
        .arg(&tar_file)
        .arg("-C")
        .arg(dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run tar ({}): {e}", tar.display()))?;
    if tar_file != archive {
        let _ = std::fs::remove_file(&tar_file);
    }
    if !status.success() {
        return Err(format!("unpacking {} failed", archive.display()));
    }
    Ok(())
}

/// The one folder an archive unpacked into.
fn single_folder(dir: &Path) -> Result<PathBuf, String> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("pax_global_header")))
        .collect();
    match entries.as_slice() {
        [only] if only.is_dir() => Ok(only.clone()),
        _ => Err(format!("{}: expected the archive to hold one folder", dir.display())),
    }
}

fn save(settings: &Settings) -> Result<(), String> {
    let path = sdk::settings_path().ok_or("no data folder for sdk.toml")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = format!("# Where Ferritron finds SDKs (see `cargo ferritron sdk status`).\n{}", settings.to_toml());
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Builds use version `version` of `sdk` from the managed folder (from the
/// next build on). A `source` of `env` or `path` becomes `managed`; `auto`
/// stays (an environment variable, if set, still wins).
pub fn use_version(sdk: &Sdk, version: &str) -> Result<(), String> {
    let mut settings = Settings::load()?;
    let entry = settings.entries.entry(sdk.id.to_string()).or_default();
    if matches!(entry.source, Source::Env | Source::Path) {
        entry.source = Source::Managed;
    }
    entry.version = Some(version.to_string());
    save(&settings)
}

/// Sets where builds look for `sdk` (`path` for [`Source::Path`]).
pub fn set_source(sdk: &Sdk, source: Source, path: Option<PathBuf>) -> Result<(), String> {
    if source == Source::Path && path.is_none() {
        return Err(format!("`path` needs a folder for {}", sdk.name));
    }
    let mut settings = Settings::load()?;
    let entry = settings.entries.entry(sdk.id.to_string()).or_default();
    entry.source = source;
    entry.path = if source == Source::Path { path } else { None };
    save(&settings)
}

/// Deletes version `version` of `sdk` from the managed folder. A version
/// `sdk.toml` chose is forgotten there too.
pub fn remove(sdk: &Sdk, version: &str) -> Result<PathBuf, String> {
    let root = managed_root()?;
    let dir = root.join(sdk.id).join(version);
    if version.is_empty() || version.contains(['/', '\\']) || version.starts_with('.') || !dir.is_dir() {
        return Err(format!("{} {version} is not in {}", sdk.name, root.join(sdk.id).display()));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete {}: {e}", dir.display()))?;
    let mut settings = Settings::load()?;
    if let Some(entry) = settings.entries.get_mut(sdk.id)
        && entry.version.as_deref() == Some(version)
    {
        entry.version = None;
        save(&settings)?;
    }
    Ok(dir)
}

/// Records that a person accepted `license` for `id` `version`, next to
/// `sdk.toml` (`sdk-consent.toml`), so it can be looked up later.
pub fn record_consent(id: &str, version: &str, license: &str, by: &str) -> Result<(), String> {
    let path = sdk::data_dir().ok_or("no data folder")?.join("sdk-consent.toml");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut entry = toml::Table::new();
    entry.insert("sdk".into(), id.into());
    entry.insert("version".into(), version.into());
    entry.insert("license".into(), license.into());
    entry.insert("accepted-unix-time".into(), (seconds as i64).into());
    entry.insert("through".into(), by.into());
    let mut wrapper = toml::Table::new();
    wrapper.insert("consent".into(), toml::Value::Array(vec![entry.into()]));
    let text = toml::to_string(&wrapper).map_err(|e| e.to_string())?;
    // `[[consent]]` tables append cleanly.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(file, "{text}").map_err(|e| format!("{}: {e}", path.display()))
}

/// What Apple's terms say about the macOS SDK on other computers; shown
/// before [`fetch_macos_sdk`].
pub const MACOS_SDK_NOTICE: &str = "The macOS SDK is Apple's, used under Apple's terms, which allow it on Apple hardware. \
Building for macOS on Windows or Linux with it is your decision under those terms; Ferritron and xclang do not make it for you. \
xclang downloads it from Apple (Command Line Tools, no Apple ID).";

/// The xclang program in an xclang folder.
pub fn xclang_program(dir: &Path) -> PathBuf {
    dir.join("bin").join(if cfg!(windows) { "xclang.exe" } else { "xclang" })
}

/// Whether xclang in `dir` has a macOS SDK (`sdk/macos`).
pub fn has_macos_sdk(dir: &Path) -> bool {
    dir.join("sdk").join("macos").join("SDKSettings.json").is_file() || dir.join("sdk").join("macos").is_dir()
}

/// Has xclang fetch Apple's macOS SDK. Only after the person accepted
/// Apple's terms ([`MACOS_SDK_NOTICE`]; xclang prints them without
/// `--accept-license`).
pub fn fetch_macos_sdk(xclang_dir: &Path) -> Result<(), String> {
    let program = xclang_program(xclang_dir);
    let status = Command::new(&program)
        .args(["sdk", "fetch", "macos", "--accept-license"])
        .status()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    if !status.success() {
        return Err("xclang could not fetch the macOS SDK".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_complete() {
        let all = catalog();
        for id in ["vst3", "clap", "ara", "xclang"] {
            let list: Vec<&Release> = all.iter().filter(|r| r.id == id).collect();
            assert!(!list.is_empty(), "{id}");
            assert_eq!(list.iter().filter(|r| r.recommended).count(), 1, "{id}: one recommended version");
            for r in list {
                assert!(r.license_url.starts_with("https://") && !r.license.is_empty());
                assert!(r.parts.iter().any(|p| p.into.is_empty()), "{id} {}: the SDK itself", r.version);
                for p in &r.parts {
                    assert!(p.url.starts_with("https://"), "{}", p.url);
                    match &p.check {
                        Check::Sha256(s) => assert_eq!(s.len(), 64),
                        Check::Commit(c) => assert!(c.len() == 40 && p.url.ends_with(c.as_str()), "{}", p.url),
                    }
                }
            }
        }
        assert_eq!(release("vst3", None).unwrap().version, "3.8.1");
        assert!(release("clap", Some("1.2.9")).is_some());
    }

    #[test]
    fn a_bad_catalog_is_rejected() {
        assert!(parse_catalog("[[vst3]]\nversion = \"1\"").is_err());
        let both = "[[clap]]\nversion=\"1\"\nlicense=\"x\"\nlicense-url=\"https://x\"\nferritron=\"1.2.0\"\nparts=[{url=\"https://x\", sha256=\"a\", commit=\"b\"}]";
        assert!(parse_catalog(both).unwrap_err().contains("exactly one"));
    }

    /// Makes `name.tar.gz` holding `top/<files>` with the system's tar.
    fn archive(dir: &Path, top: &str, files: &[&str]) -> PathBuf {
        let src = dir.join("src");
        let _ = std::fs::remove_dir_all(&src);
        for f in files {
            let path = src.join(top).join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let out = dir.join(format!("{top}.tar.gz"));
        let ok = Command::new(system_program("tar")).arg("-czf").arg(&out).arg("-C").arg(&src).arg(top).status().unwrap().success();
        assert!(ok);
        out
    }

    #[test]
    fn unpacking_finds_the_archive_folder_and_checks_it() {
        let dir = std::env::temp_dir().join(format!("ferritron-sdk-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = archive(&dir, "free-audio-clap-195b42a", &["include/clap/clap.h"]);
        let out = dir.join("out");
        unpack(&file, &out).unwrap();
        let top = single_folder(&out).unwrap();
        assert!(top.ends_with("free-audio-clap-195b42a"));
        assert!(sdk::holding(&sdk::CLAP, &top).is_some());
        assert_eq!(sha256_file(&file).unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_fetch_leaves_nothing() {
        let root = std::env::temp_dir().join(format!("ferritron-sdk-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let release = Release {
            id: "clap".into(),
            version: "0.0.0".into(),
            recommended: false,
            license: "MIT".into(),
            license_url: "https://example.invalid".into(),
            ferritron: "1.2.0".into(),
            notice: None,
            parts: vec![Part { into: String::new(), url: "http://example.invalid/clap.tar.gz".into(), check: Check::Commit("0".repeat(40)), host: None, size: None }],
        };
        let err = fetch(&release, &root, &mut |_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("only HTTPS"), "{err}");
        let left: Vec<_> = std::fs::read_dir(root.join("clap")).unwrap().flatten().collect();
        assert!(left.is_empty(), "nothing left behind: {left:?}");
        let cancelled = fetch(&release, &root, &mut |_| {}, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(cancelled, "cancelled");
        let _ = std::fs::remove_dir_all(root);
    }
}
