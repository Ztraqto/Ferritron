//! `cargo ferritron`: builds Ferritron plugins and packages them for hosts.

mod exports;
mod formats;
mod i18n;
mod new;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Must match `ferritron_core::reload::MARKER_SUFFIX`.
const HOT_RELOAD_MARKER_SUFFIX: &str = ".ferritron-hot-reload";

const USAGE: &str = "\
Usage:
  cargo ferritron new <name> [--template instrument]   Create a plugin project
  cargo ferritron bundle <package> [options]    Build and package a plugin
  cargo ferritron i18n check <package>          List untranslated strings
  cargo ferritron --version                     Print the Ferritron version

`bundle` packages <package> for every plugin format it was built with
(e.g. VST3 bundle, CLAP file) in target/bundled/. Formats are declared by
their crates (`[package.metadata.ferritron.format]`), not by this tool.

Options:
  --dev       Debug build with hot reload: after installing once,
              `cargo build -p <package>` updates the plugin inside the running host
  --install   Also copy the results to each format's system folder
              (may need administrator rights)

macOS: bundles get an Info.plist, the `ferritron-host` helper for crash
isolation (compiled with `cc`) and a code signature: ad hoc, or with the
identity in FERRITRON_CODESIGN_IDENTITY (e.g. \"Developer ID Application: ...\").";

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
    let (mut dev, mut install) = (false, false);
    for arg in &args[1..] {
        match arg.as_str() {
            "--dev" => dev = true,
            "--install" => install = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`\n\n{USAGE}")),
            name => package = Some(name.to_string()),
        }
    }
    let package = package.ok_or_else(|| format!("missing <package>\n\n{USAGE}"))?;

    let cargo = cargo_program();
    let artifact = build(&package, !dev)?;
    let metadata = cargo_metadata(&cargo, false).ok_or("`cargo metadata` failed")?;
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
    let values = manifest.template_values();
    let file_name: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    // target/<profile>/<lib> -> target/bundled/
    let out_dir = artifact.library.parent().and_then(Path::parent).ok_or("unexpected build output location")?.join("bundled");

    let companions = companions(&artifact)?;
    for format in &formats {
        let (root, binary) = format.package(&artifact.library, &out_dir, &file_name, &values)?;
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
        if root.is_dir() && cfg!(unix) {
            // Isolated plugins run in a child process started by this helper.
            let helper = host_helper(&out_dir)?;
            let helpers = root.join("Contents/Helpers");
            std::fs::create_dir_all(&helpers).map_err(|e| format!("cannot create {}: {e}", helpers.display()))?;
            std::fs::copy(&helper, helpers.join("ferritron-host")).map_err(|e| format!("cannot copy the ferritron-host helper: {e}"))?;
        }
        if cfg!(target_os = "macos") {
            sign(&root)?;
        }
        println!("Bundled {} ({})", root.display(), format.name);
        if install {
            let installed = format.install(&root, &[HOT_RELOAD_MARKER_SUFFIX])?;
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
    if dev {
        println!("Hot reload: rebuild with `cargo build -p {package}` and the running plugin updates itself.");
    }
    Ok(())
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

/// Runs `cargo build` and returns the plugin library it produced.
fn build(package: &str, release: bool) -> Result<Artifact, String> {
    let cargo = cargo_program();
    let mut cmd = Command::new(&cargo);
    cmd.args(["build", "--package", package, "--message-format=json-render-diagnostics"]);
    if let Some(dir) = package_dir(&cargo, package) {
        cmd.env("FERRITRON_ASSETS_STAMP", assets_stamp(&dir));
    }
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
            .find(|f| [".dll", ".so", ".dylib"].iter().any(|ext| f.ends_with(ext)));
        let manifest = msg["manifest_path"].as_str();
        if let (Some(library), Some(manifest)) = (library, manifest) {
            let manifest_dir = Path::new(manifest).parent().unwrap().to_path_buf();
            return Ok(Artifact { library: library.into(), manifest_dir, out_dir: out_dir(&msg["package_id"]) });
        }
    }
    Err(format!("`{package}` did not produce a plugin library; add `crate-type = [\"cdylib\"]` under [lib] in its Cargo.toml"))
}

fn cargo_metadata(cargo: &str, no_deps: bool) -> Option<serde_json::Value> {
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1"]);
    if no_deps {
        cmd.arg("--no-deps");
    }
    serde_json::from_slice(&cmd.stderr(Stdio::inherit()).output().ok()?.stdout).ok()
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

/// The `ferritron-host` helper (macOS, Linux), compiled once into `out_dir`
/// with the system C compiler (`CC` or `cc`).
fn host_helper(out_dir: &Path) -> Result<PathBuf, String> {
    const SOURCE: &str = include_str!("host_helper.c");
    let dir = out_dir.join(".ferritron-host");
    let (source, binary) = (dir.join("host_helper.c"), dir.join("ferritron-host"));
    if binary.is_file() && std::fs::read_to_string(&source).is_ok_and(|s| s == SOURCE) {
        return Ok(binary);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::write(&source, SOURCE).map_err(|e| format!("cannot write {}: {e}", source.display()))?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut cmd = Command::new(&cc);
    cmd.args(["-O2", "-o"]).arg(&binary).arg(&source);
    if cfg!(target_os = "linux") {
        cmd.arg("-ldl");
    }
    let status = cmd.status().map_err(|e| format!("cannot run {cc} to build the ferritron-host helper: {e}"))?;
    if !status.success() {
        return Err(format!("building the ferritron-host helper with {cc} failed"));
    }
    Ok(binary)
}

/// Signs a macOS bundle: hosts on Apple Silicon only load signed code. Ad
/// hoc unless FERRITRON_CODESIGN_IDENTITY names a certificate.
fn sign(root: &Path) -> Result<(), String> {
    let identity = std::env::var("FERRITRON_CODESIGN_IDENTITY").unwrap_or_else(|_| "-".into());
    let helper = root.join("Contents/Helpers/ferritron-host");
    // Nested code first, then the bundle.
    for target in [helper.as_path(), root].into_iter().filter(|p| p.exists()) {
        let mut cmd = Command::new("codesign");
        cmd.args(["--force", "--sign", &identity]);
        if identity != "-" {
            cmd.args(["--timestamp", "--options", "runtime"]);
        }
        let status = cmd.arg(target).status().map_err(|e| format!("cannot run codesign: {e}"))?;
        if !status.success() {
            return Err(format!("codesign failed for {}", target.display()));
        }
    }
    Ok(())
}
