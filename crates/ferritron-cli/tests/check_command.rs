//! `cargo ferritron check`, run for real on the workspace's packages: the examples must not be
//! warned about, and a plugin whose delay is not what it reports must be.

use std::path::PathBuf;
use std::process::{Command, Output};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn check(package: &str, language: &str, skew: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-ferritron"));
    cmd.args(["check", package]).current_dir(workspace()).env("FERRITRON_LANGUAGE", language).env_remove("LATENCY_CHECK_SKEW");
    if let Some(skew) = skew {
        cmd.env("LATENCY_CHECK_SKEW", skew);
    }
    cmd.output().expect("cargo-ferritron runs")
}

fn text(out: &Output) -> (String, String) {
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn an_example_with_matching_latency_is_not_warned_about() {
    for (package, language, ok_text) in [("gain", "en", "matches the measured delay (0 samples)"), ("latency-check", "ja", "申告と実測が一致しています（64 サンプル）")] {
        let out = check(package, language, None);
        let (stdout, stderr) = text(&out);
        assert!(out.status.success(), "{package}: {stdout}\n{stderr}");
        assert!(stdout.contains(ok_text), "{package}: {stdout}");
        assert!(!stderr.contains("warning") && !stderr.contains("警告"), "{package}: {stderr}");
    }
}

#[test]
fn a_plugin_that_delays_more_than_it_reports_is_warned_about_in_japanese_and_english() {
    let ja = check("latency-check", "ja", Some("7"));
    let (stdout, stderr) = text(&ja);
    assert!(ja.status.success(), "warnings do not fail the command: {stderr}");
    assert!(stderr.contains("警告: レイテンシの申告と実測が一致しません。申告 64 サンプル、実測 71 サンプル（差 +7）"), "{stderr}");
    assert!(stdout.contains("警告 1 件"), "{stdout}");

    let en = check("latency-check", "en", Some("7"));
    let (stdout, stderr) = text(&en);
    assert!(stderr.contains("warning: the reported latency (64 samples) does not match the measured delay (71 samples, difference +7)"), "{stderr}");
    assert!(stdout.contains("1 warning(s)"), "{stdout}");
}

#[test]
fn a_package_without_a_latency_test_is_told_how_to_add_one() {
    // `sine-synth` has no test named like that
    let out = check("sine-synth", "en", None);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("ferritron::testing::check_latency::<MyPlugin>()"), "{stdout}");
}

fn temp_package(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferritron-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    dir
}

#[test]
fn migrate_frtx_renames_the_project_file_and_check_warns_before() {
    let dir = temp_package("oldproj");
    let project = "[plugin]\nid = \"a.b\"\nname = \"Old\"\n";
    std::fs::write(dir.join("oldproj.frtx"), project).unwrap();
    let run = |args: &[&str], language: &str| {
        Command::new(env!("CARGO_BIN_EXE_cargo-ferritron")).args(args).current_dir(&dir).env("FERRITRON_LANGUAGE", language).output().unwrap()
    };
    // check warns about the leftover file (the latency test of this empty package is not found, which is no warning)
    let checked = run(&["check", "oldproj"], "ja");
    let stderr = String::from_utf8_lossy(&checked.stderr).into_owned();
    assert!(stderr.contains("警告: 古い拡張子 .frtx") && stderr.contains("migrate-frtx"), "{stderr}");

    let out = run(&["migrate-frtx", "oldproj"], "en");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success() && stdout.contains("Renamed:") && stdout.contains("oldproj.ferritroject"), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(!dir.join("oldproj.frtx").exists());
    assert_eq!(std::fs::read_to_string(dir.join("oldproj.ferritroject")).unwrap(), project);

    // nothing left to do the second time
    let again = run(&["migrate-frtx", "oldproj"], "ja");
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("名前を変更する .frtx ファイルがありません") || String::from_utf8_lossy(&again.stderr).contains("has no .frtx file to rename"));
    let _ = std::fs::remove_dir_all(dir);
}
