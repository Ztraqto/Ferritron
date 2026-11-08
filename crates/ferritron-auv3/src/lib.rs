//! AUv3 format adapter (macOS, iPhone, iPad).
//!
//! An AUv3 is an app extension inside an app: `cargo ferritron bundle`
//! puts `<Name> AUv3.appex` into the standalone app (`<Name>.app`, feature
//! `standalone`), on macOS and with `--target aarch64-apple-ios`. Hosts
//! (Logic Pro, GarageBand, AUM, Cubasis, ...) find it once the app was
//! opened.
//!
//! The extension builds on the Audio Unit v2 of `ferritron-au`: its
//! principal class (objc/auv3.m) registers that unit inside the extension
//! and returns an `AUAudioUnitV2Bridge` of it, so AU v2 and AUv3 behave the
//! same (parameters, state, MIDI, latency). The extension's component codes
//! are the project file's `[au]` codes.
//!
//! Written and type-checked; never built or run on a Mac (see the README).

/// The plugin's Audio Unit v2 for `desc`, registered by the principal class.
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[unsafe(no_mangle)]
pub extern "C" fn ferritron_auv3_factory(desc: *const ferritron_au::sys::AudioComponentDescription) -> *mut std::ffi::c_void {
    ferritron_au::factory(desc)
}
