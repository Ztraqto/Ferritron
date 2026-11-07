//! Compiles the VST3 shim against the developer's own copy of the VST3 SDK.
//!
//! Ferritron does not ship the SDK: it is found by Ferritron's rule
//! (`ferritron_build::sdk`: `VST3_SDK_DIR`, `sdk.toml`, the managed folder).

const HINT: &str = "Or build without VST3: ferritron = { ..., default-features = false, features = [\"clap\"] }";

fn main() {
    ferritron_build::rerun_if_changed(["shim/vst3_shim.cpp"]);
    let sdk = ferritron_build::sdk_with_hint("vst3", HINT);
    let ara = ferritron_build::ara_sdk_dir();
    if ferritron_build::skip_native() {
        return;
    }
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").flag_if_supported("/EHsc").include(&sdk).include(ferritron_build::abi_include()).file("shim/vst3_shim.cpp");
    if let Some(ara) = &ara {
        build.include(ara).define("FERRITRON_HAVE_ARA", None);
    }
    build.compile("ferritron_vst3_shim");
    if ferritron_build::target_windows() {
        println!("cargo::rustc-link-lib=user32"); // DPI queries
    }
}
