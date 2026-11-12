//! `cargo ferritron`: builds Ferritron plugins and packages them for hosts.

mod android;
mod check;
mod compat;
mod exports;
mod formats;
mod launcher;
mod i18n;
mod lang;
mod new;
mod pgo;
mod sdk;
mod shortcut;
mod sizes;
mod target;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Must match `ferritron_core::reload::MARKER_SUFFIX`.
const HOT_RELOAD_MARKER_SUFFIX: &str = ".ferritron-hot-reload";

const USAGE: &str = "\
Usage:
  cargo ferritron new <name> [--template instrument]   Create a plugin project
  cargo ferritron bundle <package> [options]    Build and package a plugin
  cargo ferritron i18n check <package>          List untranslated strings
  cargo ferritron check <package>               Look a plugin over: reported vs. measured latency, a leftover .frtx, a 1.x plugin
  cargo ferritron migrate-frtx <package>        Rename the old <name>.frtx project file to <name>.ferritroject
  cargo ferritron notarize <bundle>             Notarize a signed macOS bundle (on a Mac)
  cargo ferritron sdk ...                       SDKs: status, list, available, fetch, use, source, remove
  cargo ferritron --version                     Print the Ferritron version

Options of `bundle`: --dev, --install, --no-install, --shortcuts, --no-shortcuts, --target <triple>, --size,
--pgo <scenario> and --features <list>
(passed to `cargo build`: a feature of the plugin that turns on a format that is off
by default, such as VST 2 or AAX).

`bundle` packages <package> for every plugin format it was built with
(e.g. VST3 bundle, CLAP file) in target/bundled/. Formats are declared by
their crates (`[package.metadata.ferritron.format]`), not by this tool.

Options:
  --dev       Debug build with hot reload: after installing once,
              `cargo build -p <package>` updates the plugin inside the running host
  --install   Also copy the results to each format's system folder
              (may need administrator rights)
  --no-install    Do not install this time, even with `[bundle] install = true`
  --shortcuts     Put a shortcut to each format's system folder next to the bundles
                  (`_VST3 Plugins`, ...; `[bundle] shortcuts = true` does it every time)
  --no-shortcuts  Do not make them this time, even with `[bundle] shortcuts = true`
  --size      At the end, print the size of the code (.text) of each bundled binary
  --pgo S     Profile-guided build: an instrumented build is played by the scenario S
              (`testhost`, or `cmd:<command with {bundle}>`), the profiles are merged
              with llvm-profdata (`rustup component add llvm-tools`, never installed by
              this tool) and the real build uses them. Release builds for this computer only.
  --target T  Build for another target: web (a Web Audio Module, WAM 2.0;
              needs `rustup target add wasm32-wasip1`), aarch64-apple-darwin,
              x86_64-apple-darwin, universal-apple-darwin (both in one).
              From Windows or Linux, macOS builds go through xclang
              (`cargo ferritron sdk fetch xclang`, then `... fetch macos`).
              Output: target/<T>/bundled/

SDK versions pinned in the project file (`[sdk] vst3 = \"3.8.1\"`) are used
from Ferritron's SDK folder; see `cargo ferritron sdk status`.

macOS: bundles get an Info.plist, the `ferritron-host` helper for crash
isolation and a code signature: ad hoc, or with the identity in
FERRITRON_CODESIGN_IDENTITY (e.g. \"Developer ID Application: ...\"). They
need macOS 13.0 or later (MACOSX_DEPLOYMENT_TARGET changes it).";

fn main() -> ExitCode {
    // Invoked as `cargo-ferritron ferritron ...` by cargo, or directly via the alias.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("ferritron") {
        args.remove(0);
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The cargo that runs this tool (`CARGO`), else the one on `PATH`.
fn cargo_program() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("bundle") => {}
        Some("new") => {
            let name = args.get(1).ok_or_else(|| format!("missing <name>

{USAGE}"))?;
            let template = match args.iter().position(|a| a == "--template").and_then(|i| args.get(i + 1)).map(String::as_str) {
                None | Some("effect") => ferritron_manifest::Template::Effect,
                Some("instrument") => ferritron_manifest::Template::Instrument,
                Some(other) => return Err(format!("unknown template `{other}` (effect, instrument)")),
            };
            return new::run(name, template);
        }
        Some("i18n") => {
            let (Some("check"), Some(package)) = (args.get(1).map(String::as_str), args.get(2)) else {
                return Err(format!("usage: cargo ferritron i18n check <package>

{USAGE}"));
            };
            let cargo = cargo_program();
            let dir = package_dir(&cargo, package).ok_or_else(|| format!("package `{package}` not found"))?;
            return i18n::check(&dir);
        }
        Some("check") => {
            let package = args.get(1).ok_or_else(|| format!("missing <package>\n\n{USAGE}"))?;
            return check::run(package);
        }
        Some("migrate-frtx") => {
            let package = args.get(1).ok_or_else(|| format!("missing <package>\n\n{USAGE}"))?;
            let dir = package_dir(&cargo_program(), package).ok_or_else(|| format!("package `{package}` not found"))?;
            let (old, new) = ferritron_manifest::migrate_frtx(&dir)?;
            println!("{} {} -> {}", lang::tr("名前を変更しました:", "Renamed:"), old.display(), new.display());
            return Ok(());
        }
        Some("sdk") => return sdk::run(&args[1..]),
        Some("notarize") => {
            let bundle = args.get(1).ok_or_else(|| format!("missing <bundle>\n\n{USAGE}"))?;
            return target::notarize(Path::new(bundle));
        }
        Some("-V" | "--version") => {
            println!("cargo-ferritron {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
    let mut package = None;
    let mut o = BundleOptions::default();
    let mut features: Option<String> = None;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dev" => o.dev = true,
            "--install" => o.install = true,
            "--no-install" => o.no_install = true,
            "--shortcuts" => o.shortcuts = Some(true),
            "--no-shortcuts" => o.shortcuts = Some(false),
            "--size" => o.size = true,
            "--pgo" => o.pgo = Some(rest.next().ok_or("--pgo needs a scenario (testhost or cmd:<command>)")?.clone()),
            "--features" => features = Some(rest.next().ok_or("--features needs a list")?.clone()),
            "--target" => o.triple = Some(rest.next().ok_or("--target needs a target triple")?.clone()),
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`\n\n{USAGE}")),
            name => package = Some(name.to_string()),
        }
    }
    o.package = package.ok_or_else(|| format!("missing <package>\n\n{USAGE}"))?;
    if let Some(list) = &features {
        // Read by `build_one`; set before anything runs in parallel.
        unsafe { std::env::set_var("FERRITRON_BUNDLE_FEATURES", list) };
    }
    bundle_command(&o)
}

#[derive(Default)]
struct BundleOptions {
    package: String,
    dev: bool,
    install: bool,
    /// `--no-install`: not even when the project file's `[bundle] install` is true.
    no_install: bool,
    /// `--shortcuts` / `--no-shortcuts`; `None` follows `[bundle] shortcuts`.
    shortcuts: Option<bool>,
    triple: Option<String>,
    /// `--size`: print the code size of each binary at the end.
    size: bool,
    /// `--pgo <scenario>`.
    pgo: Option<String>,
}

/// What a bundling run made: the bundle roots and the plugin binaries inside them.
#[derive(Default)]
struct Bundled {
    roots: Vec<PathBuf>,
    binaries: Vec<PathBuf>,
}

fn bundle_command(o: &BundleOptions) -> Result<(), String> {
    let target = target::Target::parse(o.triple.as_deref())?;
    if o.install && o.no_install {
        return Err("--install and --no-install together".into());
    }
    if target.triple.is_some() && (o.dev || o.install) {
        return Err("--dev and --install build for this computer; leave out --target".into());
    }
    if o.pgo.is_some() && (target.triple.is_some() || o.dev || o.install) {
        return Err(lang::tr(
            "--pgo はこのコンピューター向けのリリースビルドだけに使えます（--dev・--install・--target と一緒には使えません）",
            "--pgo is for release builds for this computer (not with --dev, --install or --target)",
        )
        .into());
    }
    let cargo = cargo_program();
    let metadata = cargo_metadata(&cargo, false).ok_or("`cargo metadata` failed")?;
    // T1: a 2.x tool does not handle 1.x plugins as they are.
    if let Some(version) = compat::plugin_framework_version(&metadata)
        && let Some(note) = compat::legacy_plugin_notice(env!("CARGO_PKG_VERSION"), &version)
    {
        return Err(note);
    }
    let bundled = match &o.pgo {
        None => bundle_once(o, &target)?,
        Some(scenario) => {
            let target_directory = PathBuf::from(metadata["target_directory"].as_str().ok_or("`cargo metadata` has no target_directory")?);
            let last = std::cell::RefCell::new(Bundled::default());
            pgo::run(scenario, &target_directory, &mut || {
                let made = bundle_once(o, &target)?;
                let roots = made.roots.clone();
                *last.borrow_mut() = made;
                Ok(roots)
            })?;
            last.into_inner()
        }
    };
    if o.size {
        println!("{}", lang::tr("コードのサイズ (.text):", "Code size (.text):"));
        for binary in &bundled.binaries {
            let shown = match sizes::text_size(binary)? {
                Some(bytes) => sizes::show(bytes),
                None => lang::tr("不明な形式", "unknown format").to_string(),
            };
            println!("  {}: {shown}", binary.display());
        }
    }
    Ok(())
}

/// Builds and packages once, with the environment as it is.
fn bundle_once(o: &BundleOptions, target: &target::Target) -> Result<Bundled, String> {
    let (package, dev) = (o.package.clone(), o.dev);
    let mut bundled = Bundled::default();
    formats::set_target(&target.os, &target.arch);

    let cargo = cargo_program();
    let artifact = build(&package, !dev, target)?;
    let metadata = format_metadata(&cargo).ok_or("`cargo metadata` failed")?;
    warn_on_version_mismatch(&metadata);
    let exported = exports::exported_names(&artifact.library)?;
    let formats: Vec<formats::Format> = formats::discover(&metadata)?
        .into_iter()
        .filter(|f| exported.contains(&f.entry_symbol))
        .collect();
    if formats.is_empty() {
        return Err(format!("`{package}` exports no plugin format; is `#[ferritron::plugin]` applied and a format enabled?"));
    }

    let manifest = project(&artifact.manifest_dir, &metadata)?;
    let name = manifest.name.clone();
    let scope = manifest.template_scope();
    let ui_dir = web_ui_dir(&artifact.manifest_dir);
    let file_name: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    // target/<profile>/<lib> -> target/bundled/
    let out_dir = artifact.library.parent().and_then(Path::parent).ok_or("unexpected build output location")?.join("bundled");

    let settings = bundle_settings(&artifact.manifest_dir)?;
    // `[bundle] install`: for this computer only, and not for the profiling pass of --pgo.
    let auto_install = settings.install && !o.install && !o.no_install;
    if auto_install && (target.triple.is_some() || o.pgo.is_some()) {
        println!("{}", lang::tr("[bundle] install: --target・--pgo のビルドはインストールしません", "[bundle] install: builds with --target or --pgo are not installed"));
    }
    let install = o.install || (auto_install && target.triple.is_none() && o.pgo.is_none());
    let companions = companions(&artifact)?;
    let mut made: Vec<PathBuf> = Vec::new();
    for format in &formats {
        let keep_root = made.contains(&format.root(&out_dir, &file_name)?);
        let inputs = formats::Inputs { name: &file_name, scope: &scope, ui: ui_dir.as_deref(), keep_root, exports: &exported };
        let (root, binary) = format.package(&artifact.library, &out_dir, &inputs)?;
        made.push(root.clone());
        bundled.binaries.push(binary.clone());
        if let Some(path) = format.launcher(&out_dir, &file_name)? {
            launcher::build(target, &path, &binary, format.launcher_kind.as_deref() == Some("appex"))?;
        }
        // Companion libraries (C# and other language hosts) go next to the binary.
        let beside = binary.parent().ok_or("unexpected bundle layout")?;
        for companion in &companions {
            let target = beside.join(companion.file_name().ok_or("companion without a file name")?);
            std::fs::copy(companion, &target).map_err(|e| format!("cannot copy {}: {e}", companion.display()))?;
        }
        if dev {
            write_marker(&binary, &artifact.library)?;
        } else {
            // A marker left by an earlier dev build would make this release package hot reload.
            let _ = std::fs::remove_file(formats::with_suffix(&binary, HOT_RELOAD_MARKER_SUFFIX));
        }
        if format.package.as_deref() == Some("apk") {
            let apk = android::package(&root, &manifest, &file_name)?;
            println!("Bundled {} ({})", apk.display(), format.name);
            continue;
        }
        if root.is_dir() && target.is_unix() && !target.is_mobile() {
            // Isolated plugins run in a child process started by this helper.
            let helper = host_helper(&out_dir, target)?;
            let helpers = root.join("Contents/Helpers");
            std::fs::create_dir_all(&helpers).map_err(|e| format!("cannot create {}: {e}", helpers.display()))?;
            std::fs::copy(&helper, helpers.join("ferritron-host")).map_err(|e| format!("cannot copy the ferritron-host helper: {e}"))?;
        }
        // Apps are signed once, after every format inside them is in place.
        if target.is_apple() && root.is_dir() && !formats.iter().skip_while(|f| f.name != format.name).skip(1).any(|f| f.root(&out_dir, &file_name).ok().as_ref() == Some(&root)) {
            target::sign_macos(&root)?;
        }
        println!("Bundled {} ({})", root.display(), format.name);
        if install && format.install_folder().is_none() {
            println!("{}", lang::tr(&format!("{} はインストール先がないのでインストールしません", format.name), &format!("{} has no install folder: not installed", format.name)));
        } else if install {
            let installed = match format.install(&root, &[HOT_RELOAD_MARKER_SUFFIX]) {
                Ok(installed) => installed,
                // Installing because of the project file: a folder that needs administrator
                // rights does not fail the bundle.
                Err(e) if !o.install => {
                    eprintln!("warning: {e}");
                    continue;
                }
                Err(e) => return Err(e),
            };
            // A single-file format (CLAP): its companions go into the same folder.
            if installed.is_file() {
                for companion in &companions {
                    let to = installed.with_file_name(companion.file_name().unwrap_or_default());
                    std::fs::copy(companion, &to).map_err(|e| format!("cannot copy {}: {e}", companion.display()))?;
                }
            }
            println!("Installed {}", installed.display());
        }
    }
    let shortcuts = o.shortcuts.unwrap_or(settings.shortcuts);
    if shortcuts && target.triple.is_some() {
        println!("{}", lang::tr("ショートカット: --target のビルドには作りません（このコンピューターのフォルダを指すため）", "Shortcuts: not made for --target builds (they point to this computer's folders)"));
    } else if shortcuts {
        let mut links = Vec::new();
        for format in &formats {
            let Some(folder) = format.install_folder() else { continue };
            let folder = folder?;
            let link = out_dir.join(format!("{}{} Plugins", settings.shortcut_prefix, format.name));
            if !links.iter().any(|(_, f): &(PathBuf, PathBuf)| *f == folder) {
                links.push((link, folder));
            }
        }
        for (link, folder) in shortcut::make(&links)? {
            println!("Shortcut {} -> {}", link.display(), folder.display());
        }
    }
    if dev {
        println!("Hot reload: rebuild with `cargo build -p {package}` and the running plugin updates itself.");
    }
    bundled.roots = made;
    Ok(bundled)
}

/// The `[bundle]` section of the package's project file.
fn bundle_settings(manifest_dir: &Path) -> Result<ferritron_manifest::BundleSettings, String> {
    let path = ferritron_manifest::locate(manifest_dir)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    ferritron_manifest::bundle_settings(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Companion libraries from `[companion] files` in the project file, relative
/// to the package's build-script output (see `ferritron_core::companion`).
fn companions(artifact: &Artifact) -> Result<Vec<PathBuf>, String> {
    let path = ferritron_manifest::locate(&artifact.manifest_dir)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let manifest: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(files) = manifest.get("companion").and_then(|c| c.get("files")).and_then(|f| f.as_array()) else { return Ok(Vec::new()) };
    let out_dir = artifact.out_dir.as_ref().ok_or("[companion] files need a build script that creates them in OUT_DIR")?;
    files
        .iter()
        .map(|f| {
            let file = out_dir.join(f.as_str().ok_or("[companion] files must be strings")?);
            if file.is_file() { Ok(file) } else { Err(format!("companion {} was not built", file.display())) }
        })
        .collect()
}

/// Tells the binary to hot reload from `library` (see `ferritron_core::reload`).
fn write_marker(binary: &Path, library: &Path) -> Result<(), String> {
    let library = std::fs::canonicalize(library).map_err(|e| format!("cannot resolve library path: {e}"))?;
    // Strip the `\\?\` prefix that canonicalize adds on Windows.
    let library = library.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    std::fs::write(formats::with_suffix(binary, HOT_RELOAD_MARKER_SUFFIX), library)
        .map_err(|e| format!("cannot write hot reload marker: {e}"))
}

struct Artifact {
    library: PathBuf,
    manifest_dir: PathBuf,
    /// OUT_DIR of the package's build script, if it has one.
    out_dir: Option<PathBuf>,
}

/// Builds `package` for `target` (both architectures and joined, for
/// universal macOS) and returns the plugin library.
fn build(package: &str, release: bool, target: &target::Target) -> Result<Artifact, String> {
    let triples = target.cargo_triples();
    if triples.len() < 2 {
        return build_one(package, release, target, triples.first().map(String::as_str));
    }
    let parts: Vec<Artifact> = triples.iter().map(|t| build_one(package, release, target, Some(t))).collect::<Result<_, _>>()?;
    // target/<arch triple>/<profile>/<lib> -> target/universal-apple-darwin/<profile>/<lib>
    let first = &parts[0].library;
    let profile = first.parent().and_then(Path::file_name).ok_or("unexpected build output location")?;
    let target_dir = first.parent().and_then(Path::parent).and_then(Path::parent).ok_or("unexpected build output location")?;
    let library = target_dir.join(target.triple.as_deref().unwrap_or_default()).join(profile).join(first.file_name().unwrap_or_default());
    target::lipo(&parts.iter().map(|a| a.library.clone()).collect::<Vec<_>>(), &library)?;
    let Artifact { manifest_dir, out_dir, .. } = parts.into_iter().next().expect("two parts");
    Ok(Artifact { library, manifest_dir, out_dir })
}

/// The environment of a build: SDK versions pinned in the project file
/// (`[sdk]`), and a stamp of `sdk.toml` so a new or changed one rebuilds.
fn sdk_environment(cmd: &mut Command, package_dir: Option<&Path>) -> Result<(), String> {
    if let Some(dir) = package_dir
        && let Ok(path) = ferritron_manifest::locate(dir)
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let manifest = ferritron_manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for (id, version) in &manifest.sdk {
            if let Some(spec) = ferritron_build::sdk::find(id) {
                cmd.env(ferritron_build::sdk::pin_var(spec), version);
            }
        }
    }
    let stamp = ferritron_build::sdk::settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .map(|bytes| format!("{:x}", bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))))
        .unwrap_or_default();
    cmd.env("FERRITRON_SDK_STAMP", stamp);
    Ok(())
}

/// Runs `cargo build` (through xclang across to macOS) and returns the plugin library it produced.
fn build_one(package: &str, release: bool, target: &target::Target, triple: Option<&str>) -> Result<Artifact, String> {
    let cargo = cargo_program();
    let mut cmd = if target.crosses_to_macos() {
        let mut cmd = Command::new(ferritron_sdk::xclang_program(&target::xclang()?));
        cmd.arg("cargo");
        cmd
    } else {
        Command::new(&cargo)
    };
    cmd.args(["build", "--package", package, "--message-format=json-render-diagnostics"]);
    if let Some(triple) = triple {
        cmd.args(["--target", triple]);
    }
    if let Ok(list) = std::env::var("FERRITRON_BUNDLE_FEATURES") {
        cmd.args(["--features", &list]);
    }
    if let Some(minimum) = target.deployment_target() {
        cmd.env("MACOSX_DEPLOYMENT_TARGET", minimum);
    }
    if target.is_web() {
        // The browser side calls the plugin's functions through its table.
        cmd.env("CARGO_TARGET_WASM32_WASIP1_RUSTFLAGS", "-Clink-arg=--export-table");
    }
    if let (true, Some(triple)) = (target.os == "android", triple) {
        // The NDK's clang links (and compiles any C), for the oldest Android supported.
        let (clang, ar) = android::ndk_tools(triple)?;
        let var = triple.replace('-', "_");
        cmd.env(format!("CARGO_TARGET_{}_LINKER", var.to_ascii_uppercase()), &clang);
        cmd.env(format!("CC_{var}"), &clang).env(format!("AR_{var}"), &ar);
    }
    if target.os == "ios" {
        cmd.env("IPHONEOS_DEPLOYMENT_TARGET", target::IOS_MINIMUM);
    }
    let dir = package_dir(&cargo, package);
    if let Some(dir) = &dir {
        cmd.env("FERRITRON_ASSETS_STAMP", assets_stamp(dir));
    }
    sdk_environment(&mut cmd, dir.as_deref())?;
    if release {
        cmd.arg("--release");
    }
    let output = cmd.stderr(Stdio::inherit()).output().map_err(|e| format!("failed to run cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!("building `{package}` failed"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let messages: Vec<serde_json::Value> = stdout.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    // The build script output of the package whose library this is.
    let out_dir = |package_id: &serde_json::Value| {
        messages
            .iter()
            .find(|msg| msg["reason"] == "build-script-executed" && msg["package_id"] == *package_id)
            .and_then(|msg| msg["out_dir"].as_str().map(PathBuf::from))
    };
    for msg in &messages {
        if msg["reason"] != "compiler-artifact"
            || msg["target"]["name"].as_str().map(|n| n.replace('-', "_")) != Some(package.replace('-', "_"))
        {
            continue;
        }
        let is_cdylib = msg["target"]["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "cdylib"));
        if !is_cdylib {
            continue;
        }
        let library = msg["filenames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str())
            .find(|f| [".dll", ".so", ".dylib", ".wasm"].iter().any(|ext| f.ends_with(ext)));
        let manifest = msg["manifest_path"].as_str();
        if let (Some(library), Some(manifest)) = (library, manifest) {
            let manifest_dir = Path::new(manifest).parent().unwrap().to_path_buf();
            return Ok(Artifact { library: library.into(), manifest_dir, out_dir: out_dir(&msg["package_id"]) });
        }
    }
    Err(format!("`{package}` did not produce a plugin library; add `crate-type = [\"cdylib\"]` under [lib] in its Cargo.toml"))
}

/// An old `cargo-ferritron` left on `PATH` may not know what the plugin's
/// Ferritron does: say so instead of failing in odd ways.
fn warn_on_version_mismatch(metadata: &serde_json::Value) {
    let ours = env!("CARGO_PKG_VERSION");
    if let Some(old) = metadata["packages"].as_array().and_then(|p| p.iter().find(|p| p["name"] == "ferritron")).and_then(|p| p["version"].as_str()).filter(|v| is_deprecated(v)) {
        eprintln!(
            "warning: this plugin uses Ferritron {old}, which is deprecated (1.4.x and older get no fixes). Move it to 2.0.0: see docs/migration-2.0.md and \"Moving up from 1.4.x\" in CHANGELOG.md"
        );
    }
    let framework = metadata["packages"].as_array().and_then(|p| p.iter().find(|p| p["name"] == "ferritron")).and_then(|p| p["version"].as_str());
    if let Some(framework) = framework
        && framework != ours
    {
        eprintln!(
            "warning: this is cargo-ferritron {ours}, the plugin uses Ferritron {framework};              install the matching tool (`cargo install --path crates/ferritron-cli` in the Ferritron folder)"
        );
    }
}

/// Ferritron 1.4.x and older are deprecated (2.0.0 is current).
fn is_deprecated(version: &str) -> bool {
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) < (2, 0)
}

fn cargo_metadata(cargo: &str, no_deps: bool) -> Option<serde_json::Value> {
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1"]);
    if no_deps {
        cmd.arg("--no-deps");
    }
    serde_json::from_slice(&cmd.stderr(Stdio::inherit()).output().ok()?.stdout).ok()
}

/// `cargo metadata` for finding the plugin formats. Formats that are off by default (VST 2,
/// AAX) are turned on with `--features`; `cargo metadata` without the same features leaves their
/// crates out of the dependency graph of a plugin outside the Ferritron workspace, so they were
/// built but never packaged. The list is passed here as it is passed to `cargo build`; if cargo
/// refuses it, the plain metadata is used (as before).
fn format_metadata(cargo: &str) -> Option<serde_json::Value> {
    let Ok(list) = std::env::var("FERRITRON_BUNDLE_FEATURES") else {
        return cargo_metadata(cargo, false);
    };
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1", "--features", &list]);
    let out = cmd.stderr(Stdio::inherit()).output().ok()?;
    match serde_json::from_slice(&out.stdout) {
        Ok(metadata) if out.status.success() => Some(metadata),
        _ => cargo_metadata(cargo, false),
    }
}

/// Finds the directory of `package` in the current workspace.
fn package_dir(cargo: &str, package: &str) -> Option<PathBuf> {
    let metadata = cargo_metadata(cargo, true)?;
    let manifest = metadata["packages"].as_array()?.iter().find(|p| p["name"] == package)?["manifest_path"].as_str()?;
    Some(Path::new(manifest).parent()?.to_path_buf())
}

/// The list of embedded files (translations, UI), so adding one triggers a rebuild.
fn assets_stamp(package_dir: &Path) -> String {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let rel = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if entry.path().is_dir() {
                walk(&entry.path(), &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    let ui = ferritron_manifest::locate(package_dir)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("view")?.get("entry")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "ui".into());
    let mut files = Vec::new();
    walk(&package_dir.join("locales"), "locales", &mut files);
    walk(&package_dir.join(&ui), &ui, &mut files);
    files.sort();
    files.join(";")
}

/// The plugin's web editor folder (`[view] backend = "web"`, `entry`), if it has one.
fn web_ui_dir(package_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(ferritron_manifest::locate(package_dir).ok()?).ok()?;
    let view = text.parse::<toml::Table>().ok()?.get("view")?.as_table()?.clone();
    if view.get("backend").and_then(|b| b.as_str()) != Some("web") {
        return None;
    }
    let dir = package_dir.join(view.get("entry").and_then(|e| e.as_str()).unwrap_or("ui"));
    dir.join("index.html").is_file().then_some(dir)
}

/// The project file, with the package's version when it names none.
fn project(manifest_dir: &Path, metadata: &serde_json::Value) -> Result<ferritron_manifest::Manifest, String> {
    let path = ferritron_manifest::locate(manifest_dir)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut manifest = ferritron_manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if manifest.version.is_empty() {
        let package = metadata["packages"].as_array().into_iter().flatten().find(|p| {
            p["manifest_path"].as_str().and_then(|m| Path::new(m).parent()).is_some_and(|d| same_dir(d, manifest_dir))
        });
        manifest.version = package.and_then(|p| p["version"].as_str()).unwrap_or("0.1.0").to_string();
    }
    Ok(manifest)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The `ferritron-host` helper (macOS, Linux), compiled once into `out_dir`:
/// with the system C compiler (`CC` or `cc`), or xclang's clang across to
/// macOS (each architecture, joined for universal).
fn host_helper(out_dir: &Path, target: &target::Target) -> Result<PathBuf, String> {
    let triples = target.cargo_triples();
    if triples.len() > 1 {
        let parts: Vec<PathBuf> = triples.iter().map(|t| host_helper_for(&out_dir.join(t), target, Some(t))).collect::<Result<_, _>>()?;
        let joined = out_dir.join(".ferritron-host").join("ferritron-host");
        target::lipo(&parts, &joined)?;
        return Ok(joined);
    }
    host_helper_for(out_dir, target, triples.first().map(String::as_str))
}

fn host_helper_for(out_dir: &Path, target: &target::Target, triple: Option<&str>) -> Result<PathBuf, String> {
    const SOURCE: &str = include_str!("host_helper.c");
    let dir = out_dir.join(".ferritron-host");
    let (source, binary) = (dir.join("host_helper.c"), dir.join("ferritron-host"));
    if binary.is_file() && std::fs::read_to_string(&source).is_ok_and(|s| s == SOURCE) {
        return Ok(binary);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::write(&source, SOURCE).map_err(|e| format!("cannot write {}: {e}", source.display()))?;
    let (cc, flags) = target::c_compiler(target, triple)?;
    let cc_name = cc.display().to_string();
    let mut cmd = Command::new(&cc);
    cmd.args(&flags).args(["-O2", "-o"]).arg(&binary).arg(&source);
    if target.os == "linux" {
        cmd.arg("-ldl");
    }
    let status = cmd.status().map_err(|e| format!("cannot run {cc_name} to build the ferritron-host helper: {e}"))?;
    if !status.success() {
        return Err(format!("building the ferritron-host helper with {cc_name} failed"));
    }
    Ok(binary)
}

