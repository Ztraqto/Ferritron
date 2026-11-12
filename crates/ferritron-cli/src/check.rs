//! `cargo ferritron check <package>`: looks a plugin package over and warns about what a build
//! does not notice.
//!
//! * a `.frtx` project file that is still there (Ferritron 2.0 does not read it: `migrate-frtx`),
//! * a plugin that still depends on Ferritron 1.x (this tool is 2.x),
//! * a latency the plugin reports that is not the delay it really has.
//!
//! The latency is measured by the plugin's own test, not by this tool: measuring needs the plugin's
//! type, and only the plugin's crate has it. The tool runs `cargo test -p <package> -- latency`
//! and reads the line `ferritron::testing::check_latency::<MyPlugin>()` prints
//! (`FERRITRON-CHECK latency measured=<n> reported=<n>`). A package without such a test is told how
//! to add one. Warnings do not fail the command.

use crate::compat;
use crate::lang::{japanese, tr};
use std::path::Path;
use std::process::{Command, Stdio};

/// What the latency test said.
#[derive(Debug, PartialEq, Eq)]
pub enum Latency {
    Match { samples: u32 },
    Mismatch { measured: u32, reported: u32 },
    /// The plugin does not answer an impulse (an instrument): nothing to compare.
    Silent { reported: u32 },
    /// No test printed a result.
    NotFound,
}

/// Reads `FERRITRON-CHECK latency ...` from the output of the test run. The last such line counts,
/// except that a mismatch wins over lines that match.
pub fn parse_latency(output: &str) -> Latency {
    let mut result = Latency::NotFound;
    for line in output.lines() {
        // The test harness prints "test name ... " first, on the same line (with --nocapture).
        let Some(at) = line.find("FERRITRON-CHECK latency ") else { continue };
        let rest = line[at + "FERRITRON-CHECK latency ".len()..].trim();
        let field = |name: &str| rest.split_whitespace().find_map(|w| w.strip_prefix(name)?.strip_prefix('=')?.parse::<u32>().ok());
        let found = match (field("measured"), field("reported"), rest.starts_with("silent")) {
            (Some(measured), Some(reported), _) if measured == reported => Latency::Match { samples: measured },
            (Some(measured), Some(reported), _) => Latency::Mismatch { measured, reported },
            (None, Some(reported), true) => Latency::Silent { reported },
            _ => continue,
        };
        // A mismatch is never hidden by a later line that matches (a test of another setting).
        if !matches!(result, Latency::Mismatch { .. }) || matches!(found, Latency::Mismatch { .. }) {
            result = found;
        }
    }
    result
}

const SNIPPET: &str = "    #[test]\n    fn reported_latency_matches_the_delay() {\n        ferritron::testing::check_latency::<MyPlugin>().unwrap();\n    }";

/// The text for a result, and whether it is a warning.
pub fn latency_message(result: &Latency) -> (String, bool) {
    let ja = japanese();
    match result {
        Latency::Match { samples } => (
            if ja { format!("レイテンシ: 申告と実測が一致しています（{samples} サンプル）") } else { format!("latency: the reported value matches the measured delay ({samples} samples)") },
            false,
        ),
        Latency::Mismatch { measured, reported } => {
            let diff = i64::from(*measured) - i64::from(*reported);
            (
                if ja {
                    format!("警告: レイテンシの申告と実測が一致しません。申告 {reported} サンプル、実測 {measured} サンプル（差 {diff:+}）。ホストの遅延補償がずれます。`Process::latency()` や `ctx.request_latency()` の値、または処理の遅延を直してください")
                } else {
                    format!("warning: the reported latency ({reported} samples) does not match the measured delay ({measured} samples, difference {diff:+}); the host's delay compensation will be off. Fix the value of `Process::latency()` / `ctx.request_latency()` or the delay itself")
                },
                true,
            )
        }
        Latency::Silent { reported } => (
            if ja {
                format!("レイテンシ: インパルスに反応しない（無音）ので比較しませんでした（申告 {reported} サンプル）")
            } else {
                format!("latency: the plugin is silent for an impulse, so nothing was compared (reported {reported} samples)")
            },
            false,
        ),
        Latency::NotFound => (
            if ja {
                format!("レイテンシ: 測定するテストが見つかりません。プラグインのテストに次を追加してください（テスト名に latency を含めます）:\n{SNIPPET}")
            } else {
                format!("latency: no test measured it. Add this to the plugin's tests (the name must contain `latency`):\n{SNIPPET}")
            },
            false,
        ),
    }
}

/// Warning about a `.frtx` project file (before 1.2), if the package has one.
pub fn frtx_warning(package_dir: &Path) -> Option<String> {
    let file = ferritron_manifest::locate_any(package_dir).ok()?;
    let old = file.extension().is_some_and(|e| e.eq_ignore_ascii_case(ferritron_manifest::OLD_EXTENSION));
    old.then(|| {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        if japanese() {
            format!("警告: 古い拡張子 .frtx のプロジェクトファイル（{name}）が残っています。Ferritron 2.0 は読み込みません。`cargo ferritron migrate-frtx <package>` で .ferritroject に名前を変更してください")
        } else {
            format!("warning: the project file {name} still has the old .frtx extension, which Ferritron 2.0 does not read. Rename it to .ferritroject with `cargo ferritron migrate-frtx <package>`")
        }
    })
}

/// Runs the package's tests whose names contain `latency` and reads the result.
pub fn measure_latency(cargo: &str, package: &str) -> Result<Latency, String> {
    let output = Command::new(cargo)
        .args(["test", "--package", package, "--lib", "--", "latency", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{} {e}", tr("cargo test を実行できません:", "cannot run cargo test:")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result = parse_latency(&stdout);
    if result == Latency::NotFound && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut tail: Vec<&str> = stderr.lines().rev().take(15).collect();
        tail.reverse();
        return Err(format!(
            "{}\n{}",
            tr("cargo test が失敗しました（テストをビルドできないか、テストが失敗しました）:", "cargo test failed (the tests did not build, or a test failed):"),
            tail.join("\n")
        ));
    }
    Ok(result)
}

pub fn run(package: &str) -> Result<(), String> {
    let cargo = crate::cargo_program();
    let dir = crate::package_dir(&cargo, package).ok_or_else(|| format!("package `{package}` not found"))?;
    let mut warnings = 0;
    if let Some(version) = crate::cargo_metadata(&cargo, false).as_ref().and_then(compat::plugin_framework_version)
        && let Some(note) = compat::legacy_plugin_notice(env!("CARGO_PKG_VERSION"), &version)
    {
        return Err(note);
    }
    match frtx_warning(&dir) {
        Some(w) => {
            eprintln!("{w}");
            warnings += 1;
        }
        None => println!("{}", tr("プロジェクトファイル: .ferritroject です", "project file: .ferritroject")),
    }
    println!("{}", tr("レイテンシを測定します（cargo test）…", "measuring the latency (cargo test)..."));
    let (text, is_warning) = latency_message(&measure_latency(&cargo, package)?);
    if is_warning {
        eprintln!("{text}");
        warnings += 1;
    } else {
        println!("{text}");
    }
    if warnings == 0 {
        println!("{}", tr("警告はありません", "no warnings"));
    } else {
        println!("{}", if japanese() { format!("警告 {warnings} 件") } else { format!("{warnings} warning(s)") });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_result_lines() {
        assert_eq!(parse_latency("running 1 test\nFERRITRON-CHECK latency measured=64 reported=64\ntest x ... ok"), Latency::Match { samples: 64 });
        assert_eq!(parse_latency("FERRITRON-CHECK latency measured=71 reported=64"), Latency::Mismatch { measured: 71, reported: 64 });
        assert_eq!(parse_latency("FERRITRON-CHECK latency silent reported=5120"), Latency::Silent { reported: 5120 });
        assert_eq!(parse_latency("test result: ok"), Latency::NotFound);
        assert_eq!(parse_latency("FERRITRON-CHECK latency measured=x reported=1"), Latency::NotFound);
        // the last line wins when several tests print one
        assert_eq!(
            parse_latency("FERRITRON-CHECK latency measured=1 reported=1\nFERRITRON-CHECK latency measured=3 reported=2"),
            Latency::Mismatch { measured: 3, reported: 2 }
        );
    }

    #[test]
    fn a_mismatch_is_a_warning_and_the_others_are_not() {
        let (text, warning) = latency_message(&Latency::Mismatch { measured: 71, reported: 64 });
        assert!(warning && text.contains("64") && text.contains("71") && text.contains("+7"), "{text}");
        for ok in [Latency::Match { samples: 3 }, Latency::Silent { reported: 0 }, Latency::NotFound] {
            assert!(!latency_message(&ok).1);
        }
        assert!(latency_message(&Latency::NotFound).0.contains("check_latency"));
    }

    #[test]
    fn warns_about_a_left_over_frtx_file() {
        let dir = std::env::temp_dir().join(format!("ferritron-cli-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(frtx_warning(&dir).is_none(), "no project file at all");
        std::fs::write(dir.join("p.ferritroject"), "").unwrap();
        assert!(frtx_warning(&dir).is_none());
        std::fs::remove_file(dir.join("p.ferritroject")).unwrap();
        std::fs::write(dir.join("p.frtx"), "").unwrap();
        let w = frtx_warning(&dir).unwrap();
        assert!(w.contains("p.frtx") && w.contains("migrate-frtx"), "{w}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
