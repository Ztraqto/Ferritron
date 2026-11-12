//! `cargo ferritron bundle <package> --pgo <scenario>`: profile-guided optimisation (X13 of the 2.0 plan).
//!
//! 1. an instrumented build (`-Cprofile-generate`) goes to `<target>/pgo/gen`, bundled as usual;
//! 2. the scenario runs it: `testhost` plays the instrumented VST3 in `ferritron-testhost` (run from
//!    `FERRITRON_HOME`, else the current folder, which must hold the Ferritron workspace), or
//!    `cmd:<command line>` runs your own command, `{bundle}` standing for the first bundle made;
//! 3. `llvm-profdata merge` joins the `.profraw` files into `<target>/pgo/merged.profdata`;
//! 4. the real build runs with `-Cprofile-use` and is bundled into `target/bundled`.
//!
//! `llvm-profdata` comes with the rustup component `llvm-tools`. This tool never installs it: when it
//! is missing it prints the command and stops. The flags go through `CARGO_ENCODED_RUSTFLAGS`, which
//! replaces `RUSTFLAGS` and the `build.rustflags` of `.cargo/config.toml` for these builds.

use crate::lang::{japanese, tr};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `llvm-profdata` of the active toolchain, if the `llvm-tools` component is installed.
pub fn find_llvm_profdata() -> Option<PathBuf> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let text = |args: &[&str]| -> Option<String> {
        let out = Command::new(&rustc).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let sysroot = text(&["--print", "sysroot"])?;
    let host = text(&["-vV"])?.lines().find_map(|l| l.strip_prefix("host: ").map(str::to_string))?;
    let exe = if cfg!(windows) { "llvm-profdata.exe" } else { "llvm-profdata" };
    let path = Path::new(sysroot.trim()).join("lib").join("rustlib").join(host).join("bin").join(exe);
    path.is_file().then_some(path)
}

pub fn missing_tools_message() -> String {
    if japanese() {
        "llvm-profdata が見つかりません。PGO には rustup のコンポーネント llvm-tools が必要です。次を実行してから、もう一度試してください（ツールが自動でインストールすることはありません）:\n    rustup component add llvm-tools".to_string()
    } else {
        "llvm-profdata was not found. PGO needs the rustup component llvm-tools. Run this, then try again (this tool never installs it for you):\n    rustup component add llvm-tools".to_string()
    }
}

/// The flags of the toolchain's environment (`CARGO_ENCODED_RUSTFLAGS`, else `RUSTFLAGS`) plus `extra`,
/// in the encoded form (separated by U+001F, so paths with spaces survive).
pub fn encoded_rustflags(existing_encoded: Option<&str>, existing_plain: Option<&str>, extra: &[String]) -> String {
    let mut flags: Vec<String> = match (existing_encoded, existing_plain) {
        (Some(e), _) if !e.is_empty() => e.split('\u{1f}').map(str::to_string).collect(),
        (_, Some(p)) => p.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    };
    flags.extend(extra.iter().cloned());
    flags.join("\u{1f}")
}

fn set_rustflags(extra: &[String]) {
    let encoded = encoded_rustflags(std::env::var("CARGO_ENCODED_RUSTFLAGS").ok().as_deref(), std::env::var("RUSTFLAGS").ok().as_deref(), extra);
    // Single-threaded here: nothing else reads the environment at the same time.
    unsafe { std::env::set_var("CARGO_ENCODED_RUSTFLAGS", encoded) };
}

/// The `.profraw` files in `dir`.
fn profiles(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    files.sort();
    files
}

/// Runs the scenario against the instrumented bundles. `original_target_dir` is the
/// `CARGO_TARGET_DIR` of the caller (the scenario's own builds must not use the instrumented one).
fn run_scenario(scenario: &str, bundles: &[PathBuf], original_target_dir: &Option<OsString>) -> Result<(), String> {
    let mut cmd;
    if scenario == "testhost" {
        let vst3 = bundles
            .iter()
            .find(|b| b.extension().is_some_and(|e| e == "vst3"))
            .ok_or_else(|| tr("testhost のシナリオには VST3 のバンドルが要ります", "the testhost scenario needs a VST3 bundle").to_string())?;
        cmd = Command::new(crate::cargo_program());
        cmd.args(["run", "--release", "--package", "ferritron-testhost", "--"]).arg(vst3).args(["--seconds", "20"]);
        if let Some(home) = std::env::var_os("FERRITRON_HOME").filter(|h| !h.is_empty()) {
            cmd.current_dir(home);
        }
    } else if let Some(line) = scenario.strip_prefix("cmd:") {
        let first = bundles.first().ok_or("nothing was bundled")?.display().to_string();
        let line = line.replace("{bundle}", &first);
        let mut words = line.split_whitespace();
        let program = words.next().ok_or_else(|| tr("cmd: の後にコマンドを書いてください", "write a command after cmd:").to_string())?;
        cmd = Command::new(program);
        cmd.args(words);
    } else {
        return Err(if japanese() {
            format!("不明なシナリオ `{scenario}`（使えるのは testhost と cmd:<コマンド>）")
        } else {
            format!("unknown scenario `{scenario}` (use testhost or cmd:<command>)")
        });
    }
    // The scenario's own builds are ordinary ones.
    cmd.env_remove("CARGO_ENCODED_RUSTFLAGS");
    match original_target_dir {
        Some(dir) => cmd.env("CARGO_TARGET_DIR", dir),
        None => cmd.env_remove("CARGO_TARGET_DIR"),
    };
    let status = cmd.status().map_err(|e| format!("{}: {e}", tr("シナリオを実行できません", "cannot run the scenario")))?;
    if !status.success() {
        return Err(tr("シナリオが失敗しました", "the scenario failed").to_string());
    }
    Ok(())
}

/// Instrumented build, scenario, merge, optimised build. `bundle` builds and packages with the current
/// environment and returns the bundles it made. `target_directory` is cargo's `target_directory`.
pub fn run(scenario: &str, target_directory: &Path, bundle: &mut dyn FnMut() -> Result<Vec<PathBuf>, String>) -> Result<Vec<PathBuf>, String> {
    let profdata = find_llvm_profdata().ok_or_else(missing_tools_message)?;
    let original_target_dir = std::env::var_os("CARGO_TARGET_DIR");
    let original_flags = std::env::var_os("CARGO_ENCODED_RUSTFLAGS");
    let work = target_directory.join("pgo");
    let (raw, merged, generated) = (work.join("raw"), work.join("merged.profdata"), work.join("gen"));
    let _ = std::fs::remove_dir_all(&raw);
    std::fs::create_dir_all(&raw).map_err(|e| format!("cannot create {}: {e}", raw.display()))?;

    println!("{}", tr("PGO 1/4: 計測用ビルド", "PGO 1/4: instrumented build"));
    unsafe { std::env::set_var("CARGO_TARGET_DIR", &generated) };
    set_rustflags(&[format!("-Cprofile-generate={}", raw.display())]);
    let instrumented = bundle();
    // Back to the caller's environment whatever happened.
    let restore = || unsafe {
        match &original_target_dir {
            Some(d) => std::env::set_var("CARGO_TARGET_DIR", d),
            None => std::env::remove_var("CARGO_TARGET_DIR"),
        }
        match &original_flags {
            Some(f) => std::env::set_var("CARGO_ENCODED_RUSTFLAGS", f),
            None => std::env::remove_var("CARGO_ENCODED_RUSTFLAGS"),
        }
    };
    let instrumented = match instrumented {
        Ok(b) => b,
        Err(e) => {
            restore();
            return Err(e);
        }
    };

    // The build itself ran instrumented build scripts and proc macros that wrote profiles too:
    // only what the scenario writes counts.
    for stale in profiles(&raw) {
        let _ = std::fs::remove_file(stale);
    }
    println!("{}", tr("PGO 2/4: シナリオを実行", "PGO 2/4: running the scenario"));
    let ran = run_scenario(scenario, &instrumented, &original_target_dir);
    if let Err(e) = ran {
        restore();
        return Err(e);
    }
    let files = profiles(&raw);
    if files.is_empty() {
        restore();
        return Err(if japanese() {
            format!("{} にプロファイル（.profraw）が書かれませんでした。シナリオがバンドルを読み込んで終了したか確認してください", raw.display())
        } else {
            format!("no profile (.profraw) was written to {}: check that the scenario loaded the bundle and exited normally", raw.display())
        });
    }

    println!("{}", if japanese() { format!("PGO 3/4: {} 個のプロファイルを統合", files.len()) } else { format!("PGO 3/4: merging {} profiles", files.len()) });
    let status = Command::new(&profdata).arg("merge").arg("-o").arg(&merged).args(&files).status().map_err(|e| {
        restore();
        format!("llvm-profdata: {e}")
    })?;
    if !status.success() {
        restore();
        return Err(tr("llvm-profdata merge が失敗しました", "llvm-profdata merge failed").to_string());
    }

    println!("{}", tr("PGO 4/4: プロファイルを使った最適化ビルド", "PGO 4/4: optimised build with the profile"));
    restore();
    set_rustflags(&[format!("-Cprofile-use={}", merged.display()), "-Cllvm-args=-pgo-warn-missing-function".to_string()]);
    let result = bundle();
    restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_keep_the_existing_ones_and_survive_spaces() {
        let extra = vec!["-Cprofile-generate=C:\\my dir\\raw".to_string()];
        assert_eq!(encoded_rustflags(None, None, &extra), "-Cprofile-generate=C:\\my dir\\raw");
        assert_eq!(encoded_rustflags(None, Some("-Ctarget-cpu=native -Cdebuginfo=0"), &extra), "-Ctarget-cpu=native\u{1f}-Cdebuginfo=0\u{1f}-Cprofile-generate=C:\\my dir\\raw");
        assert_eq!(encoded_rustflags(Some("-Ca\u{1f}-Cb"), Some("-Cignored"), &["-Cc".to_string()]), "-Ca\u{1f}-Cb\u{1f}-Cc");
    }

    #[test]
    fn the_missing_tool_message_names_the_command() {
        assert!(missing_tools_message().contains("rustup component add llvm-tools"));
    }

    #[test]
    fn profiles_lists_only_profraw_files() {
        let dir = std::env::temp_dir().join(format!("ferritron-pgo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.profraw"), "").unwrap();
        std::fs::write(dir.join("a.profraw"), "").unwrap();
        std::fs::write(dir.join("c.txt"), "").unwrap();
        let names: Vec<_> = profiles(&dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.profraw", "b.profraw"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_scenarios_are_refused() {
        let err = run_scenario("nonsense", &[], &None).unwrap_err();
        assert!(err.contains("nonsense"), "{err}");
    }
}
