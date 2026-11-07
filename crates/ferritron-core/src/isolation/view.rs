//! Editors in a separate process (design doc 6.5): the plugin's editor runs
//! in a child process (the same plugin library under `rundll32.exe`, like
//! isolation level 3) as a child window of the host's window.
//!
//! - The host side is a `ferritron.view.v1` table whose views are proxies.
//!   A timer on the host's UI thread mirrors parameter values into shared
//!   memory and passes the editor's edits (begin / perform / end) to the host.
//! - The child opens the real editor with a view host that reads the mirror
//!   and queues edits, and follows size changes. It ends with the host.
//! - If the editor process dies, the host keeps running and a new editor
//!   process takes its place.
//!
//! Used for plugins in level 3 isolation, and for `[view] process = true`
//! (frameworks with their own event loop, crash-proof editors).

use super::shm::MAX_PARAMS;
use super::win32::*;
use crate::abi::{EXT_VIEW, ParamInfoC, PluginApi, ViewApi, ViewHost};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};
use std::time::{Duration, Instant};

const MAX_EDITS: usize = 1024;
const EDIT_BEGIN: u32 = 1;
const EDIT_PERFORM: u32 = 2;
const EDIT_END: u32 = 3;
/// Host-side refresh: edits to the host, values to the editor.
const TICK_MS: u32 = 16;
/// How long opening may take (WebView2 starts in a few hundred ms).
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

#[repr(C)]
struct Edit {
    kind: AtomicU32,
    id: AtomicU32,
    value: AtomicU64,
}

/// Shared memory of one open editor.
#[repr(C)]
struct GuiShared {
    /// 1: the editor is open; 2: it could not be opened.
    ready: AtomicU32,
    quit: AtomicU32,
    parent: AtomicU64,
    /// Physical size and scale; `size_generation` changes with each update.
    size_generation: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    scale: AtomicU64,
    /// Normalized values by parameter index, written by the host side.
    params: [AtomicU64; MAX_PARAMS],
    /// Edits from the editor: the child writes at `head`, the host reads at `tail`.
    head: AtomicU32,
    tail: AtomicU32,
    edits: [Edit; MAX_EDITS],
}

fn gui_names(id: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let w = |suffix: &str| wide(&format!("Local\\ferritron-view-{id}-{suffix}"));
    (w("memory"), w("command"), w("ready"))
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetTimer(hwnd: *mut c_void, id: usize, ms: u32, proc_: Option<unsafe extern "system" fn(*mut c_void, u32, usize, u32)>) -> usize;
    fn KillTimer(hwnd: *mut c_void, id: usize) -> i32;
    fn MsgWaitForMultipleObjects(count: u32, handles: *const *mut c_void, wait_all: i32, ms: u32, wake_mask: u32) -> u32;
    fn PeekMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn GetWindowDpiAwarenessContext(hwnd: *mut c_void) -> *mut c_void;
    fn SetThreadDpiAwarenessContext(context: *mut c_void) -> *mut c_void;
    fn IsWindow(hwnd: *mut c_void) -> i32;
}

#[repr(C)]
struct Msg {
    hwnd: *mut c_void,
    message: u32,
    w: usize,
    l: isize,
    time: u32,
    pt: [i32; 2],
    private: u32,
}

const QS_SENDMESSAGE: u32 = 0x0040;
const QS_ALLINPUT: u32 = 0x04FF;
const PM_REMOVE: u32 = 0x0001;
const PM_NOREMOVE: u32 = 0x0000;
const PM_QS_SENDMESSAGE: u32 = QS_SENDMESSAGE << 16;
const WAIT_TIMEOUT: u32 = 0x102;

/// Waits for `handle` while answering messages other threads (the editor
/// process creating its window in ours) send to this thread.
fn wait_answering(handle: *mut c_void, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let left = timeout.saturating_sub(start.elapsed());
        let ms = left.as_millis().min(u32::MAX as u128 - 1) as u32;
        match unsafe { MsgWaitForMultipleObjects(1, &handle, 0, ms, QS_SENDMESSAGE) } {
            WAIT_OBJECT_0 => return true,
            WAIT_TIMEOUT => return false,
            _ => {
                let mut msg: Msg = unsafe { std::mem::zeroed() };
                // Peeking delivers pending sent messages without taking posted ones.
                unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE) };
                if start.elapsed() >= timeout {
                    return false;
                }
            }
        }
    }
}

// --- Host side -----------------------------------------------------------

static INNER: OnceLock<&'static ViewApi> = OnceLock::new();
static PROXY: OnceLock<ViewApi> = OnceLock::new();
static NEXT_VIEW: AtomicUsize = AtomicUsize::new(0);
/// Set in an editor process: the plugin then hands out its real editor.
static IN_EDITOR_PROCESS: AtomicBool = AtomicBool::new(false);

/// The editor table to give the host: a proxy that opens `inner` in a
/// separate process, or `inner` itself inside that process.
pub(crate) fn proxy(inner: &'static ViewApi) -> &'static ViewApi {
    if IN_EDITOR_PROCESS.load(Relaxed) {
        return inner;
    }
    let _ = INNER.set(inner);
    PROXY.get_or_init(|| ViewApi {
        width: inner.width,
        height: inner.height,
        min_width: inner.min_width,
        min_height: inner.min_height,
        max_width: inner.max_width,
        max_height: inner.max_height,
        open,
        close,
        set_size,
    })
}

struct Proxy {
    id: String,
    shared: *mut GuiShared,
    mapping: *mut c_void,
    command: *mut c_void,
    ready: *mut c_void,
    child: *mut c_void,
    host: ViewHost,
    ids: Vec<u32>,
    timer: usize,
    /// When the editor process last started, to slow down restarts.
    started: Instant,
}

thread_local! {
    static PROXIES: RefCell<HashMap<usize, *mut Proxy>> = RefCell::new(HashMap::new());
}

impl Proxy {
    fn shared(&self) -> &GuiShared {
        unsafe { &*self.shared }
    }

    fn mirror(&self) {
        for (i, &id) in self.ids.iter().enumerate() {
            let value = unsafe { (self.host.get_param)(self.host.ctx, id) };
            self.shared().params[i].store(value.to_bits(), Relaxed);
        }
    }

    /// Starts the editor process and waits until its editor is open.
    fn spawn(&mut self) -> bool {
        let shared = self.shared();
        shared.ready.store(0, SeqCst);
        shared.quit.store(0, SeqCst);
        self.started = Instant::now();
        let Some(library) = super::client::current_module_path() else { return false };
        let pid = unsafe { GetCurrentProcessId() };
        let process = match spawn_rundll(&library, &format!("view {} {pid}", self.id)) {
            Ok(process) => process,
            Err(e) => {
                super::client::log(&format!("view {}: cannot start the editor process: {e}", self.id));
                return false;
            }
        };
        let opened = wait_answering(self.ready, OPEN_TIMEOUT) && self.shared().ready.load(SeqCst) == 1;
        if !opened {
            super::client::log(&format!("view {}: the editor process did not open the editor", self.id));
            unsafe {
                TerminateProcess(process, 1);
                CloseHandle(process);
            }
            return false;
        }
        self.child = process;
        true
    }

    fn stop_child(&mut self) {
        if self.child.is_null() {
            return;
        }
        self.shared().quit.store(1, SeqCst);
        unsafe { SetEvent(self.command) };
        if !wait_answering(self.child, Duration::from_secs(2)) {
            unsafe { TerminateProcess(self.child, 1) };
        }
        unsafe { CloseHandle(self.child) };
        self.child = std::ptr::null_mut();
    }

    /// Edits to the host, then current values to the editor.
    fn tick(&mut self) {
        let shared = unsafe { &*self.shared };
        let head = shared.head.load(Acquire);
        let mut tail = shared.tail.load(Relaxed);
        while tail != head {
            let edit = &shared.edits[tail as usize % MAX_EDITS];
            let (kind, id, value) = (edit.kind.load(Relaxed), edit.id.load(Relaxed), f64::from_bits(edit.value.load(Relaxed)));
            unsafe {
                match kind {
                    EDIT_BEGIN => (self.host.begin_edit)(self.host.ctx, id),
                    EDIT_PERFORM => (self.host.perform_edit)(self.host.ctx, id, value),
                    EDIT_END => (self.host.end_edit)(self.host.ctx, id),
                    _ => {}
                }
            }
            tail = tail.wrapping_add(1);
        }
        shared.tail.store(tail, Release);
        self.mirror();
        // A crashed editor is replaced (at most once a second); the host never notices.
        if !self.child.is_null() && unsafe { WaitForSingleObject(self.child, 0) } == WAIT_OBJECT_0 {
            unsafe { CloseHandle(self.child) };
            self.child = std::ptr::null_mut();
            super::client::log(&format!("view {}: the editor process ended unexpectedly; restarting", self.id));
        }
        let parent = shared.parent.load(Relaxed) as usize as *mut c_void;
        if self.child.is_null() && self.started.elapsed() > Duration::from_secs(1) && unsafe { IsWindow(parent) } != 0 && self.spawn() {
            super::client::log(&format!("view {}: editor process restarted", self.id));
        }
    }
}

unsafe extern "system" fn on_timer(_hwnd: *mut c_void, _msg: u32, id: usize, _time: u32) {
    if let Some(proxy) = PROXIES.with(|p| p.borrow().get(&id).copied()) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { (*proxy).tick() }));
    }
}

fn parameter_ids(api: &PluginApi) -> Vec<u32> {
    let count = (unsafe { (api.param_count)() } as usize).min(MAX_PARAMS);
    (0..count as u32)
        .map(|i| {
            let mut info = ParamInfoC::empty();
            unsafe { (api.param_info)(i, &mut info) };
            info.id
        })
        .collect()
}

static API: OnceLock<&'static PluginApi> = OnceLock::new();

/// Remembers the plugin's table (parameter IDs for the mirror).
pub(crate) fn set_api(api: &'static PluginApi) {
    let _ = API.set(api);
}

unsafe extern "C" fn open(parent: *mut c_void, host: *const ViewHost, width: u32, height: u32, scale: f64) -> *mut c_void {
    let (Some(api), false, false) = (API.get(), parent.is_null(), host.is_null()) else { return std::ptr::null_mut() };
    let id = format!("{}-{}", unsafe { GetCurrentProcessId() }, NEXT_VIEW.fetch_add(1, SeqCst));
    let (memory, command, ready) = gui_names(&id);
    let size = size_of::<GuiShared>();
    unsafe {
        let mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null_mut(), PAGE_READWRITE, 0, size as u32, memory.as_ptr());
        let view = if mapping.is_null() { std::ptr::null_mut() } else { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        let command = CreateEventW(std::ptr::null_mut(), 0, 0, command.as_ptr());
        let ready = CreateEventW(std::ptr::null_mut(), 0, 0, ready.as_ptr());
        if view.is_null() || command.is_null() || ready.is_null() {
            for h in [mapping, command, ready] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
            return std::ptr::null_mut();
        }
        let shared = &*(view as *const GuiShared);
        shared.parent.store(parent as usize as u64, SeqCst);
        shared.width.store(width, SeqCst);
        shared.height.store(height, SeqCst);
        shared.scale.store(scale.to_bits(), SeqCst);
        let mut proxy = Box::new(Proxy {
            id,
            shared: view as *mut GuiShared,
            mapping,
            command,
            ready,
            child: std::ptr::null_mut(),
            host: *host,
            ids: parameter_ids(api),
            timer: 0,
            started: Instant::now(),
        });
        proxy.mirror();
        if !proxy.spawn() {
            drop_proxy(*proxy);
            return std::ptr::null_mut();
        }
        proxy.timer = SetTimer(std::ptr::null_mut(), 0, TICK_MS, Some(on_timer));
        let ptr: *mut Proxy = &mut *proxy;
        PROXIES.with(|p| p.borrow_mut().insert(proxy.timer, ptr));
        Box::into_raw(proxy).cast()
    }
}

unsafe extern "C" fn set_size(view: *mut c_void, width: u32, height: u32, scale: f64) {
    let Some(proxy) = (unsafe { (view as *mut Proxy).as_ref() }) else { return };
    let shared = proxy.shared();
    shared.width.store(width, SeqCst);
    shared.height.store(height, SeqCst);
    shared.scale.store(scale.to_bits(), SeqCst);
    shared.size_generation.fetch_add(1, SeqCst);
    unsafe { SetEvent(proxy.command) };
}

fn drop_proxy(mut proxy: Proxy) {
    proxy.stop_child();
    unsafe {
        UnmapViewOfFile(proxy.shared as *const c_void);
        for h in [proxy.mapping, proxy.command, proxy.ready] {
            CloseHandle(h);
        }
    }
}

unsafe extern "C" fn close(view: *mut c_void) {
    if view.is_null() {
        return;
    }
    let mut proxy = unsafe { Box::from_raw(view as *mut Proxy) };
    unsafe { KillTimer(std::ptr::null_mut(), proxy.timer) };
    PROXIES.with(|p| p.borrow_mut().remove(&proxy.timer));
    proxy.tick(); // the last edits (e.g. an end of gesture) still reach the host
    drop_proxy(*proxy);
}

// --- Editor process -------------------------------------------------------

struct ChildHost {
    shared: *const GuiShared,
    ids: Vec<u32>,
}

impl ChildHost {
    fn shared(&self) -> &GuiShared {
        unsafe { &*self.shared }
    }

    fn push(&self, kind: u32, id: u32, value: f64) {
        let shared = self.shared();
        let head = shared.head.load(Relaxed);
        if head.wrapping_sub(shared.tail.load(Acquire)) as usize >= MAX_EDITS {
            return; // the host stopped reading; drop rather than block the editor
        }
        let edit = &shared.edits[head as usize % MAX_EDITS];
        edit.kind.store(kind, Relaxed);
        edit.id.store(id, Relaxed);
        edit.value.store(value.to_bits(), Relaxed);
        shared.head.store(head.wrapping_add(1), Release);
    }
}

unsafe extern "C" fn child_get(ctx: *mut c_void, id: u32) -> f64 {
    let host = unsafe { &*(ctx as *const ChildHost) };
    host.ids.iter().position(|&i| i == id).map_or(0.0, |i| f64::from_bits(host.shared().params[i].load(Relaxed)))
}

unsafe extern "C" fn child_begin(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const ChildHost) }.push(EDIT_BEGIN, id, 0.0);
}

unsafe extern "C" fn child_perform(ctx: *mut c_void, id: u32, normalized: f64) {
    let host = unsafe { &*(ctx as *const ChildHost) };
    // Shown at once; the host's value replaces it at the next refresh.
    if let Some(i) = host.ids.iter().position(|&i| i == id) {
        host.shared().params[i].store(normalized.to_bits(), Relaxed);
    }
    host.push(EDIT_PERFORM, id, normalized);
}

unsafe extern "C" fn child_end(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const ChildHost) }.push(EDIT_END, id, 0.0);
}

/// Body of `ferritron_host_main view <id> <host pid>`: opens the editor in
/// the host's window and serves it until the host closes it.
pub(crate) unsafe fn run_child(api: &'static PluginApi, args: &str) {
    IN_EDITOR_PROCESS.store(true, SeqCst);
    let mut args = args.split_whitespace();
    let (Some(id), Some(host_pid)) = (args.next(), args.next().and_then(|p| p.parse::<u32>().ok())) else { return };
    let (memory, command, ready) = gui_names(id);
    unsafe {
        let mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, memory.as_ptr());
        let view = if mapping.is_null() { std::ptr::null_mut() } else { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<GuiShared>()) };
        let command = OpenEventW(EVENT_ALL_ACCESS, 0, command.as_ptr());
        let ready = OpenEventW(EVENT_ALL_ACCESS, 0, ready.as_ptr());
        let host_process = OpenProcess(SYNCHRONIZE, 0, host_pid);
        if view.is_null() || command.is_null() || ready.is_null() || host_process.is_null() {
            return;
        }
        let shared = &*(view as *const GuiShared);
        let inner = (api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi;
        let parent = shared.parent.load(SeqCst) as usize as *mut c_void;
        // Same DPI handling as the host's window, so sizes mean the same.
        let awareness = GetWindowDpiAwarenessContext(parent);
        if !awareness.is_null() {
            SetThreadDpiAwarenessContext(awareness);
        }
        let host = Box::new(ChildHost { shared, ids: parameter_ids(api) });
        let view_host = ViewHost {
            ctx: &*host as *const ChildHost as *mut c_void,
            get_param: child_get,
            begin_edit: child_begin,
            perform_edit: child_perform,
            end_edit: child_end,
        };
        let opened = match inner.as_ref() {
            Some(inner) => (inner.open)(
                parent,
                &view_host,
                shared.width.load(SeqCst),
                shared.height.load(SeqCst),
                f64::from_bits(shared.scale.load(SeqCst)),
            ),
            None => std::ptr::null_mut(),
        };
        shared.ready.store(if opened.is_null() { 2 } else { 1 }, SeqCst);
        SetEvent(ready);
        if opened.is_null() {
            return;
        }
        let inner = &*inner;
        let mut generation = shared.size_generation.load(SeqCst);
        let waits = [command, host_process];
        loop {
            let woke = MsgWaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE, QS_ALLINPUT);
            if woke == WAIT_OBJECT_0 + 1 || shared.quit.load(SeqCst) != 0 {
                break; // closed, or the host is gone
            }
            let g = shared.size_generation.load(SeqCst);
            if g != generation {
                generation = g;
                (inner.set_size)(opened, shared.width.load(SeqCst), shared.height.load(SeqCst), f64::from_bits(shared.scale.load(SeqCst)));
            }
            let mut msg: Msg = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        (inner.close)(opened);
    }
}

/// Splits `view <args>` off a host-main command line.
pub(crate) fn editor_command(command_line: *const c_char) -> Option<String> {
    if command_line.is_null() {
        return None;
    }
    let line = unsafe { CStr::from_ptr(command_line) }.to_string_lossy().into_owned();
    line.strip_prefix("view ").map(str::to_string)
}
