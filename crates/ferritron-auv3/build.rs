//! Compiles the extension's principal class (Objective-C) for macOS and iOS.
//! Its name is unique to this build, so two Ferritron AUv3s loaded into one
//! host process never share a class; `cargo ferritron bundle` reads it back
//! from the binary's exports for the extension's Info.plist.

fn main() {
    println!("cargo::rerun-if-changed=objc/auv3.m");
    println!("cargo::rerun-if-env-changed=FERRITRON_SKIP_NATIVE");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" && os != "ios" {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in out_dir.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let class = format!("FerritronAUv3_{hash:016x}");
    for framework in ["AudioToolbox", "CoreAudioKit", "Foundation", "AVFAudio"] {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
    println!("cargo::rustc-link-lib=framework={}", if os == "macos" { "AppKit" } else { "UIKit" });
    // `cargo check` from another OS has no Objective-C toolchain.
    if std::env::var_os("FERRITRON_SKIP_NATIVE").is_some() {
        return;
    }
    cc::Build::new().file("objc/auv3.m").flag("-fobjc-arc").flag("-fapplication-extension").define("FERRITRON_AUV3_CLASS", class.as_str()).compile("ferritron_auv3");
}
