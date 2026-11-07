//! Compiles the ARA bridge against the developer's own copy of the ARA SDK.
//!
//! Ferritron does not ship the SDK: it is found by Ferritron's rule
//! (`ferritron_build::sdk`: `ARA_SDK_DIR`, `sdk.toml`, the managed folder).

const HINT: &str = "The ARA SDK is Apache 2.0; the VST3 and CLAP adapters find it the same way to offer ARA to hosts.";

fn main() {
    ferritron_build::rerun_if_changed(["bridge/ferritron_ara.cpp"]);
    let sdk = ferritron_build::sdk_with_hint("ara", HINT);
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
