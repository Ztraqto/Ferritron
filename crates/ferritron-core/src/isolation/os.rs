//! The operating system calls isolation needs, behind one small interface:
//! named shared memory, named auto-reset events ("signals") and processes.
//!
//! - Windows: file mappings, events and process handles; the child is the
//!   plugin library run by `rundll32.exe`.
//! - macOS / Linux: POSIX shared memory, named FIFOs (a signal is a byte in
//!   the pipe; waits use `poll`) and a tiny helper executable that loads the
//!   plugin library (`ferritron-host`, placed in the bundle by
//!   `cargo ferritron bundle`).

#[cfg(unix)]
use std::path::Path;

pub use imp::{Memory, Process, Signal};

/// A short name for `key` that every OS accepts (macOS limits shared memory
/// names to 31 bytes).
pub fn short_name(key: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in key.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fr-{hash:016x}")
}

/// Where the helper executable that runs the child is, next to `library`
/// (Unix). A macOS bundle keeps it in `Contents/Helpers`; elsewhere it sits
/// beside the library. `FERRITRON_HOST_HELPER` overrides both.
#[cfg(unix)]
pub fn helper_for(library: &Path) -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("FERRITRON_HOST_HELPER") {
        return Some(path.into());
    }
    let dir = library.parent()?;
    let candidates = [dir.join("../Helpers/ferritron-host"), dir.join("ferritron-host")];
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(windows)]
mod imp {
    use super::short_name;
    use crate::isolation::win32::*;
    use std::ffi::c_void;
    use std::path::Path;

    fn global(name: &str) -> Vec<u16> {
        wide(&format!("Local\\{}", short_name(name)))
    }

    fn ms(timeout: Option<u32>) -> u32 {
        timeout.unwrap_or(INFINITE)
    }

    /// A named shared memory region, zero-filled when created.
    pub struct Memory {
        handle: *mut c_void,
        ptr: *mut u8,
    }

    unsafe impl Send for Memory {}
    unsafe impl Sync for Memory {}

    impl Memory {
        pub fn create(name: &str, len: usize) -> Option<Self> {
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null_mut(),
                    PAGE_READWRITE,
                    (len as u64 >> 32) as u32,
                    len as u32,
                    global(name).as_ptr(),
                )
            };
            Self::map(handle, len)
        }

        pub fn open(name: &str, len: usize) -> Option<Self> {
            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, global(name).as_ptr()) };
            Self::map(handle, len)
        }

        fn map(handle: *mut c_void, len: usize) -> Option<Self> {
            if handle.is_null() {
                return None;
            }
            let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) } as *mut u8;
            if ptr.is_null() {
                unsafe { CloseHandle(handle) };
                return None;
            }
            Some(Self { handle, ptr })
        }

        pub fn ptr(&self) -> *mut u8 {
            self.ptr
        }
    }

    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.ptr as *const c_void);
                CloseHandle(self.handle);
            }
        }
    }

    /// An auto-reset event: `set` wakes one wait; sets while nobody waits
    /// are remembered once.
    pub struct Signal(*mut c_void);

    unsafe impl Send for Signal {}
    unsafe impl Sync for Signal {}

    impl Signal {
        pub fn create(name: &str) -> Option<Self> {
            let h = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, global(name).as_ptr()) };
            (!h.is_null()).then_some(Self(h))
        }

        /// A signal used inside this process only.
        pub fn anonymous() -> Option<Self> {
            let h = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
            (!h.is_null()).then_some(Self(h))
        }

        pub fn open(name: &str) -> Option<Self> {
            let h = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, global(name).as_ptr()) };
            (!h.is_null()).then_some(Self(h))
        }

        pub fn set(&self) {
            unsafe { SetEvent(self.0) };
        }

        /// Waits until the signal is set (true), `timeout` ms pass (`None`:
        /// no limit) or `process` ends.
        pub fn wait(&self, timeout: Option<u32>, process: Option<&Process>) -> bool {
            unsafe {
                match process {
                    Some(p) => WaitForMultipleObjects(2, [self.0, p.0].as_ptr(), 0, ms(timeout)) == WAIT_OBJECT_0,
                    None => WaitForSingleObject(self.0, ms(timeout)) == WAIT_OBJECT_0,
                }
            }
        }
    }

    impl Drop for Signal {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A process: a child this process started, or another one to watch.
    pub struct Process(*mut c_void);

    unsafe impl Send for Process {}
    unsafe impl Sync for Process {}

    impl Process {
        pub fn current_id() -> u32 {
            unsafe { GetCurrentProcessId() }
        }

        /// Runs `library`'s `ferritron_host_main` export with `args` in a new
        /// process, through `rundll32.exe` (no executable to ship).
        pub fn spawn(library: &Path, args: &str) -> Result<Self, String> {
            spawn_rundll(library, args).map(Self).map_err(|e| e.to_string())
        }

        pub fn open(pid: u32) -> Option<Self> {
            let h = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
            (!h.is_null()).then_some(Self(h))
        }

        /// Waits until the process ends (true), `timeout` ms pass or `wake` is set.
        pub fn wait_exit(&self, timeout: Option<u32>, wake: Option<&Signal>) -> bool {
            unsafe {
                match wake {
                    Some(w) => WaitForMultipleObjects(2, [self.0, w.0].as_ptr(), 0, ms(timeout)) == WAIT_OBJECT_0,
                    None => WaitForSingleObject(self.0, ms(timeout)) == WAIT_OBJECT_0,
                }
            }
        }

        pub fn kill(&self) {
            unsafe { TerminateProcess(self.0, 1) };
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::short_name;
    use std::ffi::CString;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Waits that also watch a process check it this often.
    const WATCH_MS: u32 = 100;

    fn shm_name(name: &str) -> CString {
        CString::new(format!("/{}", short_name(name))).unwrap()
    }

    fn fifo_path(name: &str) -> CString {
        let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        CString::new(format!("{}/{}", dir.trim_end_matches('/'), short_name(name))).unwrap()
    }

    /// A named shared memory region, zero-filled when created.
    pub struct Memory {
        ptr: *mut u8,
        len: usize,
        /// The creator removes the name when done.
        owned: Option<CString>,
    }

    unsafe impl Send for Memory {}
    unsafe impl Sync for Memory {}

    impl Memory {
        pub fn create(name: &str, len: usize) -> Option<Self> {
            let cname = shm_name(name);
            unsafe {
                libc::shm_unlink(cname.as_ptr()); // a region left by a crashed host
                let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint);
                if fd < 0 {
                    return None;
                }
                if libc::ftruncate(fd, len as libc::off_t) != 0 {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                    return None;
                }
                Self::map(fd, len, Some(cname))
            }
        }

        pub fn open(name: &str, len: usize) -> Option<Self> {
            let cname = shm_name(name);
            unsafe {
                let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600 as libc::c_uint);
                if fd < 0 {
                    return None;
                }
                Self::map(fd, len, None)
            }
        }

        unsafe fn map(fd: libc::c_int, len: usize, owned: Option<CString>) -> Option<Self> {
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
            unsafe { libc::close(fd) };
            if ptr == libc::MAP_FAILED {
                if let Some(name) = &owned {
                    unsafe { libc::shm_unlink(name.as_ptr()) };
                }
                return None;
            }
            Some(Self { ptr: ptr as *mut u8, len, owned })
        }

        pub fn ptr(&self) -> *mut u8 {
            self.ptr
        }
    }

    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
                if let Some(name) = &self.owned {
                    libc::shm_unlink(name.as_ptr());
                }
            }
        }
    }

    /// An auto-reset event over a named FIFO: `set` writes a byte, a wait
    /// takes every byte there is, so sets while nobody waits count once.
    pub struct Signal {
        read: libc::c_int,
        write: libc::c_int,
        /// The creator removes the FIFO when done.
        owned: Option<CString>,
    }

    unsafe impl Send for Signal {}
    unsafe impl Sync for Signal {}

    impl Signal {
        pub fn create(name: &str) -> Option<Self> {
            let path = fifo_path(name);
            unsafe {
                libc::unlink(path.as_ptr());
                if libc::mkfifo(path.as_ptr(), 0o600 as libc::mode_t) != 0 {
                    return None;
                }
            }
            let mut signal = Self::open_path(&path)?;
            signal.owned = Some(path);
            Some(signal)
        }

        /// A signal used inside this process only.
        pub fn anonymous() -> Option<Self> {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return None;
            }
            for fd in fds {
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
            Some(Self { read: fds[0], write: fds[1], owned: None })
        }

        pub fn open(name: &str) -> Option<Self> {
            Self::open_path(&fifo_path(name))
        }

        fn open_path(path: &CString) -> Option<Self> {
            // Read and write on one descriptor: opening never blocks and the
            // pipe never reports end-of-file while this side is open.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            (fd >= 0).then_some(Self { read: fd, write: fd, owned: None })
        }

        pub fn set(&self) {
            // A full pipe is signaled already.
            unsafe { libc::write(self.write, [1u8].as_ptr() as *const libc::c_void, 1) };
        }

        /// Takes every pending byte; whether there was any.
        fn drain(&self) -> bool {
            let mut buf = [0u8; 64];
            let mut any = false;
            while unsafe { libc::read(self.read, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {
                any = true;
            }
            any
        }

        fn poll(&self, ms: u32) -> bool {
            let mut fd = libc::pollfd { fd: self.read, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut fd, 1, ms as libc::c_int) };
            ready > 0 && self.drain()
        }

        /// Waits until the signal is set (true), `timeout` ms pass (`None`:
        /// no limit) or `process` ends.
        pub fn wait(&self, timeout: Option<u32>, process: Option<&Process>) -> bool {
            let deadline = timeout.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
            loop {
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u32);
                let slice = match (left, process) {
                    (Some(l), Some(_)) => l.min(WATCH_MS),
                    (Some(l), None) => l,
                    (None, Some(_)) => WATCH_MS,
                    (None, None) => 60_000,
                };
                if self.poll(slice) {
                    return true;
                }
                if process.is_some_and(|p| !p.alive()) || left.is_some_and(|l| l <= slice) {
                    return false;
                }
            }
        }
    }

    impl Drop for Signal {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.read);
                if self.write != self.read {
                    libc::close(self.write);
                }
                if let Some(path) = &self.owned {
                    libc::unlink(path.as_ptr());
                }
            }
        }
    }

    /// A process: a child this process started, or another one to watch.
    pub struct Process {
        pid: libc::pid_t,
        child: Option<Mutex<std::process::Child>>,
    }

    impl Process {
        pub fn current_id() -> u32 {
            std::process::id()
        }

        /// Runs `library`'s `ferritron_host_main` export with `args` in a new
        /// process, through the `ferritron-host` helper next to the library.
        pub fn spawn(library: &Path, args: &str) -> Result<Self, String> {
            let helper = super::helper_for(library).ok_or_else(|| {
                format!("no ferritron-host helper beside {} (bundle the plugin with `cargo ferritron bundle`)", library.display())
            })?;
            let child = std::process::Command::new(&helper)
                .arg(library)
                .args(args.split_whitespace())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("cannot run {}: {e}", helper.display()))?;
            Ok(Self { pid: child.id() as libc::pid_t, child: Some(Mutex::new(child)) })
        }

        pub fn open(pid: u32) -> Option<Self> {
            let process = Self { pid: pid as libc::pid_t, child: None };
            process.alive().then_some(process)
        }

        fn alive(&self) -> bool {
            match &self.child {
                // Reaps the child once it ended.
                Some(child) => matches!(child.lock().unwrap_or_else(std::sync::PoisonError::into_inner).try_wait(), Ok(None)),
                None => unsafe { libc::kill(self.pid, 0) == 0 || *errno() == libc::EPERM },
            }
        }

        /// Waits until the process ends (true), `timeout` ms pass or `wake` is set.
        pub fn wait_exit(&self, timeout: Option<u32>, wake: Option<&Signal>) -> bool {
            let deadline = timeout.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
            loop {
                if !self.alive() {
                    return true;
                }
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u32);
                if left == Some(0) {
                    return false;
                }
                let slice = left.unwrap_or(WATCH_MS).min(WATCH_MS);
                match wake {
                    Some(w) if w.poll(slice) => return false,
                    Some(_) => {}
                    None => std::thread::sleep(Duration::from_millis(slice as u64)),
                }
            }
        }

        pub fn kill(&self) {
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }

    #[cfg(target_vendor = "apple")]
    unsafe fn errno() -> *mut libc::c_int {
        unsafe { libc::__error() }
    }

    #[cfg(not(target_vendor = "apple"))]
    unsafe fn errno() -> *mut libc::c_int {
        unsafe { libc::__errno_location() }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // A child that ended is reaped; one still running is left alone.
            let _ = self.alive();
        }
    }
}
