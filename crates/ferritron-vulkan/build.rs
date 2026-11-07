//! Ferritron's own Vulkan shaders are written in Slang (`shaders/*.slang`).
//! Their SPIR-V is kept next to them, so building needs no Slang; when
//! Slang is available (`SLANGC`, `SLANG_DIR` or on `PATH`) the shaders are
//! compiled again and the SPIR-V beside them is refreshed.

use std::path::{Path, PathBuf};
use std::process::Command;

/// (shader, extra slangc arguments)
const SHADERS: &[(&str, &[&str])] = &[
    ("modal", &["-capability", "spvGroupNonUniform", "-capability", "spvGroupNonUniformArithmetic"]),
    ("mixer", &["-capability", "spvCooperativeMatrixKHR"]),
];

fn slangc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SLANGC") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("SLANG_DIR") {
        return Some(Path::new(&dir).join("bin").join(if cfg!(windows) { "slangc.exe" } else { "slangc" }));
    }
    Command::new("slangc").arg("-v").output().ok().map(|_| PathBuf::from("slangc"))
}

fn main() {
    println!("cargo::rerun-if-env-changed=SLANGC");
    println!("cargo::rerun-if-env-changed=SLANG_DIR");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let slangc = slangc();
    for (name, extra) in SHADERS {
        let source = dir.join(format!("{name}.slang"));
        let kept = dir.join(format!("{name}.spv"));
        println!("cargo::rerun-if-changed={}", source.display());
        println!("cargo::rerun-if-changed={}", kept.display());
        let target = out.join(format!("{name}.spv"));
        let compiled = slangc.as_ref().is_some_and(|slangc| {
            Command::new(slangc)
                .arg(&source)
                .args(["-target", "spirv", "-profile", "spirv_1_5", "-entry", "main", "-stage", "compute", "-o"])
                .arg(&target)
                .args(*extra)
                .output()
                .is_ok_and(|o| o.status.success())
        });
        if compiled {
            // Keep the SPIR-V beside the source current for builds without Slang.
            if std::fs::read(&kept).ok() != std::fs::read(&target).ok() {
                let _ = std::fs::copy(&target, &kept);
            }
        } else {
            std::fs::copy(&kept, &target).unwrap_or_else(|e| panic!("{}: {e} (and Slang is not available to compile it)", kept.display()));
        }
    }
}
