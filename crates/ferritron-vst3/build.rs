//! Compiles the VST3 shim against the developer's own copy of the VST3 SDK.
//!
//! Ferritron does not ship the SDK. Download it from Steinberg and point
//! `VST3_SDK_DIR` at the folder that contains `pluginterfaces/`.

const HELP: &str = "\
Ferritron does not include the VST3 SDK; please provide your own copy.
  1. Download it from Steinberg: https://github.com/steinbergmedia/vst3sdk
     (or https://www.steinberg.net/developers/) and accept its license.
  2. Set VST3_SDK_DIR to the SDK folder (the one containing `pluginterfaces`),
     e.g. in your project's .cargo/config.toml:
         [env]
         VST3_SDK_DIR = \"C:/SDKs/vst3sdk\"
  Or build without VST3: ferritron = { ..., default-features = false, features = [\"clap\"] }";

fn main() {
    ferritron_build::rerun_if_changed(["shim/vst3_shim.cpp"]);
    let sdk = ferritron_build::sdk_dir("VST3_SDK_DIR", &["pluginterfaces/vst/ivstcomponent.h"], HELP);
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
