//! The Win32 calls isolation needs (`os` and editors in a separate process, `view`).

use std::ffi::c_void;
use std::path::Path;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateFileMappingW(file: *mut c_void, attrs: *mut c_void, protect: u32, high: u32, low: u32, name: *const u16) -> *mut c_void;
    pub fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    pub fn MapViewOfFile(mapping: *mut c_void, access: u32, high: u32, low: u32, bytes: usize) -> *mut c_void;
    pub fn UnmapViewOfFile(address: *const c_void) -> i32;
    pub fn CreateEventW(attrs: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> *mut c_void;
    pub fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    pub fn SetEvent(event: *mut c_void) -> i32;
    pub fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    pub fn WaitForMultipleObjects(count: u32, handles: *const *mut c_void, wait_all: i32, ms: u32) -> u32;
    pub fn CloseHandle(handle: *mut c_void) -> i32;
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    pub fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    pub fn GetCurrentProcessId() -> u32;
    pub fn CreateProcessW(
        app: *const u16,
        cmd: *mut u16,
        pattrs: *mut c_void,
        tattrs: *mut c_void,
        inherit: i32,
        flags: u32,
        env: *mut c_void,
        dir: *const u16,
        startup: *mut StartupInfo,
        info: *mut ProcessInformation,
    ) -> i32;
}

pub const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
pub const PAGE_READWRITE: u32 = 0x04;
pub const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;
pub const EVENT_ALL_ACCESS: u32 = 0x1F0003;
pub const SYNCHRONIZE: u32 = 0x0010_0000;
pub const WAIT_OBJECT_0: u32 = 0;
pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[repr(C)]
pub struct StartupInfo {
    pub cb: u32,
    pub reserved: *mut u16,
    pub desktop: *mut u16,
    pub title: *mut u16,
    pub x: u32,
    pub y: u32,
    pub x_size: u32,
    pub y_size: u32,
    pub x_count_chars: u32,
    pub y_count_chars: u32,
    pub fill_attribute: u32,
    pub flags: u32,
    pub show_window: u16,
    pub reserved2_len: u16,
    pub reserved2: *mut u8,
    pub std_input: *mut c_void,
    pub std_output: *mut c_void,
    pub std_error: *mut c_void,
}

#[repr(C)]
pub struct ProcessInformation {
    pub process: *mut c_void,
    pub thread: *mut c_void,
    pub process_id: u32,
    pub thread_id: u32,
}

/// Runs `library`'s `ferritron_host_main` export with `args` in a new
/// process, through `rundll32.exe` (no executable to ship). Returns the
/// process handle (the caller closes it).
pub fn spawn_rundll(library: &Path, args: &str) -> std::io::Result<*mut c_void> {
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let rundll = format!("{system}\\System32\\rundll32.exe");
    let mut command = wide(&format!("\"{rundll}\" \"{}\",ferritron_host_main {args}", library.display()));
    let mut startup: StartupInfo = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<StartupInfo>() as u32;
    let mut info: ProcessInformation = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            wide(&rundll).as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut startup,
            &mut info,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { CloseHandle(info.thread) };
    Ok(info.process)
}
