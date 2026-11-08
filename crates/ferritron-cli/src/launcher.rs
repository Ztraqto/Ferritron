//! The executable of an app (`launcher` in a format's metadata): a small C
//! program (launcher.c) that loads the plugin binary and calls its entry
//! symbol. Compiled for the target with MSVC (Windows), the system C
//! compiler (Linux, a Mac), Xcode's for iOS, or xclang across to macOS.

use crate::target::Target;
use std::path::{Component, Path};
use std::process::Command;

const SOURCE: &str = include_str!("launcher.c");

/// `to` relative to the folder `from`, with `/` separators.
fn relative(from: &Path, to: &Path) -> String {
    let (from, to): (Vec<Component>, Vec<Component>) = (from.components().collect(), to.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

fn c_string(text: &str) -> String {
    let mut out = String::from("\"");
    for b in text.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}\"\"")), // split so following hex digits are not swallowed
        }
    }
    out.push('"');
    out
}

/// Compiles the launcher to `output`, loading `binary`. `appex`: an Apple app extension.
pub fn build(target: &Target, output: &Path, binary: &Path, appex: bool) -> Result<(), String> {
    let dir = output.parent().ok_or("invalid launcher path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let work = std::env::temp_dir().join(format!("ferritron-launcher-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create {}: {e}", work.display()))?;
    let library = relative(dir, binary);
    std::fs::write(work.join("launcher_config.h"), format!("#define FERRITRON_LIBRARY {}\n", c_string(&library))).map_err(|e| e.to_string())?;
    let source = work.join("launcher.c");
    std::fs::write(&source, SOURCE).map_err(|e| e.to_string())?;
    let result = compile(target, &work, &source, output, appex);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn compile(target: &Target, work: &Path, source: &Path, output: &Path, appex: bool) -> Result<(), String> {
    let mut cmd = match target.os.as_str() {
        "windows" => {
            let triple = format!("{}-pc-windows-msvc", target.arch);
            let tool = cc::windows_registry::find_tool(&triple, "cl.exe")
                .ok_or("building the app needs the MSVC C compiler (Visual Studio Build Tools, \"Desktop development with C++\")")?;
            let mut cmd = tool.to_command();
            cmd.args(["/nologo", "/O2", "/utf-8", "/DUNICODE", "/D_UNICODE"]).arg(format!("/I{}", work.display())).arg(source);
            cmd.arg(format!("/Fe{}", output.display())).arg(format!("/Fo{}\\", work.display()));
            cmd.args(["/link", "/SUBSYSTEM:WINDOWS", "user32.lib", "kernel32.lib"]);
            cmd
        }
        "ios" => {
            if std::env::consts::OS != "macos" {
                return Err("building for iPhone and iPad needs a Mac with Xcode (Apple's iOS SDK is only licensed there)".into());
            }
            let mut cmd = Command::new("xcrun");
            let sdk = if target.triple.as_deref().is_some_and(|t| t.ends_with("-sim")) { "iphonesimulator" } else { "iphoneos" };
            cmd.args(["-sdk", sdk, "clang", "-arch", "arm64", &format!("-mios-version-min={}", crate::target::IOS_MINIMUM), "-O2"]);
            cmd
        }
        _ => {
            let triples = target.cargo_triples();
            let (cc, flags) = crate::target::c_compiler(target, triples.first().map(String::as_str))?;
            let mut cmd = Command::new(cc);
            cmd.args(flags).arg("-O2");
            if triples.len() > 1 {
                // Universal macOS: both architectures in one file.
                cmd.args(["-arch", "arm64", "-arch", "x86_64"]);
            }
            cmd
        }
    };
    if target.os != "windows" {
        cmd.arg(format!("-I{}", work.display())).arg(source).arg("-o").arg(output);
        if target.os == "linux" {
            cmd.arg("-ldl");
        }
        if appex {
            cmd.args(["-DFERRITRON_APPEX", "-fapplication-extension", "-e", "_NSExtensionMain", "-framework", "Foundation"]);
        }
    }
    let status = cmd.current_dir(work).status().map_err(|e| format!("cannot run the C compiler for the app: {e}"))?;
    if !status.success() {
        return Err(format!("building the app's launcher {} failed", output.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn paths_are_relative_to_the_launcher() {
        let root = PathBuf::from("/b/Gain.app");
        assert_eq!(relative(&root.join("Contents/MacOS"), &root.join("Contents/Frameworks/Gain.dylib")), "../Frameworks/Gain.dylib");
        assert_eq!(relative(&root.join("x"), &root.join("x/Gain.dll")), "Gain.dll");
        assert_eq!(c_string("a\"b\\ é"), "\"a\\\"b\\\\ \\xc3\"\"\\xa9\"\"\"");
    }
}
