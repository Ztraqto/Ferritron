//! Compiles the Objective-C view factory (macOS): hosts find an Audio Unit's
//! Cocoa editor by class name inside the bundle, so that class must be in
//! the binary, not created at run time. Everything else is Rust.

fn main() {
    println!("cargo::rerun-if-changed=objc/view_factory.m");
    println!("cargo::rerun-if-env-changed=FERRITRON_SKIP_NATIVE");
    // iOS: the v2 unit only (AUv3 builds on it); no Cocoa view.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        for framework in ["AudioToolbox", "CoreAudio", "CoreFoundation", "Foundation"] {
            println!("cargo::rustc-link-lib=framework={framework}");
        }
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // A class name of this build: plugins built elsewhere never share it.
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in out_dir.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let class = format!("FerritronAUView_{hash:016x}");
    println!("cargo::rustc-env=FERRITRON_AU_VIEW_CLASS={class}");
    for framework in ["AudioToolbox", "AudioUnit", "CoreAudio", "CoreFoundation", "Foundation", "AppKit"] {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
    // `cargo check` for macOS from another OS has no Objective-C toolchain.
    if std::env::var_os("FERRITRON_SKIP_NATIVE").is_some() {
        return;
    }
    cc::Build::new().file("objc/view_factory.m").flag("-fobjc-arc").define("FERRITRON_AU_VIEW_CLASS", class.as_str()).compile("ferritron_au_view");
}
