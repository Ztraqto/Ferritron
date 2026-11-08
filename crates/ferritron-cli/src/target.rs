//! What `bundle` builds for: this computer, or another target (`--target`).
//!
//! macOS targets build natively on a Mac. From Windows or Linux they build
//! through xclang (`xclang cargo build`), found like an SDK (`XCLANG_DIR`,
//! `sdk.toml`, Ferritron's SDK folder), with Apple's macOS SDK that xclang
//! fetches after the developer accepts Apple's terms. Such bundles are only
//! ad-hoc signed unless `rcodesign` is installed; distribution needs signing
//! and notarizing on a Mac (or CI).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The oldest macOS bundles run on, unless `MACOSX_DEPLOYMENT_TARGET` says.
/// xclang's own minimum; native Mac builds use the same.
pub const MACOS_MINIMUM: &str = "13.0";

/// The oldest iOS / iPadOS apps run on.
pub const IOS_MINIMUM: &str = "15.0";

/// Both architectures of a universal macOS build.
pub const UNIVERSAL_PARTS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// `None`: this computer (no `--target`).
    pub triple: Option<String>,
    /// `windows`, `macos`, `linux` (as `std::env::consts::OS`).
    pub os: String,
    /// `x86_64`, `aarch64`, or `universal`.
    pub arch: String,
}

impl Target {
    pub fn parse(triple: Option<&str>) -> Result<Self, String> {
        let Some(triple) = triple else {
            return Ok(Self { triple: None, os: std::env::consts::OS.into(), arch: std::env::consts::ARCH.into() });
        };
        let arch = triple.split('-').next().unwrap_or_default();
        // The browser: WebAssembly with WASI's basics (a shim in the page provides them).
        if matches!(triple, "web" | "wasm32-wasip1") {
            return Ok(Self { triple: Some("wasm32-wasip1".into()), os: "web".into(), arch: "wasm32".into() });
        }
        // Phones and tablets: apps (standalone, and AUv3 on iOS).
        let triple = match triple {
            "android" => "aarch64-linux-android",
            "ios" => "aarch64-apple-ios",
            t => t,
        };
        if let Some(arch) = triple.strip_suffix("-linux-android") {
            if !matches!(arch, "aarch64" | "x86_64") {
                return Err(format!("unsupported target `{triple}` (aarch64-linux-android, x86_64-linux-android)"));
            }
            return Ok(Self { triple: Some(triple.into()), os: "android".into(), arch: arch.into() });
        }
        if matches!(triple, "aarch64-apple-ios" | "aarch64-apple-ios-sim") {
            return Ok(Self { triple: Some(triple.into()), os: "ios".into(), arch: "aarch64".into() });
        }
        let os = if triple.ends_with("-apple-darwin") {
            "macos"
        } else if triple.contains("-windows") {
            "windows"
        } else if triple.contains("-linux") {
            "linux"
        } else {
            return Err(format!("unsupported target `{triple}` (e.g. web, android, ios, aarch64-apple-darwin, universal-apple-darwin)"));
        };
        if !matches!(arch, "x86_64" | "aarch64" | "universal") || (arch == "universal" && os != "macos") {
            return Err(format!("unsupported target `{triple}`"));
        }
        Ok(Self { triple: Some(triple.into()), os: os.into(), arch: arch.into() })
    }

    pub fn is_macos(&self) -> bool {
        self.os == "macos"
    }

    /// macOS or Linux: bundles get the `ferritron-host` helper.
    pub fn is_unix(&self) -> bool {
        matches!(self.os.as_str(), "macos" | "linux")
    }

    /// iPhone, iPad or Android: apps only.
    pub fn is_mobile(&self) -> bool {
        matches!(self.os.as_str(), "ios" | "android")
    }

    /// Apple platforms: bundles are code-signed.
    pub fn is_apple(&self) -> bool {
        matches!(self.os.as_str(), "macos" | "ios")
    }

    /// The browser (`--target web`): a WAM, built for `wasm32-wasip1`.
    pub fn is_web(&self) -> bool {
        self.os == "web"
    }

    /// A macOS target built on another OS: through xclang.
    pub fn crosses_to_macos(&self) -> bool {
        self.is_macos() && std::env::consts::OS != "macos"
    }

    /// The cargo `--target`s to build (two for universal), none for this computer.
    pub fn cargo_triples(&self) -> Vec<String> {
        match self.triple.as_deref() {
            None => Vec::new(),
            Some("universal-apple-darwin") => UNIVERSAL_PARTS.iter().map(|t| t.to_string()).collect(),
            Some(t) => vec![t.to_string()],
        }
    }

    pub fn deployment_target(&self) -> Option<String> {
        self.is_macos().then(|| std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| MACOS_MINIMUM.into()))
    }
}

/// The xclang folder, with Apple's macOS SDK in it, or what to do.
pub fn xclang() -> Result<PathBuf, String> {
    let context = ferritron_build::sdk::Context::current()?;
    let found = context.resolve(&ferritron_build::sdk::XCLANG).map_err(|tried| {
        format!(
            "building for macOS on {} needs xclang (https://github.com/clice-io/xclang), which was not found.\n\
             Looked at: {}\n\
             Get it with `cargo ferritron sdk fetch xclang` (or the Ferritron Editor), or set XCLANG_DIR to its folder.\n\
             macOS 向けのビルドには xclang が必要ですが、見つかりませんでした。\n\
             `cargo ferritron sdk fetch xclang`（または Ferritron Editor）で取得するか、XCLANG_DIR にフォルダを設定してください。",
            std::env::consts::OS,
            tried.join("; ")
        )
    })?;
    if !ferritron_sdk::has_macos_sdk(&found.dir) {
        return Err(format!(
            "xclang ({}) has no macOS SDK yet. Apple's SDK is used under Apple's terms; to accept them and fetch it, run\n\
             \x20 cargo ferritron sdk fetch macos   (or: xclang sdk fetch macos --accept-license)\n\
             xclang に macOS SDK がまだありません。Apple の利用規約を確認して同意したうえで、上のコマンドで取得してください。",
            found.dir.display()
        ));
    }
    Ok(found.dir)
}

/// Joins per-architecture Mach-O files into one universal file.
pub fn lipo(inputs: &[PathBuf], output: &Path) -> Result<(), String> {
    let program = if std::env::consts::OS == "macos" {
        PathBuf::from("lipo")
    } else {
        let exe = if cfg!(windows) { "llvm-lipo.exe" } else { "llvm-lipo" };
        xclang().map(|d| d.join("bin").join(exe)).ok().filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(exe))
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let status = Command::new(&program)
        .arg("-create")
        .args(inputs)
        .arg("-output")
        .arg(output)
        .status()
        .map_err(|e| format!("cannot run {} to make a universal binary: {e}", program.display()))?;
    if !status.success() {
        return Err(format!("{} failed to make {}", program.display(), output.display()));
    }
    Ok(())
}

/// A C compiler for `triple`: the system's (`CC`, `cc`) on its own OS, xclang's clang across.
pub fn c_compiler(target: &Target, triple: Option<&str>) -> Result<(PathBuf, Vec<String>), String> {
    let mut args = Vec::new();
    if let Some(triple) = triple {
        args.push(format!("--target={triple}"));
    }
    if let Some(minimum) = target.deployment_target() {
        args.push(format!("-mmacos-version-min={minimum}"));
    }
    if target.crosses_to_macos() {
        let exe = if cfg!(windows) { "clang.exe" } else { "clang" };
        return Ok((xclang()?.join("bin").join(exe), args));
    }
    // On the target's own OS, `--target` only when one was given.
    Ok((PathBuf::from(std::env::var("CC").unwrap_or_else(|_| "cc".into())), args))
}

/// Signs a macOS bundle: `codesign` on a Mac (ad hoc, or the identity in
/// FERRITRON_CODESIGN_IDENTITY); elsewhere `rcodesign` if installed (ad
/// hoc); else only the linker's ad-hoc signature of the binary remains.
pub fn sign_macos(root: &Path) -> Result<(), String> {
    if std::env::consts::OS == "macos" {
        return codesign(root);
    }
    if Command::new("rcodesign").arg("--version").output().is_ok_and(|o| o.status.success()) {
        for (target, entitlements) in nested_code(root) {
            let mut cmd = Command::new("rcodesign");
            cmd.arg("sign");
            if let Some(entitlements) = &entitlements {
                cmd.arg("--entitlements-xml-path").arg(entitlements);
            }
            let status = cmd.arg(&target).status().map_err(|e| format!("cannot run rcodesign: {e}"))?;
            if !status.success() {
                return Err(format!("rcodesign failed for {}", target.display()));
            }
        }
        println!("Signed {} ad hoc with rcodesign", root.display());
        return Ok(());
    }
    println!(
        "Note: {} carries only the linker's ad-hoc signature. To distribute it, sign and notarize on a Mac \
         (`cargo ferritron notarize`) or install rcodesign.",
        root.display()
    );
    Ok(())
}

/// Code inside a bundle, innermost first, with the entitlements of each
/// (an app extension's: `<out dir>/<extension name>.entitlements`), then the bundle.
fn nested_code(root: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let list = |dir: PathBuf| -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort();
        entries
    };
    let contents = if root.join("Contents").is_dir() { root.join("Contents") } else { root.to_path_buf() };
    let mut code: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for item in list(contents.join("Frameworks")) {
        code.push((item, None));
    }
    for appex in list(contents.join("PlugIns")).into_iter().filter(|p| p.extension().is_some_and(|e| e == "appex")) {
        let entitlements = root.parent().zip(appex.file_stem()).map(|(dir, stem)| dir.join(format!("{}.entitlements", stem.to_string_lossy())));
        code.push((appex, entitlements.filter(|e| e.is_file())));
    }
    code.push((contents.join("Helpers/ferritron-host"), None));
    code.push((root.to_path_buf(), None));
    code.into_iter().filter(|(p, _)| p.exists()).collect()
}

fn codesign(root: &Path) -> Result<(), String> {
    let identity = std::env::var("FERRITRON_CODESIGN_IDENTITY").unwrap_or_else(|_| "-".into());
    // Nested code first, then the bundle.
    for (target, entitlements) in nested_code(root) {
        let mut cmd = Command::new("codesign");
        cmd.args(["--force", "--sign", &identity]);
        if identity != "-" {
            cmd.args(["--timestamp", "--options", "runtime"]);
        }
        if let Some(entitlements) = &entitlements {
            cmd.arg("--entitlements").arg(entitlements);
        }
        let status = cmd.arg(&target).status().map_err(|e| format!("cannot run codesign: {e}"))?;
        if !status.success() {
            return Err(format!("codesign failed for {}", target.display()));
        }
    }
    Ok(())
}

/// `cargo ferritron notarize <bundle>`: sends a signed bundle to Apple's
/// notary service and staples the ticket (on a Mac, `xcrun notarytool`).
/// Credentials: FERRITRON_NOTARY_PROFILE (a keychain profile made with
/// `xcrun notarytool store-credentials`), or an App Store Connect API key in
/// FERRITRON_NOTARY_KEY (.p8 file), FERRITRON_NOTARY_KEY_ID and
/// FERRITRON_NOTARY_ISSUER.
pub fn notarize(bundle: &Path) -> Result<(), String> {
    if !bundle.exists() {
        return Err(format!("{} does not exist", bundle.display()));
    }
    if std::env::consts::OS != "macos" {
        return Err("notarizing needs a Mac (xcrun notarytool) for now; build there or in CI".into());
    }
    let credentials: Vec<String> = if let Ok(profile) = std::env::var("FERRITRON_NOTARY_PROFILE") {
        vec!["--keychain-profile".into(), profile]
    } else {
        let var = |name: &str| std::env::var(name).map_err(|_| format!("set FERRITRON_NOTARY_PROFILE, or {name} (with the other FERRITRON_NOTARY_* keys)"));
        vec!["--key".into(), var("FERRITRON_NOTARY_KEY")?, "--key-id".into(), var("FERRITRON_NOTARY_KEY_ID")?, "--issuer".into(), var("FERRITRON_NOTARY_ISSUER")?]
    };
    // notarytool takes a zip of the bundle.
    let zip = bundle.with_extension("notarize.zip");
    let status = Command::new("ditto").args(["-c", "-k", "--keepParent"]).arg(bundle).arg(&zip).status().map_err(|e| format!("cannot run ditto: {e}"))?;
    if !status.success() {
        return Err("ditto failed to zip the bundle".into());
    }
    let status = Command::new("xcrun")
        .args(["notarytool", "submit"])
        .arg(&zip)
        .args(&credentials)
        .arg("--wait")
        .status()
        .map_err(|e| format!("cannot run xcrun notarytool: {e}"))?;
    let _ = std::fs::remove_file(&zip);
    if !status.success() {
        return Err("notarization failed (see notarytool's log above)".into());
    }
    let status = Command::new("xcrun").args(["stapler", "staple"]).arg(bundle).status().map_err(|e| format!("cannot run xcrun stapler: {e}"))?;
    if !status.success() {
        return Err("stapling the ticket failed".into());
    }
    println!("Notarized and stapled {}", bundle.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        let mac = Target::parse(Some("aarch64-apple-darwin")).unwrap();
        assert!(mac.is_macos() && mac.is_unix() && mac.arch == "aarch64");
        assert_eq!(mac.cargo_triples(), ["aarch64-apple-darwin"]);
        let universal = Target::parse(Some("universal-apple-darwin")).unwrap();
        assert_eq!(universal.cargo_triples(), UNIVERSAL_PARTS);
        assert_eq!(Target::parse(Some("x86_64-pc-windows-msvc")).unwrap().os, "windows");
        assert!(Target::parse(Some("universal-pc-windows-msvc")).is_err());
        assert!(Target::parse(Some("wasm32-unknown-unknown")).is_err());
        let android = Target::parse(Some("android")).unwrap();
        assert!(android.is_mobile() && android.cargo_triples() == ["aarch64-linux-android"] && !android.is_unix());
        let ios = Target::parse(Some("ios")).unwrap();
        assert!(ios.is_apple() && ios.is_mobile() && !ios.is_macos());
        assert!(Target::parse(Some("armv7-linux-android")).is_err());
        let web = Target::parse(Some("web")).unwrap();
        assert!(web.is_web() && !web.is_unix() && web.cargo_triples() == ["wasm32-wasip1"]);
        let here = Target::parse(None).unwrap();
        assert!(here.cargo_triples().is_empty() && !here.crosses_to_macos());
    }
}
