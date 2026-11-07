//! Companion libraries: plugins whose code lives in a second library next to
//! the plugin binary (design doc 7, language hosts).
//!
//! Some languages build their own shared library, with their own runtime
//! inside: C# with NativeAOT today, and later Python (a library that embeds
//! the interpreter) and others. Such a library exports
//! `ferritron_plugin_entry` like any Ferritron plugin. The plugin binary is
//! then a small Rust crate with the format adapters whose entry point loads
//! the companion from its own folder and hands its table on:
//!
//! ```ignore
//! ferritron_core::companion_plugin!("Gain.managed.dll");
//! ```
//!
//! `cargo ferritron bundle` copies companions listed in the project file
//! (`[companion] files`) next to the binary. The companion is loaded once,
//! by full path, and stays loaded: runtimes such as .NET's cannot unload.
//! Its `process` runs with denormals flushed, as Rust plugins do (some
//! languages, C# among them, cannot set the FPU flags themselves).

use crate::abi::{ABI_VERSION, Event, Executor, PluginApi};
use std::ffi::c_void;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Loads `file_name` from the folder of the plugin binary (once) and returns
/// its plugin table, or None (and a line in `%TEMP%/ferritron-companion.log`).
pub fn load(file_name: &str) -> Option<&'static PluginApi> {
    static LOADED: OnceLock<Option<PluginApi>> = OnceLock::new();
    LOADED
        .get_or_init(|| match try_load(file_name) {
            Ok(api) => {
                let _ = ORIGINAL.set(api as *const PluginApi as usize);
                // The companion's table, with `process` behind a denormal guard.
                let mut wrapped = *api;
                wrapped.process = guarded_process;
                Some(wrapped)
            }
            Err(message) => {
                log(&message);
                None
            }
        })
        .as_ref()
}

/// The companion's own table (for `guarded_process`).
static ORIGINAL: OnceLock<usize> = OnceLock::new();

unsafe extern "C" fn guarded_process(
    instance: *mut c_void,
    channels: *const *mut f32,
    num_channels: u32,
    frames: u32,
    events: *const Event,
    num_events: u32,
    executor: *const Executor,
) -> bool {
    let Some(&api) = ORIGINAL.get() else { return false };
    // SAFETY: the companion is never unloaded.
    let api = unsafe { &*(api as *const PluginApi) };
    let _denormals = crate::abi::DenormalGuard::new();
    unsafe { (api.process)(instance, channels, num_channels, frames, events, num_events, executor) }
}

fn try_load(file_name: &str) -> Result<&'static PluginApi, String> {
    let path = sys::module_dir().ok_or("cannot find the plugin binary's folder")?.join(file_name);
    let handle = sys::open(&path)?;
    let entry = sys::symbol(handle, c"ferritron_plugin_entry");
    if entry.is_null() {
        return Err(format!("{} does not export ferritron_plugin_entry", path.display()));
    }
    let entry: unsafe extern "C" fn() -> *const PluginApi = unsafe { std::mem::transmute(entry) };
    let api = unsafe { entry().as_ref() }.ok_or_else(|| format!("{}: ferritron_plugin_entry returned null", path.display()))?;
    if api.abi_version != ABI_VERSION || (api.struct_size as usize) < size_of::<PluginApi>() {
        return Err(format!("{} was built for another Ferritron ABI version", path.display()));
    }
    Ok(api)
}

fn log(message: &str) {
    let path = std::env::temp_dir().join("ferritron-companion.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

/// Defines `ferritron_plugin_entry` as the plugin in companion `file_name`
/// (a file next to the plugin binary).
#[macro_export]
macro_rules! companion_plugin {
    ($file_name:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn ferritron_plugin_entry() -> *const $crate::abi::PluginApi {
            $crate::companion::load($file_name).map_or(::core::ptr::null(), |api| api as *const _)
        }
    };
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, buffer: *mut u16, size: u32) -> u32;
    }

    pub fn module_dir() -> Option<PathBuf> {
        let mut module = std::ptr::null_mut();
        // FROM_ADDRESS | UNCHANGED_REFCOUNT: the module containing this function.
        if unsafe { GetModuleHandleExW(0x4 | 0x2, module_dir as *const u16, &mut module) } == 0 {
            return None;
        }
        let mut buf = vec![0u16; 32_768];
        let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let path = PathBuf::from(std::ffi::OsString::from_wide(buf.get(..len)?));
        path.parent().map(PathBuf::from)
    }

    pub fn open(path: &std::path::Path) -> Result<*mut c_void, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            return Err(format!("cannot load {}: {}", path.display(), std::io::Error::last_os_error()));
        }
        Ok(handle)
    }

    pub fn symbol(handle: *mut c_void, name: &std::ffi::CStr) -> *mut c_void {
        unsafe { GetProcAddress(handle, name.as_ptr().cast()) }
    }
}

#[cfg(unix)]
mod sys {
    use super::*;
    use std::ffi::{CStr, CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct DlInfo {
        fname: *const c_char,
        fbase: *mut c_void,
        sname: *const c_char,
        saddr: *mut c_void,
    }

    unsafe extern "C" {
        fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
        fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
    }

    const RTLD_NOW: c_int = 2;

    pub fn module_dir() -> Option<PathBuf> {
        let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
        if unsafe { dladdr(module_dir as *const c_void, &mut info) } == 0 || info.fname.is_null() {
            return None;
        }
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(info.fname) }.to_bytes()));
        path.parent().map(PathBuf::from)
    }

    pub fn open(path: &std::path::Path) -> Result<*mut c_void, String> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| "path contains NUL".to_string())?;
        let handle = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            let error = unsafe { dlerror() };
            let error = if error.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy() };
            return Err(format!("cannot load {}: {error}", path.display()));
        }
        Ok(handle)
    }

    pub fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
        unsafe { dlsym(handle, name.as_ptr()) }
    }
}
