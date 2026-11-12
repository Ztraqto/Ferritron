//! Ferritron: a simple, fast, extensible audio plugin framework.
//!
//! ```ignore
//! use ferritron::prelude::*;
//!
//! #[derive(Default)]
//! struct Gain;
//!
//! #[ferritron::plugin]
//! impl Process for Gain {
//!     fn process(&mut self, ctx: &mut Context) {
//!         let gain = db_to_gain(ctx.params().gain.value());
//!         ctx.io().for_each_sample(|s| *s *= gain);
//!     }
//! }
//! ```
//!
//! Parameters and plugin metadata live in the project file, `<name>.ferritroject` (`.frtx` before 1.2), next to `Cargo.toml`.

pub use ferritron_core::*;
pub use ferritron_macros::plugin;

/// The Ferritron version this plugin is built with (e.g. `"2.0.0"`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Plugin formats. Linking a format crate is all it takes to export that
// format; each one is a feature here. Third-party formats are linked the same
// way from the plugin crate: `use ferritron_aax as _;`.
#[cfg(all(feature = "vst3", not(any(target_family = "wasm", target_os = "android", target_os = "ios"))))]
use ferritron_vst3 as _;
#[cfg(all(feature = "clap", not(any(target_family = "wasm", target_os = "android", target_os = "ios"))))]
use ferritron_clap as _;
#[cfg(all(feature = "au", target_os = "macos"))]
use ferritron_au as _;
#[cfg(all(feature = "lv2", not(any(target_family = "wasm", target_os = "android", target_os = "ios"))))]
use ferritron_lv2 as _;
#[cfg(all(feature = "standalone", not(target_family = "wasm")))]
use ferritron_standalone as _;
#[cfg(all(feature = "auv3", any(target_os = "macos", target_os = "ios")))]
use ferritron_auv3 as _;
#[cfg(all(feature = "aax", any(windows, target_os = "macos")))]
use ferritron_aax as _;
#[cfg(all(feature = "vst2", any(windows, target_os = "macos", target_os = "linux")))]
use ferritron_vst2 as _;

#[cfg(feature = "dsp")]
pub use ferritron_dsp as dsp;

#[cfg(feature = "web")]
#[doc(hidden)]
pub use ferritron_web as web;

/// Emitted by generated code for `[view] backend = "web"`.
#[cfg(feature = "web")]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_web {
    () => {};
}

#[cfg(not(feature = "web"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_web {
    () => {
        ::core::compile_error!(
            "the project file (.ferritroject) declares a web [view]; enable the `web` feature: ferritron = { ..., features = [\"web\"] }"
        );
    };
}

#[cfg(feature = "gpu-view")]
pub use ferritron_gpu_view as gpu_view;

/// Emitted by generated code for `[view] backend = "gpu"`.
#[cfg(feature = "gpu-view")]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_gpu_view {
    () => {};
}

#[cfg(not(feature = "gpu-view"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_gpu_view {
    () => {
        ::core::compile_error!(
            "the project file (.ferritroject) declares a gpu [view]; enable the `gpu-view` feature: ferritron = { ..., features = [\"gpu-view\"] }"
        );
    };
}

#[cfg(feature = "ara")]
pub use ferritron_ara as ara;

/// Emitted by generated code for `[ara]`.
#[cfg(feature = "ara")]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_ara {
    () => {};
}

#[cfg(not(feature = "ara"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_ara {
    () => {
        ::core::compile_error!(
            "the project file (.ferritroject) declares [ara]; enable the `ara` feature: ferritron = { ..., features = [\"ara\"] }"
        );
    };
}

pub mod prelude {
    pub use crate::util::{db_to_gain, gain_to_db};
    pub use crate::{BoolParam, EnumParam, FloatParam, IntParam, ParamRef, ParsedText, Process, Quality, Setup, Tail};
}

/// Exports `ferritron_plugin_entry`, the format-independent entry point.
/// Format crates (VST3, CLAP, ...) find the plugin through it. Called by
/// generated code.
#[doc(hidden)]
#[macro_export]
macro_rules! __export_formats {
    ($plugin:ty) => {
        static __FERRITRON_INFO_C: $crate::abi::PluginInfoC =
            $crate::abi::PluginInfoC::new(<$plugin as $crate::__private::PluginMeta>::INFO);
        static __FERRITRON_API: $crate::abi::PluginApi = $crate::abi::PluginApi::of::<$plugin>(&__FERRITRON_INFO_C);

        #[unsafe(no_mangle)]
        pub extern "C" fn ferritron_plugin_entry() -> *const $crate::abi::PluginApi {
            &__FERRITRON_API
        }

        /// WebAssembly guests (isolation Level 2): lets the host place
        /// buffers in this module's memory. Never freed: the host allocates
        /// once per instance.
        #[cfg(target_family = "wasm")]
        #[unsafe(no_mangle)]
        pub extern "C" fn ferritron_wasm_alloc(size: u32) -> *mut u8 {
            ::std::vec::Vec::<u8>::with_capacity(size as usize).leak().as_mut_ptr()
        }

        /// Entry point of the isolation child process (started via rundll32).
        #[cfg(windows)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn ferritron_host_main(
            _window: *mut ::core::ffi::c_void,
            _instance: *mut ::core::ffi::c_void,
            command_line: *const ::core::ffi::c_char,
            _show: i32,
        ) {
            unsafe { $crate::isolation::host_main(&__FERRITRON_API, command_line) }
        }

        /// Entry point of the isolation child process (started by the
        /// `ferritron-host` helper, which passes its arguments as one line).
        #[cfg(unix)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn ferritron_host_main(command_line: *const ::core::ffi::c_char) {
            unsafe { $crate::isolation::host_main(&__FERRITRON_API, command_line) }
        }
    };
}
