//! Named shared memory, the ground of resource sharing and DirectLink: the
//! same name maps the same memory in every plugin library and every process
//! of the user's session. The operating system owns it, so nothing leaks
//! when a plugin crashes.

use std::io;

/// A mapped region. Every mapping of a name starts with a [`Header`].
pub(crate) struct Mapping {
    ptr: *mut u8,
    /// Mapped bytes, header included (Unix unmaps by length).
    #[cfg_attr(windows, allow(dead_code))]
    len: usize,
    #[cfg(windows)]
    handle: *mut std::ffi::c_void,
    #[cfg(unix)]
    name: std::ffi::CString,
}

// SAFETY: the memory is shared on purpose; users access it through atomics.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

/// The start of every region: counts the processes that map it, so the last
/// one can remove the name where the OS keeps it (Unix).
#[repr(C)]
pub(crate) struct Header {
    pub users: std::sync::atomic::AtomicU32,
    _reserved: [u32; 15],
}

pub(crate) const HEADER: usize = size_of::<Header>();

/// A stable name for `key`, valid for every OS.
pub(crate) fn name_for(kind: &str, key: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in key.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("ferritron-{kind}-{hash:016x}")
}

impl Mapping {
    /// Maps `name` with `len` usable bytes after the header, creating it
    /// zero-filled if it does not exist yet.
    pub fn open(name: &str, len: usize) -> io::Result<Mapping> {
        let total = HEADER + len;
        let mapping = sys::open(name, total)?;
        mapping.header().users.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(mapping)
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.ptr as *const Header) }
    }

    /// The usable bytes, after the header.
    pub fn data(&self) -> *mut u8 {
        unsafe { self.ptr.add(HEADER) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let last = self.header().users.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1;
        sys::close(self, last);
    }
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(file: *mut c_void, attrs: *mut c_void, protect: u32, high: u32, low: u32, name: *const u16) -> *mut c_void;
        fn MapViewOfFile(mapping: *mut c_void, access: u32, high: u32, low: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(address: *const c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_ALL_ACCESS: u32 = 0xf001f;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    pub fn open(name: &str, total: usize) -> io::Result<Mapping> {
        // Pagefile-backed and named in this session: other processes find it by name.
        let wide: Vec<u16> = format!("Local\\{name}").encode_utf16().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null_mut(), PAGE_READWRITE, (total >> 32) as u32, total as u32, wide.as_ptr())
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, total) } as *mut u8;
        if ptr.is_null() {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(e);
        }
        Ok(Mapping { ptr, len: total, handle })
    }

    pub fn close(m: &mut Mapping, _last: bool) {
        // Windows removes the region with its last handle.
        unsafe {
            UnmapViewOfFile(m.ptr as *const c_void);
            CloseHandle(m.handle);
        }
    }
}

#[cfg(unix)]
mod sys {
    use super::*;
    use std::ffi::{CString, c_char, c_int, c_void};

    #[cfg_attr(target_os = "linux", link(name = "rt"))]
    unsafe extern "C" {
        fn shm_open(name: *const c_char, flags: c_int, mode: u32) -> c_int;
        fn shm_unlink(name: *const c_char) -> c_int;
        fn ftruncate(fd: c_int, len: i64) -> c_int;
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[link_name = "close"]
        fn close_fd(fd: c_int) -> c_int;
    }

    const O_RDWR: c_int = 2;
    #[cfg(target_os = "linux")]
    const O_CREAT: c_int = 0o100;
    #[cfg(target_os = "linux")]
    const O_EXCL: c_int = 0o200;
    #[cfg(not(target_os = "linux"))]
    const O_CREAT: c_int = 0x200;
    #[cfg(not(target_os = "linux"))]
    const O_EXCL: c_int = 0x800;
    const PROT_READ_WRITE: c_int = 1 | 2;
    const MAP_SHARED: c_int = 1;

    pub fn open(name: &str, total: usize) -> io::Result<Mapping> {
        let cname = CString::new(format!("/{name}")).map_err(|_| io::Error::other("bad name"))?;
        let mut created = true;
        let mut fd = unsafe { shm_open(cname.as_ptr(), O_RDWR | O_CREAT | O_EXCL, 0o600) };
        // The creator sizes the region (zero-filled); others map it as it is.
        if fd < 0 {
            created = false;
            fd = unsafe { shm_open(cname.as_ptr(), O_RDWR, 0o600) };
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if created && unsafe { ftruncate(fd, total as i64) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { close_fd(fd) };
            return Err(e);
        }
        let ptr = unsafe { mmap(std::ptr::null_mut(), total, PROT_READ_WRITE, MAP_SHARED, fd, 0) };
        unsafe { close_fd(fd) };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: ptr as *mut u8, len: total, name: cname })
    }

    pub fn close(m: &mut Mapping, last: bool) {
        unsafe { munmap(m.ptr as *mut c_void, m.len) };
        if last {
            // The OS keeps named regions until removed: the last user removes it.
            unsafe { shm_unlink(m.name.as_ptr()) };
        }
    }
}
