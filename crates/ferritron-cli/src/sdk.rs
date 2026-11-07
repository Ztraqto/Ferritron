//! `cargo ferritron sdk`: where the SDKs are found, the versions Ferritron
//! knows, and fetching one after the developer accepts its license.

use ferritron_build::sdk::{self, Context, Sdk, Source};
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

pub const USAGE: &str = "\
  cargo ferritron sdk status                    Where each SDK is found (and how)
  cargo ferritron sdk list                      SDK versions in Ferritron's SDK folder
  cargo ferritron sdk available [<id>]          Versions Ferritron can fetch
  cargo ferritron sdk fetch <id> [--version V] [--accept-license]
                                                Fetch an SDK after accepting its license
  cargo ferritron sdk fetch macos [--accept-license]
                                                Have xclang fetch Apple's macOS SDK
  cargo ferritron sdk use <id> <version>        Build with that version
  cargo ferritron sdk source <id> auto|env|managed|path [<folder>]
  cargo ferritron sdk remove <id> <version>     Delete a version from the SDK folder

  <id>: vst3, clap, ara, xclang";

fn spec(id: Option<&String>) -> Result<&'static Sdk, String> {
    let id = id.ok_or_else(|| format!("missing <id>\n\n{USAGE}"))?;
    sdk::find(id).ok_or_else(|| format!("unknown SDK `{id}` (vst3, clap, ara, xclang)"))
}

pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("status") => status(),
        Some("list") => list(),
        Some("available") => available(args.get(1).map(String::as_str)),
        Some("fetch") if args.get(1).map(String::as_str) == Some("macos") => fetch_macos(args.iter().any(|a| a == "--accept-license")),
        Some("fetch") => {
            let spec = spec(args.get(1))?;
            let version = args.iter().position(|a| a == "--version").and_then(|i| args.get(i + 1)).map(String::as_str);
            fetch(spec, version, args.iter().any(|a| a == "--accept-license"))
        }
        Some("use") => {
            let spec = spec(args.get(1))?;
            let version = args.get(2).ok_or("missing <version>")?;
            let root = ferritron_sdk::managed_root()?;
            if sdk::holding(spec, &root.join(spec.id).join(version)).is_none() {
                return Err(format!("{} {version} is not in {} (`cargo ferritron sdk fetch {} --version {version}`)", spec.name, root.join(spec.id).display(), spec.id));
            }
            ferritron_sdk::use_version(spec, version)?;
            println!("{} {version} from the next build on.", spec.name);
            Ok(())
        }
        Some("source") => {
            let spec = spec(args.get(1))?;
            let source = args.get(2).and_then(|s| Source::parse(s)).ok_or("give one of auto, env, managed, path")?;
            let path = args.get(3).map(std::path::PathBuf::from);
            ferritron_sdk::set_source(spec, source, path)?;
            println!("{}: source = {}", spec.name, source.as_str());
            Ok(())
        }
        Some("remove") => {
            let spec = spec(args.get(1))?;
            let version = args.get(2).ok_or("missing <version>")?;
            let dir = ferritron_sdk::remove(spec, version)?;
            println!("Deleted {}", dir.display());
            Ok(())
        }
        _ => Err(format!("usage:\n{USAGE}")),
    }
}

fn status() -> Result<(), String> {
    let context = Context::current()?;
    let settings = sdk::settings_path().map_or_else(|| "(no data folder)".into(), |p| {
        format!("{}{}", p.display(), if p.is_file() { "" } else { " (none: every SDK is `auto`)" })
    });
    println!("sdk.toml:   {settings}");
    println!("SDK folder: {}", context.root().map_or_else(|| "(none)".into(), |r| r.display().to_string()));
    for spec in sdk::ALL {
        let source = context.settings.entry(spec.id).source.as_str();
        match context.resolve(spec) {
            Ok(found) => println!("{:<7} found    {} [{}; source {source}]", spec.id, found.dir.display(), found.origin),
            Err(tried) => {
                println!("{:<7} missing  [source {source}] looked at:", spec.id);
                for place in tried {
                    println!("           - {place}");
                }
            }
        }
    }
    if let Ok(found) = context.resolve(&sdk::XCLANG) {
        let has = ferritron_sdk::has_macos_sdk(&found.dir);
        println!("macos   {}", if has { "found    (xclang's macOS SDK)" } else { "missing  (`cargo ferritron sdk fetch macos`)" });
    }
    Ok(())
}

fn list() -> Result<(), String> {
    let context = Context::current()?;
    let root = ferritron_sdk::managed_root()?;
    println!("{}", root.display());
    for spec in sdk::ALL {
        let chosen = context.settings.entry(spec.id).version;
        let installed = sdk::installed(spec, &root);
        if installed.is_empty() {
            println!("  {:<7} (none)", spec.id);
        }
        for (version, _) in installed {
            let mark = if chosen.as_deref() == Some(&version) { "  (chosen in sdk.toml)" } else { "" };
            println!("  {:<7} {version}{mark}", spec.id);
        }
    }
    Ok(())
}

fn available(id: Option<&str>) -> Result<(), String> {
    let root = ferritron_sdk::managed_root().ok();
    for spec in sdk::ALL.into_iter().filter(|s| id.is_none_or(|i| i == s.id)) {
        for release in ferritron_sdk::releases(spec.id) {
            let size = release.download_size().map_or_else(String::new, |b| format!(", {:.0} MB", b as f64 / 1e6));
            let here = root.as_ref().is_some_and(|r| r.join(spec.id).join(&release.version).is_dir());
            println!(
                "{:<7} {:<10} {}{}{}  {} ({})",
                spec.id,
                release.version,
                if release.recommended { "recommended" } else { "checked" },
                size,
                if here { ", fetched" } else { "" },
                release.license,
                release.license_url
            );
        }
    }
    Ok(())
}

/// Asks in the terminal; true only for an explicit yes.
fn ask(question: &str) -> bool {
    print!("{question} [type yes / no]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "yes" | "y")
}

fn fetch(spec: &Sdk, version: Option<&str>, accepted: bool) -> Result<(), String> {
    let release = ferritron_sdk::release(spec.id, version).ok_or_else(|| {
        let known: Vec<String> = ferritron_sdk::releases(spec.id).into_iter().map(|r| r.version).collect();
        format!("Ferritron knows no {} {} for this computer (known: {})", spec.name, version.unwrap_or("version"), known.join(", "))
    })?;
    let root = ferritron_sdk::managed_root()?;
    println!("{} {}{}", spec.name, release.version, if release.recommended { " (recommended)" } else { "" });
    println!("  License: {}", release.license);
    println!("  Read it: {}", release.license_url);
    for part in release.parts_here() {
        println!("  From:    {}", part.url);
    }
    if let Some(size) = release.download_size() {
        println!("  Size:    {:.0} MB", size as f64 / 1e6);
    }
    if let Some(notice) = &release.notice {
        println!("  {notice}");
    }
    println!("  Into:    {}", root.join(spec.id).join(&release.version).display());
    if !accepted && !ask(&format!("Do you accept the {} for {}?", release.license, spec.name)) {
        return Err("not fetched: the license was not accepted".into());
    }
    let mut last = String::new();
    let target = ferritron_sdk::fetch(
        &release,
        &root,
        &mut |p| {
            let line = match p {
                ferritron_sdk::Progress::Downloading { index, count, bytes, total } => match total {
                    Some(t) if t > 0 => format!("downloading {}/{count}: {:.0}%", index + 1, bytes as f64 * 100.0 / t as f64),
                    _ => format!("downloading {}/{count}: {:.1} MB", index + 1, bytes as f64 / 1e6),
                },
                ferritron_sdk::Progress::Checking { index } => format!("checking {}", index + 1),
                ferritron_sdk::Progress::Unpacking { index } => format!("unpacking {}", index + 1),
                ferritron_sdk::Progress::Installing => "installing".into(),
            };
            if line != last {
                eprint!("\r{line:<40}");
                last = line;
            }
        },
        &AtomicBool::new(false),
    );
    eprintln!();
    let target = target?;
    ferritron_sdk::record_consent(spec.id, &release.version, &release.license, "cargo ferritron sdk fetch")?;
    let source = Context::current()?.settings.entry(spec.id).source;
    if matches!(source, Source::Auto | Source::Managed) {
        ferritron_sdk::use_version(spec, &release.version)?;
    }
    println!("Fetched {} {} into {}", spec.name, release.version, target.display());
    if source == Source::Auto && std::env::var_os(spec.var).is_some() {
        println!("Note: {} is set and wins with source `auto`; unset it or `cargo ferritron sdk source {} managed`.", spec.var, spec.id);
    }
    Ok(())
}

fn fetch_macos(accepted: bool) -> Result<(), String> {
    let context = Context::current()?;
    let xclang = context.resolve(&sdk::XCLANG).map_err(|_| "xclang is needed first: `cargo ferritron sdk fetch xclang`".to_string())?;
    println!("{}", ferritron_sdk::MACOS_SDK_NOTICE);
    if !accepted && !ask("Do you accept Apple's terms for the macOS SDK?") {
        return Err("not fetched: Apple's terms were not accepted".into());
    }
    ferritron_sdk::fetch_macos_sdk(&xclang.dir)?;
    ferritron_sdk::record_consent("macos", "xclang default", "Apple SDK terms", "cargo ferritron sdk fetch macos")?;
    println!("xclang has the macOS SDK.");
    Ok(())
}
