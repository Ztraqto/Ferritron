//! Android apps (`package = "apk"`): the staged folder (AndroidManifest.xml,
//! java/, assets/, lib/<abi>/) becomes a signed APK with the Android SDK's
//! own tools and a JDK, without Gradle:
//!
//! javac (against android.jar) -> d8 -> aapt2 link -> aapt add (classes.dex,
//! native libraries) -> zipalign -> apksigner.
//!
//! The Android SDK is found at ANDROID_HOME (or ANDROID_SDK_ROOT, or where
//! Android Studio puts it), the JDK at JAVA_HOME (or on PATH). APKs are
//! signed with the debug key (~/.android/debug.keystore, made if missing)
//! unless FERRITRON_ANDROID_KEYSTORE names a release key store
//! (FERRITRON_ANDROID_KEYSTORE_PASS, FERRITRON_ANDROID_KEY_ALIAS).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The oldest Android the apps run on (AAudio with low latency: 8.1; MIDI: 6.0).
pub const MIN_SDK: u32 = 28;

pub fn sdk_dir() -> Result<PathBuf, String> {
    let candidates = [std::env::var_os("ANDROID_HOME"), std::env::var_os("ANDROID_SDK_ROOT")]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .chain(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Android").join("Sdk")))
        .chain(std::env::var_os("HOME").map(|d| PathBuf::from(d).join(if cfg!(target_os = "macos") { "Library/Android/sdk" } else { "Android/Sdk" })));
    let found: Vec<PathBuf> = candidates.collect();
    found.iter().find(|d| d.join("build-tools").is_dir() && d.join("platforms").is_dir()).cloned().ok_or_else(|| {
        format!(
            "Android apps need the Android SDK (build-tools and a platform), which was not found.\n\
             Looked at: {}\nInstall it with Android Studio or the command-line tools and set ANDROID_HOME.\n\
             Android アプリには Android SDK（build-tools とプラットフォーム）が必要ですが、見つかりませんでした。ANDROID_HOME を設定してください。",
            found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; ")
        )
    })
}

/// The newest version folder in `dir` (versions compare by their numbers).
fn newest(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let key = |p: &PathBuf| -> Vec<u32> {
        p.file_name().unwrap_or_default().to_string_lossy().trim_start_matches(prefix).split(['.', '-']).filter_map(|n| n.parse().ok()).collect()
    };
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).max_by_key(key)
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    if cfg!(windows) {
        for ext in ["exe", "bat"] {
            let p = dir.join(format!("{name}.{ext}"));
            if p.is_file() {
                return p;
            }
        }
    }
    dir.join(name)
}

/// The major version of the JDK at `home` (from its `release` file).
fn jdk_version(home: &Path) -> Option<u32> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let line = release.lines().find(|l| l.starts_with("JAVA_VERSION="))?;
    let version = line.trim_start_matches("JAVA_VERSION=").trim_matches('"');
    let mut parts = version.split(['.', '-', '+']);
    match parts.next()?.parse().ok()? {
        1 => parts.next()?.parse().ok(),
        major => Some(major),
    }
}

/// The JDK for javac, d8 and apksigner. The SDK's d8 needs Java 17 to 21:
/// FERRITRON_ANDROID_JDK, JAVA_HOME, Android Studio's own JDK, or one on
/// PATH, the first in that range (else the first found).
fn jdk() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> =
        [std::env::var_os("FERRITRON_ANDROID_JDK"), std::env::var_os("JAVA_HOME")].into_iter().flatten().map(PathBuf::from).collect();
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(programs).join("Android").join("Android Studio").join("jbr"));
    }
    candidates.push(PathBuf::from("/Applications/Android Studio.app/Contents/jbr/Contents/Home"));
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if let Some(home) = dir.parent().filter(|_| exe(&dir, "javac").is_file()) {
            candidates.push(home.to_path_buf());
        }
    }
    let found: Vec<(PathBuf, Option<u32>)> =
        candidates.into_iter().filter(|c| exe(&c.join("bin"), "javac").is_file()).map(|c| (jdk_version(&c), c)).map(|(v, c)| (c, v)).collect();
    found.iter().find(|(_, v)| v.is_some_and(|v| (17..=21).contains(&v))).or(found.first()).map(|(c, _)| c.clone())
}

fn java_tool(name: &str) -> PathBuf {
    match jdk() {
        Some(home) => exe(&home.join("bin"), name),
        None => PathBuf::from(name),
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
    // d8 and apksigner start `java` from JAVA_HOME.
    if let Some(home) = jdk() {
        cmd.env("JAVA_HOME", &home);
    }
    let status = cmd.status().map_err(|e| format!("cannot run {what}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what} failed")) }
}

fn java_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            java_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "java" || e == "class") {
            out.push(p);
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() { collect(&p, out) } else { out.push(p) }
    }
}

/// The debug key store, made with keytool the first time.
fn debug_keystore() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).ok_or("no home folder for the debug key store")?;
    let dir = PathBuf::from(home).join(".android");
    let store = dir.join("debug.keystore");
    if !store.is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        run(
            Command::new(java_tool("keytool")).args(["-genkeypair", "-keystore"]).arg(&store).args([
                "-storepass", "android", "-alias", "androiddebugkey", "-keypass", "android", "-keyalg", "RSA", "-keysize", "2048", "-validity", "10000",
                "-dname", "CN=Android Debug,O=Android,C=US",
            ]),
            "keytool (making the debug key store)",
        )?;
    }
    Ok(store)
}

/// Makes `<name>.apk` next to the staged folder `stage`; returns its path.
pub fn package(stage: &Path, manifest: &ferritron_manifest::Manifest, name: &str) -> Result<PathBuf, String> {
    let sdk = sdk_dir()?;
    let tools = newest(&sdk.join("build-tools"), "").ok_or("the Android SDK has no build-tools")?;
    let platform = newest(&sdk.join("platforms"), "android-").ok_or("the Android SDK has no platform")?;
    let android_jar = platform.join("android.jar");
    let build = stage.with_extension("android-build");
    let _ = std::fs::remove_dir_all(&build);
    let classes = build.join("classes");
    std::fs::create_dir_all(&classes).map_err(|e| e.to_string())?;

    let mut sources = Vec::new();
    java_files(&stage.join("java"), &mut sources);
    run(
        Command::new(java_tool("javac")).args(["-source", "17", "-target", "17", "-nowarn", "-encoding", "UTF-8", "-classpath"]).arg(&android_jar).arg("-d").arg(&classes).args(&sources),
        "javac",
    )?;
    let mut class_files = Vec::new();
    java_files(&classes, &mut class_files);
    run(
        Command::new(exe(&tools, "d8")).args(["--release", "--min-api", &MIN_SDK.to_string(), "--lib"]).arg(&android_jar).arg("--output").arg(&build).args(&class_files),
        "d8",
    )?;

    let unsigned = build.join("unsigned.apk");
    let version_code = manifest.version_number().max(1).to_string();
    let mut link = Command::new(exe(&tools, "aapt2"));
    link.arg("link").arg("-o").arg(&unsigned).arg("-I").arg(&android_jar).arg("--manifest").arg(stage.join("AndroidManifest.xml"));
    link.args(["--min-sdk-version", &MIN_SDK.to_string(), "--target-sdk-version", &platform_level(&platform).to_string()]);
    link.args(["--version-code", &version_code, "--version-name", &manifest.version]);
    run(&mut link, "aapt2 link")?;

    // classes.dex and the native libraries, by their paths inside the APK.
    std::fs::copy(build.join("classes.dex"), stage.join("classes.dex")).map_err(|e| e.to_string())?;
    let mut add = Command::new(exe(&tools, "aapt"));
    add.current_dir(stage).arg("add").arg(&unsigned).arg("classes.dex");
    let mut libs = Vec::new();
    for abi in std::fs::read_dir(stage.join("lib")).into_iter().flatten().flatten() {
        for lib in std::fs::read_dir(abi.path()).into_iter().flatten().flatten() {
            libs.push(format!("lib/{}/{}", abi.file_name().to_string_lossy(), lib.file_name().to_string_lossy()));
        }
    }
    add.args(&libs);
    // Assets by name with `/` (aapt2's -A keeps Windows separators in names).
    let mut assets = Vec::new();
    let mut files = Vec::new();
    collect(&stage.join("assets"), &mut files);
    for file in files {
        if let Ok(rel) = file.strip_prefix(stage) {
            assets.push(rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"));
        }
    }
    add.args(&assets);
    run(&mut add, "aapt add")?;
    let _ = std::fs::remove_file(stage.join("classes.dex"));

    let aligned = build.join("aligned.apk");
    run(Command::new(exe(&tools, "zipalign")).args(["-f", "-p", "4"]).arg(&unsigned).arg(&aligned), "zipalign")?;

    let apk = stage.with_file_name(format!("{name}.apk"));
    let mut sign = Command::new(exe(&tools, "apksigner"));
    sign.arg("sign");
    match std::env::var_os("FERRITRON_ANDROID_KEYSTORE") {
        Some(store) => {
            let pass = std::env::var("FERRITRON_ANDROID_KEYSTORE_PASS").map_err(|_| "set FERRITRON_ANDROID_KEYSTORE_PASS for the release key store")?;
            sign.arg("--ks").arg(store).args(["--ks-pass", &format!("env:FERRITRON_ANDROID_KEYSTORE_PASS"), "--ks-key-alias"]);
            sign.arg(std::env::var("FERRITRON_ANDROID_KEY_ALIAS").unwrap_or_else(|_| "release".into()));
            let _ = pass;
        }
        None => {
            sign.arg("--ks").arg(debug_keystore()?).args(["--ks-pass", "pass:android", "--key-pass", "pass:android", "--ks-key-alias", "androiddebugkey"]);
        }
    }
    sign.arg("--out").arg(&apk).arg(&aligned);
    run(&mut sign, "apksigner")?;
    let _ = std::fs::remove_dir_all(&build);
    Ok(apk)
}

fn platform_level(platform: &Path) -> u32 {
    platform.file_name().and_then(|n| n.to_string_lossy().trim_start_matches("android-").parse().ok()).unwrap_or(34)
}

/// The NDK's clang for `triple` (as cargo's linker), and its llvm-ar.
pub fn ndk_tools(triple: &str) -> Result<(PathBuf, PathBuf), String> {
    let context = ferritron_build::sdk::Context::current()?;
    let found = context.resolve(&ferritron_build::sdk::NDK).map_err(|tried| {
        format!(
            "Android builds need the Android NDK, which was not found.\nLooked at: {}\n\
             Install it (Android Studio's SDK Manager, or https://developer.android.com/ndk/downloads) and set ANDROID_NDK_HOME.\n\
             Android 向けのビルドには Android NDK が必要ですが、見つかりませんでした。ANDROID_NDK_HOME を設定してください。",
            tried.join("; ")
        )
    })?;
    let host = match std::env::consts::OS {
        "windows" => "windows-x86_64",
        "macos" => "darwin-x86_64",
        _ => "linux-x86_64",
    };
    let bin = found.dir.join("toolchains/llvm/prebuilt").join(host).join("bin");
    let clang_target = triple.replace("armv7-linux-androideabi", "armv7a-linux-androideabi");
    let clang = bin.join(format!("{clang_target}{MIN_SDK}-clang{}", if cfg!(windows) { ".cmd" } else { "" }));
    let ar = bin.join(format!("llvm-ar{}", std::env::consts::EXE_SUFFIX));
    if !clang.is_file() {
        return Err(format!("the NDK at {} has no {}", found.dir.display(), clang.display()));
    }
    Ok((clang, ar))
}
