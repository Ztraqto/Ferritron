//! Hot reload of the plugin core while the host keeps running.
//!
//! `cargo ferritron bundle <pkg> --dev` writes a marker file next to the
//! plugin binary (`<binary>.ferritron-hot-reload`) naming the library cargo
//! builds (e.g. `target/debug/gain.dll`). The
//! binary the host loaded then acts as a proxy: it loads a private copy of
//! that library and forwards every ABI call to it. When cargo rewrites the
//! library, the proxy loads the new copy, moves each instance's state over and
//! swaps instances between audio blocks. The host never notices.
//!
//! Only compiled into debug builds (Windows, macOS, Linux); release builds
//! call straight into the plugin.

use crate::abi::PluginApi;

#[cfg(all(any(windows, unix), debug_assertions))]
mod proxy;

/// Returns the function table a format shim should use: a hot-reload proxy
/// when this module belongs to a dev bundle, otherwise `api` itself.
pub fn resolve(api: &'static PluginApi) -> &'static PluginApi {
    crate::isolation::remember(api);
    // In process isolation, the child process reloads (see `dev`).
    if let Some(isolated) = crate::isolation::install(api) {
        return isolated;
    }
    #[cfg(all(any(windows, unix), debug_assertions))]
    if let Some(proxy) = proxy::install(api) {
        return proxy;
    }
    api
}

/// The table an isolation child process runs: the hot-reload proxy for
/// development bundles, so isolated plugins reload too.
#[cfg_attr(not(any(windows, unix)), allow(dead_code))]
pub(crate) fn dev(api: &'static PluginApi) -> &'static PluginApi {
    #[cfg(all(any(windows, unix), debug_assertions))]
    if let Some(proxy) = proxy::install(api) {
        return proxy;
    }
    api
}

/// Suffix appended to the plugin binary's file name to form the marker path.
pub const MARKER_SUFFIX: &str = ".ferritron-hot-reload";
