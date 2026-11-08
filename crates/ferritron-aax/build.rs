//! Compiles the AAX shim and the SDK's own library sources (AAXLibrary and
//! the ACF entry points) against the developer's AAX SDK, found by
//! Ferritron's rule (`ferritron_build::sdk`: `AAX_SDK_DIR`, `sdk.toml`, the
//! managed folder). The entry points are compiled under `ferritron_aax_*`
//! names; src/lib.rs exports the real ones.

use std::path::{Path, PathBuf};

const HINT: &str = "AAX needs Avid's AAX SDK (https://developer.avid.com, sign up), then AAX_SDK_DIR. Or leave out the `aax` feature.";

const ENTRY_POINTS: [&str; 7] = [
    "ACFRegisterPlugin",
    "ACFRegisterComponent",
    "ACFGetClassFactory",
    "ACFCanUnloadNow",
    "ACFStartup",
    "ACFShutdown",
    "ACFGetSDKVersion",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "cpp") {
            out.push(path);
        }
    }
}

fn main() {
    ferritron_build::rerun_if_changed(["shim/aax_shim.cpp"]);
    println!("cargo::rustc-check-cfg=cfg(ferritron_aax)");
    // Avid's SDK cannot be fetched for the developer: without it the crate
    // builds empty (workspace builds keep working) and says so; the plugin
    // then simply has no AAX.
    let Some(sdk) = ferritron_build::optional_sdk("aax") else {
        println!("cargo::warning=AAX SDK not found: built without AAX. {HINT}");
        return;
    };
    if ferritron_build::skip_native() {
        return;
    }
    println!("cargo::rustc-cfg=ferritron_aax");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(ferritron_build::abi_include());
    for include in ["Interfaces", "Interfaces/ACF", "Libs/AAXLibrary/include"] {
        build.include(sdk.join(include));
    }
    let mut files = Vec::new();
    sources(&sdk.join("Libs/AAXLibrary/source"), &mut files);
    // The ACF entry points: AAX_Exports.cpp, wherever this SDK version keeps it.
    let mut all = Vec::new();
    sources(&sdk.join("Interfaces"), &mut all);
    files.extend(
        all.into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "AAX_Exports.cpp")),
    );
    if files.is_empty() {
        ferritron_build::fail(&format!(
            "{} has no AAXLibrary sources (Libs/AAXLibrary/source); is it the AAX SDK?",
            sdk.display()
        ));
    }
    for name in ENTRY_POINTS {
        build.define(name, format!("ferritron_aax_{name}").as_str());
    }
    if ferritron_build::target_windows() {
        build
            .define("WINDOWS_VERSION", "1")
            .define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        build
            .define("MAC_VERSION", "1")
            .flag("-Wno-deprecated-declarations");
    }
    build
        .files(files)
        .file("shim/aax_shim.cpp")
        .compile("ferritron_aax_shim");
    if ferritron_build::target_windows() {
        for lib in ["user32", "ole32"] {
            println!("cargo::rustc-link-lib={lib}");
        }
    } else {
        for framework in ["CoreFoundation", "AppKit"] {
            println!("cargo::rustc-link-lib=framework={framework}");
        }
    }
}
