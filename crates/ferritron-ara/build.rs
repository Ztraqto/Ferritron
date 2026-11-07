//! Compiles the ARA bridge against the developer's own copy of the ARA SDK.
//!
//! Ferritron does not ship the SDK. Download it from Celemony and point
//! `ARA_SDK_DIR` at the folder that contains `ARA_API/` and `ARA_Library/`.

const HELP: &str = "\
Ferritron does not include the ARA SDK; please provide your own copy.
  1. Download it from Celemony: https://github.com/Celemony/ARA_SDK
     (git clone --recursive) and accept its license (Apache 2.0).
  2. Set ARA_SDK_DIR to the SDK folder (the one containing `ARA_API` and `ARA_Library`),
     e.g. in your project's .cargo/config.toml:
         [env]
         ARA_SDK_DIR = \"C:/SDKs/ARA_SDK\"
  The VST3 and CLAP adapters read the same variable to offer ARA to hosts.";

fn main() {
    ferritron_build::rerun_if_changed(["bridge/ferritron_ara.cpp"]);
    let sdk = ferritron_build::sdk_dir("ARA_SDK_DIR", &["ARA_API/ARAInterface.h", "ARA_Library/PlugIn/ARAPlug.cpp"], HELP);
    let library = sdk.join("ARA_Library");
    if ferritron_build::skip_native() {
        return;
    }
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .flag_if_supported("/EHsc")
        .include(&sdk)
        .define("NDEBUG", None)
        .define("NOMINMAX", "1")
        .warnings(false)
        .file("bridge/ferritron_ara.cpp")
        .file(library.join("PlugIn/ARAPlug.cpp"))
        .file(library.join("Dispatch/ARAPlugInDispatch.cpp"))
        .file(library.join("Utilities/ARAChannelFormat.cpp"))
        .file(library.join("Utilities/ARAPitchInterpretation.cpp"));
    build.compile("ferritron_ara_bridge");
    // ARADebug.c is C.
    cc::Build::new().include(&sdk).define("NDEBUG", None).warnings(false).file(library.join("Debug/ARADebug.c")).compile("ferritron_ara_debug");
}
