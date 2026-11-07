//! Loading libraries and finding the one this code is in: the few OS calls
//! hot reload, isolation and the shared thread pool need.

pub use imp::*;

#[cfg(windows)]
mod imp {
    use std::ffi::{CStr, OsStr, OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, buffer: *mut u16, size: u32) -> u32;
    }

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x2;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    /// Path of the module (DLL) that contains this function.
    pub fn current_module_path() -> Option<PathBuf> {
        let mut module = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        let address = current_module_path as *const u16;
        if unsafe { GetModuleHandleExW(flags, address, &mut module) } == 0 {
            return None;
        }
        let mut buf = vec![0u16; 32_768];
        let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (len > 0 && len < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
    }

    /// A loaded private copy of a library. The copy is deleted when dropped.
    pub struct Library {
        handle: *mut c_void,
        copy: PathBuf,
    }

    // SAFETY: module handles may be used from any thread.
    unsafe impl Send for Library {}
    unsafe impl Sync for Library {}

    impl Library {
        /// Copies `path` to `copy` and loads the copy, so the original stays
        /// writable for the next build.
        pub fn load_copy(path: &Path, copy: PathBuf) -> Result<Self, String> {
            std::fs::copy(path, &copy).map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
            let handle = unsafe { LoadLibraryW(wide(copy.as_os_str()).as_ptr()) };
            if handle.is_null() {
                let _ = std::fs::remove_file(&copy);
                return Err(format!("cannot load {}: {}", copy.display(), std::io::Error::last_os_error()));
            }
            Ok(Self { handle, copy })
        }

        pub fn symbol(&self, name: &CStr) -> *mut c_void {
            unsafe { GetProcAddress(self.handle, name.as_ptr().cast()) }
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            unsafe { FreeLibrary(self.handle) };
            let _ = std::fs::remove_file(&self.copy);
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, CString, OsStr, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    /// Path of the library (or executable) that contains this function.
    pub fn current_module_path() -> Option<PathBuf> {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::dladdr(current_module_path as *const c_void, &mut info) } == 0 || info.dli_fname.is_null() {
            return None;
        }
        let path = PathBuf::from(OsStr::from_bytes(unsafe { CStr::from_ptr(info.dli_fname) }.to_bytes()));
        Some(std::fs::canonicalize(&path).unwrap_or(path))
    }

    /// Keeps the library that contains this code loaded until the process
    /// exits, even when the host unloads it.
    pub fn pin_current_module() -> bool {
        let Some(path) = current_module_path() else { return false };
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
        // One more reference, never released.
        !unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_NODELETE) }.is_null()
    }

    /// A loaded private copy of a library. The copy is deleted when dropped.
    pub struct Library {
        handle: *mut c_void,
        copy: PathBuf,
    }

    // SAFETY: library handles may be used from any thread.
    unsafe impl Send for Library {}
    unsafe impl Sync for Library {}

    impl Library {
        /// Copies `path` to `copy` and loads the copy, so the original stays
        /// writable for the next build (and each build is a new file to the
        /// dynamic loader).
        pub fn load_copy(path: &Path, copy: PathBuf) -> Result<Self, String> {
            std::fs::copy(path, &copy).map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
            let c = CString::new(copy.as_os_str().as_bytes()).map_err(|_| "bad library path".to_string())?;
            let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                let error = unsafe { libc::dlerror() };
                let error = if error.is_null() { String::new() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned() };
                let _ = std::fs::remove_file(&copy);
                return Err(format!("cannot load {}: {error}", copy.display()));
            }
            Ok(Self { handle, copy })
        }

        pub fn symbol(&self, name: &CStr) -> *mut c_void {
            unsafe { libc::dlsym(self.handle, name.as_ptr()) }
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            // macOS keeps libraries with thread-local storage mapped; the
            // copy is removed either way.
            unsafe { libc::dlclose(self.handle) };
            let _ = std::fs::remove_file(&self.copy);
        }
    }
}
