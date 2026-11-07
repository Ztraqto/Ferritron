//! What Ferritron's build scripts share: finding the plugin-format SDKs the
//! developer provides (Ferritron never ships them), reporting what is missing
//! in words the developer can act on, and skipping native code for
//! cross-OS `cargo check` (`FERRITRON_SKIP_NATIVE`).

use std::path::{Path, PathBuf};

/// Stops the build with `message`, one `cargo::error` per line.
pub fn fail(message: &str) -> ! {
    for line in message.lines() {
        println!("cargo::error={line}");
    }
    std::process::exit(1);
}

/// The SDK folder in environment variable `var`; fails the build with `help`
/// when it is not set or does not contain every file of `probe` (relative
/// to the folder).
pub fn sdk_dir(var: &str, probe: &[&str], help: &str) -> PathBuf {
    println!("cargo::rerun-if-env-changed={var}");
    let Some(dir) = std::env::var_os(var).map(PathBuf::from) else {
        fail(&format!("{var} is not set.\n{help}"));
    };
    if let Some(missing) = probe.iter().find(|p| !dir.join(p).is_file()) {
        fail(&format!("{var} ({}) does not contain {missing}.\n{help}", dir.display()));
    }
    dir
}

/// The SDK folder in `var` if it is set and contains `probe`; `None` otherwise.
pub fn optional_sdk_dir(var: &str, probe: &str) -> Option<PathBuf> {
    println!("cargo::rerun-if-env-changed={var}");
    std::env::var_os(var).map(PathBuf::from).filter(|d| d.join(probe).is_file())
}

/// The developer's ARA SDK, when configured (`ARA_SDK_DIR`): the format
/// adapters then offer ARA to hosts.
pub fn ara_sdk_dir() -> Option<PathBuf> {
    optional_sdk_dir("ARA_SDK_DIR", "ARA_API/ARAInterface.h")
}

/// Whether to skip C, C++ and Objective-C code: `cargo check` for another OS
/// (e.g. macOS from Windows) has no toolchain for it, and the Rust code is
/// still checked.
pub fn skip_native() -> bool {
    println!("cargo::rerun-if-env-changed=FERRITRON_SKIP_NATIVE");
    std::env::var_os("FERRITRON_SKIP_NATIVE").is_some()
}

/// The directory of `ferritron_abi.h` and `ferritron.hpp`, published by
/// ferritron-core to crates that depend on it.
pub fn abi_include() -> PathBuf {
    std::env::var_os("DEP_FERRITRON_ABI_INCLUDE").map(PathBuf::from).expect("ferritron-core publishes its include directory")
}

/// Whether the target is Windows.
pub fn target_windows() -> bool {
    std::env::var_os("CARGO_CFG_WINDOWS").is_some()
}

/// `cargo::rerun-if-changed` for each of `paths`.
pub fn rerun_if_changed<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) {
    for path in paths {
        println!("cargo::rerun-if-changed={}", path.as_ref().display());
    }
}
