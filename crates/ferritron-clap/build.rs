//! Compiles the CLAP shim against the developer's own copy of the CLAP headers.
//!
//! Ferritron does not ship the CLAP SDK. Download it and point `CLAP_SDK_DIR`
//! at the folder that contains `include/clap/clap.h`.

const HELP: &str = "\
Ferritron does not include the CLAP SDK; please provide your own copy.
  1. Download it from https://github.com/free-audio/clap and accept its license.
  2. Set CLAP_SDK_DIR to the SDK folder (the one containing `include/clap/clap.h`),
     e.g. in your project's .cargo/config.toml:
         [env]
         CLAP_SDK_DIR = \"C:/SDKs/clap\"
  Or build without CLAP: ferritron = { ..., default-features = false, features = [\"vst3\"] }";

fn main() {
    ferritron_build::rerun_if_changed(["shim/clap_shim.c"]);
    let include = ferritron_build::sdk_dir("CLAP_SDK_DIR", &["include/clap/clap.h"], HELP).join("include");
    // The CLAP version the shim is compiled against, for clap_entry (Rust).
    let version = std::fs::read_to_string(include.join("clap/version.h")).unwrap_or_default();
    for part in ["MAJOR", "MINOR", "REVISION"] {
        let value = version
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("#define CLAP_VERSION_{part} ")))
            .map(|v| v.trim().trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
            .unwrap_or_else(|| ferritron_build::fail(&format!("cannot read CLAP_VERSION_{part} from clap/version.h")));
        println!("cargo::rustc-env=FERRITRON_CLAP_VERSION_{part}={value}");
    }
    let ara = ferritron_build::ara_sdk_dir();
    if ferritron_build::skip_native() {
        return;
    }
    let mut build = cc::Build::new();
    build.std("c11").include(include).include(ferritron_build::abi_include()).file("shim/clap_shim.c");
    if let Some(ara) = &ara {
        build.include(ara).define("FERRITRON_HAVE_ARA", None);
    }
    build.compile("ferritron_clap_shim");
    if ferritron_build::target_windows() {
        println!("cargo::rustc-link-lib=user32"); // DPI queries
    }
}
