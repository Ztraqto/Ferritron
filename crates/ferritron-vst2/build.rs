//! Compiles the VST 2 shim against the developer's own copy of the VST 2 SDK.
//!
//! Steinberg no longer licenses VST 2, and Ferritron neither ships nor fetches the SDK:
//! it is found by Ferritron's rule (`ferritron_build::sdk`: `VST2_SDK_DIR`, `sdk.toml`,
//! the managed folder). Without it the crate builds empty, with a warning.

const HINT: &str = "VST 2 needs your own copy of the VST 2 SDK (VST2_SDK_DIR points at the folder that holds pluginterfaces/vst2.x). Or leave out the `vst2` feature.";

fn main() {
    ferritron_build::rerun_if_changed(["shim/vst2_shim.cpp"]);
    println!("cargo::rustc-check-cfg=cfg(ferritron_vst2)");
    let Some(sdk) = ferritron_build::optional_sdk("vst2") else {
        println!("cargo::warning=VST 2 SDK not found: built without VST 2. {HINT}");
        return;
    };
    if ferritron_build::skip_native() {
        return;
    }
    println!("cargo::rustc-cfg=ferritron_vst2");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .flag_if_supported("/EHsc")
        .include(&sdk)
        .include(ferritron_build::abi_include())
        .file("shim/vst2_shim.cpp")
        .compile("ferritron_vst2_shim");
}
