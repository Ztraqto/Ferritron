// Compiles the C++ test host against the developer-provided VST3 SDK.
// Windows only for now (it embeds editors in Win32 windows).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(have_clap)");
    if !ferritron_build::target_windows() {
        return;
    }
    ferritron_build::rerun_if_changed(["host.cpp", "clap_checks.inc", "shared_runtime.inc"]);
    let sdk = ferritron_build::sdk("vst3");
    let clap = ferritron_build::optional_sdk("clap");
    let vst2 = ferritron_build::optional_sdk("vst2");
    if ferritron_build::skip_native() {
        return;
    }
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").flag_if_supported("/EHsc").include(sdk).file("host.cpp");
    // The format-independent ABI, for the --shared-runtime check.
    build.include(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/ferritron-core/include"));
    // Loads VST 2 plugin libraries too when the developer has the VST 2 SDK (never fetched by Ferritron).
    if let Some(vst2) = vst2 {
        build.include(vst2).define("FERRITRON_HAVE_VST2", None);
    }
    // The CLAP checks are compiled in when the CLAP headers are available too.
    if let Some(clap) = clap {
        build.include(clap.join("include")).define("FERRITRON_HAVE_CLAP", None);
        println!("cargo::rustc-cfg=have_clap");
    }
    build.compile("ferritron_testhost");
    for lib in ["user32", "gdi32", "ole32"] {
        println!("cargo::rustc-link-lib={lib}");
    }
}
