//! Crash isolation, level 3 (design doc, 8.5): the plugin runs in a separate
//! process, so a crash cannot take the host down.
//!
//! The library the host loaded becomes a client: [`crate::reload::resolve`] swaps in a
//! function table that forwards processing to a child process. The child is
//! the same plugin library, which runs its exported `ferritron_host_main`:
//! loaded by Windows' `rundll32.exe` (no executable to ship), or on macOS and
//! Linux by the small `ferritron-host` helper that `cargo ferritron bundle`
//! puts into the bundle.
//!
//! - Audio, notes and parameter events go through shared memory; kernel
//!   events (Windows) or named FIFOs (Unix) wake the child and signal
//!   completion.
//! - Editors of isolated plugins open in a third process on Windows. macOS
//!   cannot embed another process's window, so there they open in the host
//!   process and talk to the isolated instance through the same mirror.
//! - Parameter values are mirrored lock-free in shared memory. Plugin state
//!   is saved and loaded by a "shadow" instance in the host process that
//!   never processes audio, so state works even while the child restarts.
//! - If the child dies, the plugin outputs silence, a watcher starts a new
//!   child, and processing resumes with the same parameters.
//!
//! Enabled with `[isolation] mode = "process"` in the project file, or by
//! `FERRITRON_ISOLATION=process` (`=guarded` forces it off).

use crate::abi::PluginApi;

#[cfg(any(windows, unix))]
mod client;
#[cfg(any(windows, unix))]
mod os;
#[cfg(any(windows, unix))]
mod server;
#[cfg(any(windows, unix))]
mod shm;
#[cfg(windows)]
mod view;
#[cfg(windows)]
mod win32;

/// Remembers the plugin's table, for editors in a separate process.
pub(crate) fn remember(api: &'static PluginApi) {
    #[cfg(windows)]
    view::set_api(api);
    #[cfg(not(windows))]
    let _ = api;
}

/// The editor table to give the host for the plugin's editor `inner`: with
/// `separate`, one that opens it in a separate process (Windows).
pub(crate) fn editor(inner: &'static crate::abi::ViewApi, separate: bool) -> *const std::ffi::c_void {
    #[cfg(windows)]
    if separate {
        return view::proxy(inner) as *const _ as *const std::ffi::c_void;
    }
    let _ = separate;
    inner as *const _ as *const std::ffi::c_void
}

/// How a plugin is isolated from the host (`[isolation] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolation {
    /// In-process; plugin panics are caught (level 1).
    Guarded,
    /// In a child process (level 3).
    Process,
}

/// How isolated plugins use processes (`[isolation] share` and `pipeline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessOptions {
    /// One process per instance; otherwise one per plugin type (the default),
    /// shared by its instances in the same host.
    pub per_instance: bool,
    /// Blocks run one block late, in parallel with the host, and the delay is
    /// reported as latency. Otherwise (the default) the host waits for each
    /// block.
    pub pipeline: bool,
}

impl ProcessOptions {
    pub const DEFAULT: Self = Self { per_instance: false, pipeline: false };
}

/// Bits of `ferritron_plugin_info.isolation` above the mode (low byte).
pub const PER_INSTANCE: u32 = 1 << 8;
pub const PIPELINE: u32 = 1 << 9;

fn wanted(declared: u32) -> bool {
    match std::env::var("FERRITRON_ISOLATION").as_deref() {
        Ok("process") => true,
        Ok("guarded") => false,
        _ => declared & 0xff == Isolation::Process as u32,
    }
}

/// Returns a client table that runs the plugin in a child process when
/// isolation is wanted, otherwise `None`.
pub(crate) fn install(api: &'static PluginApi) -> Option<&'static PluginApi> {
    if !wanted(unsafe { (*api.info).isolation }) {
        return None;
    }
    #[cfg(any(windows, unix))]
    return client::install(api);
    #[cfg(not(any(windows, unix)))]
    None
}

/// Body of the plugin library's `ferritron_host_main` export, run in the
/// child process by `rundll32.exe` (Windows) or the `ferritron-host` helper
/// (macOS, Linux).
///
/// # Safety
/// `command_line` must be null or a null-terminated string.
pub unsafe fn host_main(api: &'static PluginApi, command_line: *const std::ffi::c_char) {
    #[cfg(windows)]
    unsafe {
        match view::editor_command(command_line) {
            Some(args) => view::run_child(api, &args),
            None => server::run(api, command_line),
        }
    };
    #[cfg(unix)]
    unsafe {
        server::run(api, command_line)
    };
    #[cfg(not(any(windows, unix)))]
    let _ = (api, command_line);
}
