//! Windows implementation: a child window of the host's HWND with a wgpu
//! surface. Its window procedure turns mouse messages into [`Input`], and a
//! timer redraws when parameters change.

use super::common::{Canvas, instance, open_view};
use super::{Button, Cursor, GpuEditor, Input, Key, Modifiers};
use ferritron_core::__private::PluginMeta;
use ferritron_core::Process;
use ferritron_core::abi::ViewHost;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle};
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};

type Hwnd = *mut c_void;

#[repr(C)]
struct WndClassExW {
    size: u32,
    style: u32,
    proc_: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
    icon_small: *mut c_void,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct PaintStruct {
    hdc: *mut c_void,
    erase: i32,
    rect: [i32; 4],
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn GetLastError() -> u32;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> isize;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
    fn SetTimer(hwnd: Hwnd, id: usize, ms: u32, proc_: *mut c_void) -> usize;
    fn KillTimer(hwnd: Hwnd, id: usize) -> i32;
    fn SetWindowPos(hwnd: Hwnd, after: Hwnd, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
    fn BeginPaint(hwnd: Hwnd, paint: *mut PaintStruct) -> *mut c_void;
    fn EndPaint(hwnd: Hwnd, paint: *const PaintStruct) -> i32;
    fn SetCapture(hwnd: Hwnd) -> Hwnd;
    fn ReleaseCapture() -> i32;
    fn ScreenToClient(hwnd: Hwnd, point: *mut Point) -> i32;
    fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetCursor(cursor: *mut c_void) -> *mut c_void;
    fn TrackMouseEvent(track: *mut TrackMouse) -> i32;
    fn SetFocus(hwnd: Hwnd) -> Hwnd;
    fn GetKeyState(key: i32) -> i16;
    fn GetParent(hwnd: Hwnd) -> Hwnd;
    fn PostMessageW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> i32;
    fn GetCursorPos(p: *mut Point) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn ClipCursor(rect: *const [i32; 4]) -> i32;
    fn ClientToScreen(hwnd: Hwnd, p: *mut Point) -> i32;
    fn GetClientRect(hwnd: Hwnd, rect: *mut [i32; 4]) -> i32;
    fn InvalidateRect(hwnd: Hwnd, rect: *const [i32; 4], erase: i32) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    /// Copies a device-independent bitmap to the device context, stretched to the destination
    /// rectangle (the rest of the signature is the Win32 one: `x_src`, `y_src`, `w_src`, `h_src`,
    /// `bits`, `info`, `usage`, `rop`).
    fn StretchDIBits(
        hdc: *mut c_void,
        x_dest: i32,
        y_dest: i32,
        w_dest: i32,
        h_dest: i32,
        x_src: i32,
        y_src: i32,
        w_src: i32,
        h_src: i32,
        bits: *const c_void,
        info: *const BitmapInfoHeader,
        usage: u32,
        rop: u32,
    ) -> i32;
}

/// `BITMAPINFOHEADER` of a 32-bit BI_RGB bitmap (no color table): the bitmap info a
/// `StretchDIBits` takes for a CPU frame.
#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    /// Negative: the rows run from the top down.
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}
const _: () = assert!(size_of::<BitmapInfoHeader>() == 40);
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateWaitableTimerExW(attributes: *mut c_void, name: *const u16, flags: u32, access: u32) -> *mut c_void;
    fn SetWaitableTimer(timer: *mut c_void, due: *const i64, period: i32, routine: *mut c_void, arg: *mut c_void, resume: i32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// A thread that posts [`WM_FAST_TICK`] to the window at the moment the view asks for, with a
/// high-resolution timer: `SetTimer` cannot do better than the 15.6 ms system tick, which would
/// cap a 120 Hz display at about 64 frames per second and make 30 Hz frames uneven. It is started
/// the first time an editor wants a precisely timed frame and ends when the view closes.
struct Ticker {
    shared: std::sync::Arc<TickerShared>,
    thread: Option<std::thread::JoinHandle<()>>,
    base: std::time::Instant,
}

struct TickerShared {
    stop: std::sync::atomic::AtomicBool,
    /// When to post, in nanoseconds after `base` (plus 1); 0 = nothing wanted.
    deadline: std::sync::atomic::AtomicU64,
    /// A tick message is waiting: do not post another.
    posted: std::sync::atomic::AtomicBool,
    /// Held while `deadline` or `stop` changes, so the thread never sleeps through one.
    gate: Mutex<()>,
    /// Signalled when there is a deadline to wait for (or the ticker stops).
    wake: Condvar,
}

const WM_FAST_TICK: u32 = 0x8001; // WM_APP + 1

impl Ticker {
    fn start(hwnd: Hwnd) -> Ticker {
        use std::sync::atomic::Ordering::*;
        let base = std::time::Instant::now();
        let shared = std::sync::Arc::new(TickerShared {
            stop: Default::default(),
            deadline: Default::default(),
            posted: Default::default(),
            gate: Mutex::new(()),
            wake: Condvar::new(),
        });
        let hwnd = hwnd as usize;
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ferritron-gpu-view-ticker".into())
            .spawn(move || {
                // CREATE_WAITABLE_TIMER_HIGH_RESOLUTION (Windows 10 1803 and later); else a plain sleep.
                let timer = unsafe { CreateWaitableTimerExW(std::ptr::null_mut(), std::ptr::null(), 0x2, 0x1F0003) };
                while !s.stop.load(Acquire) {
                    let d = s.deadline.load(Acquire);
                    if d == 0 {
                        // Nothing wanted: sleep until `wake_after` (or the drop) signals, not on a timer.
                        let guard = s.gate.lock().unwrap_or_else(PoisonError::into_inner);
                        if s.deadline.load(Acquire) == 0 && !s.stop.load(Acquire) {
                            let _woken = s.wake.wait(guard).unwrap_or_else(PoisonError::into_inner);
                        }
                        continue;
                    }
                    let target = d - 1;
                    let now = base.elapsed().as_nanos() as u64;
                    if now < target {
                        // wait, but not long: the view may move the deadline
                        let wait = (target - now).min(20_000_000);
                        if timer.is_null() {
                            std::thread::sleep(std::time::Duration::from_nanos(wait));
                        } else {
                            let due = -((wait / 100).max(1) as i64);
                            unsafe {
                                SetWaitableTimer(timer, &due, 0, std::ptr::null_mut(), std::ptr::null_mut(), 0);
                                WaitForSingleObject(timer, 100);
                            }
                        }
                        continue;
                    }
                    // due: post once, then wait for the next deadline
                    if s.deadline.compare_exchange(d, 0, AcqRel, Acquire).is_ok() && !s.posted.swap(true, AcqRel) {
                        unsafe { PostMessageW(hwnd as Hwnd, WM_FAST_TICK, 0, 0) };
                    }
                }
                if !timer.is_null() {
                    unsafe { CloseHandle(timer) };
                }
            })
            .ok();
        Ticker { shared, thread, base }
    }

    /// Wake the view after `after` (None: no wake-up wanted).
    fn wake_after(&self, after: Option<std::time::Duration>) {
        let v = after.map_or(0, |a| (self.base.elapsed() + a).as_nanos() as u64 + 1);
        let _gate = self.shared.gate.lock().unwrap_or_else(PoisonError::into_inner);
        self.shared.deadline.store(v, std::sync::atomic::Ordering::Release);
        self.shared.wake.notify_all();
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        {
            let _gate = self.shared.gate.lock().unwrap_or_else(PoisonError::into_inner);
            self.shared.stop.store(true, std::sync::atomic::Ordering::Release);
            self.shared.wake.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetCompositionTimingInfo(hwnd: Hwnd, info: *mut DwmTimingInfo) -> i32;
}

/// The start of `DWM_TIMING_INFO`. The header packs it to 1 byte: 292 bytes in all, and
/// `cbSize` must be the whole size.
#[repr(C)]
struct DwmTimingInfo {
    size: u32,
    /// `rateRefresh`: refreshes per second as a ratio.
    refresh_numerator: u32,
    refresh_denominator: u32,
    rest: [u8; 280],
}
const _: () = assert!(size_of::<DwmTimingInfo>() == 292);

/// The refresh rate of the display the compositor draws on, in Hz (`DwmGetCompositionTimingInfo`).
pub(crate) fn dwm_refresh_hz() -> Option<f64> {
    let mut info = DwmTimingInfo { size: 292, refresh_numerator: 0, refresh_denominator: 0, rest: [0; 280] };
    // SAFETY: `info` is a writable buffer of the size the call is told.
    let hr = unsafe { DwmGetCompositionTimingInfo(std::ptr::null_mut(), &mut info) };
    (hr >= 0 && info.refresh_denominator != 0).then(|| info.refresh_numerator as f64 / info.refresh_denominator as f64)
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfoExW) -> i32;
    fn EnumDisplaySettingsW(device: *const u16, mode: u32, devmode: *mut DevModeW) -> i32;
}

/// `MONITORINFOEXW`: the device name (`szDevice`) of a monitor.
#[repr(C)]
struct MonitorInfoExW {
    size: u32,
    monitor: [i32; 4],
    work: [i32; 4],
    flags: u32,
    device: [u16; 32],
}

/// `DEVMODEW` as bytes: only `dmSize` (offset 68) and `dmDisplayFrequency` (offset 184) are read.
#[repr(C)]
struct DevModeW {
    bytes: [u8; 220],
}

/// The refresh rate of the display the window is on, in Hz (`EnumDisplaySettingsW` of its monitor).
/// Monitors can differ (60 Hz next to 144 Hz); the compositor's rate does not tell them apart.
pub(crate) fn monitor_refresh_hz(hwnd: Hwnd) -> Option<f64> {
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const ENUM_CURRENT_SETTINGS: u32 = u32::MAX; // (DWORD)-1
    const DEVMODE_SIZE: usize = 220;
    const DM_SIZE: std::ops::Range<usize> = 68..70;
    const DM_DISPLAY_FREQUENCY: std::ops::Range<usize> = 184..188;
    // SAFETY: each call gets a buffer of the size it is told; the monitor handle is only used as given.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info = MonitorInfoExW { size: size_of::<MonitorInfoExW>() as u32, monitor: [0; 4], work: [0; 4], flags: 0, device: [0; 32] };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let mut mode = DevModeW { bytes: [0; DEVMODE_SIZE] };
    mode.bytes[DM_SIZE].copy_from_slice(&(DEVMODE_SIZE as u16).to_le_bytes());
    if unsafe { EnumDisplaySettingsW(info.device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) } == 0 {
        return None;
    }
    let hz = u32::from_le_bytes(mode.bytes[DM_DISPLAY_FREQUENCY].try_into().ok()?);
    // 0 and 1 mean the hardware default: unknown.
    (hz > 1).then_some(hz as f64)
}

#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: Hwnd) -> *mut c_void;
    fn ImmReleaseContext(hwnd: Hwnd, himc: *mut c_void) -> i32;
    fn ImmGetCompositionStringW(himc: *mut c_void, index: u32, buffer: *mut c_void, len: u32) -> i32;
    fn ImmSetCompositionWindow(himc: *mut c_void, form: *const CompositionForm) -> i32;
}

/// `COMPOSITIONFORM`: where the composition window of the input method goes.
#[repr(C)]
struct CompositionForm {
    style: u32,
    position: Point,
    area: [i32; 4],
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(hkey: isize, sub_key: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, size: *mut u32) -> i32;
}

/// Whether Windows apps are set to the dark theme (`AppsUseLightTheme` = 0).
pub(crate) fn system_dark() -> bool {
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const RRF_RT_REG_DWORD: u32 = 0x10;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (key, value) = (wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"), wide("AppsUseLightTheme"));
    let (mut data, mut size) = (1u32, 4u32);
    // SAFETY: reads one DWORD into a local.
    let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut data as *mut u32 as *mut c_void, &mut size) };
    status == 0 && data == 0
}

const GWLP_USERDATA: i32 = -21;
const WS_CHILD: u32 = 0x4000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const CS_DBLCLKS: u32 = 0x0008;
const CS_OWNDC: u32 = 0x0020;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const IDC_ARROW: usize = 32512;

const WM_PAINT: u32 = 0x000F;
const WM_SETFOCUS: u32 = 0x0007;
const WM_KILLFOCUS: u32 = 0x0008;
const WM_GETDLGCODE: u32 = 0x0087;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const DLGC_WANTARROWS: isize = 0x0001;
const DLGC_WANTTAB: isize = 0x0002;
const DLGC_WANTALLKEYS: isize = 0x0004;
const DLGC_WANTCHARS: isize = 0x0080;
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_TIMER: u32 = 0x0113;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_SETTINGCHANGE: u32 = 0x001A;
const WM_THEMECHANGED: u32 = 0x031A;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_SETCURSOR: u32 = 0x0020;
const WM_MOVE: u32 = 0x0003;
const WM_ACTIVATE: u32 = 0x0006;
const WM_CANCELMODE: u32 = 0x001F;
const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_CAPTURECHANGED: u32 = 0x0215;
const WM_IME_STARTCOMPOSITION: u32 = 0x010D;
const WM_IME_COMPOSITION: u32 = 0x010F;
/// The key code of a key the input method is processing (the composed text comes as `WM_IME_COMPOSITION`).
const VK_PROCESSKEY: usize = 0xE5;
const GCS_RESULTSTR: u32 = 0x0800;
const CFS_POINT: u32 = 0x0002;
const HTCLIENT: isize = 1;
const TME_LEAVE: u32 = 0x0000_0002;

#[repr(C)]
struct TrackMouse {
    size: u32,
    flags: u32,
    hwnd: Hwnd,
    hover_time: u32,
}

/// Shows the pointer shape the editor asked for.
fn apply_cursor(cursor: Cursor) {
    let id: usize = match cursor {
        Cursor::Hidden => {
            unsafe { SetCursor(std::ptr::null_mut()) };
            return;
        }
        Cursor::Hand | Cursor::Grab => 32649,       // IDC_HAND
        Cursor::Grabbing | Cursor::Move => 32646,   // IDC_SIZEALL
        Cursor::Text => 32513,                      // IDC_IBEAM
        Cursor::Crosshair => 32515,                 // IDC_CROSS
        Cursor::ResizeHorizontal => 32644,          // IDC_SIZEWE
        Cursor::ResizeVertical => 32645,            // IDC_SIZENS
        Cursor::NotAllowed => 32648,                // IDC_NO
        _ => IDC_ARROW,
    };
    unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), id as *const u16)) };
}

/// The slow check: polls for changes made outside the editor (host automation) and is the
/// safety net under the precise ticker.
const TICK_MS: u32 = 16;
const TIMER_ID: usize = 1;

/// A pointer lock (see `Ui::lock_pointer`).
struct Lock {
    /// The lock point, in client pixels: moves are measured from here and the pointer is put back.
    center: (i32, i32),
    /// Where the pointer was, in screen pixels.
    saved: (i32, i32),
}

struct View {
    hwnd: Hwnd,
    canvas: Canvas,
    /// Buttons held, so the editor keeps getting moves outside the window.
    held: u32,
    /// The button of the last press, and where the pointer was last (for the release sent when
    /// the capture is lost).
    pressed: Button,
    pos: (f64, f64),
    /// Windows was asked to report the mouse leaving (`WM_MOUSELEAVE`).
    tracking: bool,
    /// Wakes the view exactly when a frame is due (started when needed).
    ticker: Option<Ticker>,
    /// While the pointer is locked: the client position of the lock point and where the pointer was.
    lock: Option<Lock>,
}

impl View {
    fn resize(&mut self, width: u32, height: u32, scale: f64) {
        unsafe { SetWindowPos(self.hwnd, std::ptr::null_mut(), 0, 0, width as i32, height as i32, SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE) };
        self.canvas.resize(width, height, scale);
        self.request_paint();
    }

    /// Asks the system to paint the window when the CPU mode drew a new frame (the paint handler
    /// then shows it; on the GPU the frame is presented directly).
    fn request_paint(&mut self) {
        if self.canvas.take_paint_request() {
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Shows the CPU mode's last frame in the window, stretched to its size.
    fn paint_cpu(&self, hdc: *mut c_void) {
        let Some((pixels, (w, h), (dest_w, dest_h))) = self.canvas.cpu_pixels() else { return };
        let info = BitmapInfoHeader {
            size: size_of::<BitmapInfoHeader>() as u32,
            width: w as i32,
            height: -(h as i32),
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            size_image: 0,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            clr_used: 0,
            clr_important: 0,
        };
        unsafe {
            StretchDIBits(hdc, 0, 0, dest_w as i32, dest_h as i32, 0, 0, w as i32, h as i32, pixels.as_ptr().cast(), &info, DIB_RGB_COLORS, SRCCOPY);
        }
    }

    /// A check of the pacing (from the slow timer or the precise ticker): draws if due, then
    /// arranges the next wake-up. The slow timer (16 ms) only polls for changes made outside the
    /// editor; frames are timed by the ticker thread, started when the first one is wanted.
    fn check(&mut self, hwnd: Hwnd) {
        let next = self.canvas.tick();
        self.request_paint();
        if self.canvas.take_refresh_due() {
            self.canvas.set_refresh_hz(monitor_refresh_hz(hwnd));
        }
        match next.due_in {
            Some(after) => {
                let ticker = self.ticker.get_or_insert_with(|| Ticker::start(hwnd));
                ticker.wake_after(Some(after));
            }
            None => {
                if let Some(t) = &self.ticker {
                    t.wake_after(None);
                }
            }
        }
    }

    /// Gives `event` to the editor, then applies a pointer lock it asked for. A frame the editor
    /// asked for is drawn by the next check, which the ticker runs at once (within a frame).
    fn input(&mut self, hwnd: Hwnd, event: Input) {
        self.canvas.input(event);
        self.sync_lock(hwnd);
        if self.canvas.wants_frame() {
            let ticker = self.ticker.get_or_insert_with(|| Ticker::start(hwnd));
            ticker.wake_after(Some(std::time::Duration::ZERO));
        }
    }

    /// The capture was lost (Alt+Tab, a dialog, the host took the mouse) while a button was held:
    /// the editor gets the release it will not otherwise get, and the pointer is free again.
    fn capture_lost(&mut self, hwnd: Hwnd) {
        if self.held == 0 {
            // Our own `ReleaseCapture` after the last button went up: nothing is held.
            return;
        }
        self.held = 0;
        let (x, y) = self.pos;
        let button = self.pressed;
        self.input(hwnd, Input::Release { x, y, button, modifiers: modifiers() });
        self.input(hwnd, Input::Leave);
        self.end_lock();
    }

    /// Puts the input method's composition window at the editor's caret (the top left if it has none).
    fn place_ime(&self, hwnd: Hwnd) {
        let (x, y) = self.canvas.text_input().unwrap_or((0.0, 0.0));
        let scale = self.canvas.scale;
        let form = CompositionForm { style: CFS_POINT, position: Point { x: (x * scale) as i32, y: (y * scale) as i32 }, area: [0; 4] };
        let himc = unsafe { ImmGetContext(hwnd) };
        if !himc.is_null() {
            unsafe {
                ImmSetCompositionWindow(himc, &form);
                ImmReleaseContext(hwnd, himc);
            }
        }
    }

    /// Locks or unlocks the pointer if the editor asked since the last look.
    fn sync_lock(&mut self, hwnd: Hwnd) {
        let Some(want) = self.canvas.take_lock_request() else { return };
        if want && self.lock.is_none() {
            let (mut cursor, mut origin, mut rect) = (Point { x: 0, y: 0 }, Point { x: 0, y: 0 }, [0i32; 4]);
            unsafe {
                GetCursorPos(&mut cursor);
                GetClientRect(hwnd, &mut rect);
                ClientToScreen(hwnd, &mut origin);
            }
            let center = ((rect[2] - rect[0]) / 2, (rect[3] - rect[1]) / 2);
            self.lock = Some(Lock { center, saved: (cursor.x, cursor.y) });
            // The pointer stays inside the editor and sits at the lock point.
            let clip = [origin.x, origin.y, origin.x + rect[2], origin.y + rect[3]];
            unsafe {
                ClipCursor(&clip);
                SetCursorPos(origin.x + center.0, origin.y + center.1);
                SetCursor(std::ptr::null_mut());
            }
            self.canvas.set_locked(true);
        } else if !want {
            self.end_lock();
        }
    }

    fn end_lock(&mut self) {
        if let Some(lock) = self.lock.take() {
            unsafe {
                ClipCursor(std::ptr::null());
                SetCursorPos(lock.saved.0, lock.saved.1);
            }
            self.canvas.set_locked(false);
            apply_cursor(self.canvas.cursor());
        }
    }

    fn position(&self, l: isize) -> (f64, f64) {
        let x = (l & 0xffff) as u16 as i16 as f64;
        let y = ((l >> 16) & 0xffff) as u16 as i16 as f64;
        (x / self.canvas.scale, y / self.canvas.scale)
    }
}

thread_local! {
    /// Whether the last key press was one the editor wanted (its characters then are text for it).
    static TAKES_TEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn key_of(code: usize) -> Key {
    match code {
        0x25 => Key::Left,
        0x26 => Key::Up,
        0x27 => Key::Right,
        0x28 => Key::Down,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x08 => Key::Backspace,
        0x2E => Key::Delete,
        0x0D => Key::Enter,
        0x1B => Key::Escape,
        0x09 => Key::Tab,
        0x20 => Key::Space,
        0x30..=0x39 | 0x41..=0x5A => Key::Character(code as u8 as char),
        other => Key::Other(other as u32),
    }
}

fn modifiers() -> Modifiers {
    let down = |key| unsafe { GetKeyState(key) } < 0;
    Modifiers { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU) }
}

/// The text the input method finished composing (`GCS_RESULTSTR`), as Rust text.
fn ime_result(hwnd: Hwnd) -> String {
    let himc = unsafe { ImmGetContext(hwnd) };
    if himc.is_null() {
        return String::new();
    }
    // The size in bytes first (negative on error), then the UTF-16 units.
    let bytes = unsafe { ImmGetCompositionStringW(himc, GCS_RESULTSTR, std::ptr::null_mut(), 0) };
    let mut units = vec![0u16; (bytes.max(0) as usize) / 2];
    if !units.is_empty() {
        unsafe { ImmGetCompositionStringW(himc, GCS_RESULTSTR, units.as_mut_ptr().cast(), (units.len() * 2) as u32) };
    }
    unsafe { ImmReleaseContext(hwnd, himc) };
    String::from_utf16_lossy(&units)
}

/// The window procedure is `extern "system"`: a panic must not cross it (Rust would stop the
/// process). Everything the view does for a message runs inside `catch_unwind`; a view that
/// panicked stops drawing and the message goes to the default procedure.
unsafe extern "system" fn window_proc(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> isize {
    let view = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut View;
    if view.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, w, l) };
    }
    // SAFETY: the view lives from `create` until `close` clears the pointer (and destroys the window).
    match catch_unwind(AssertUnwindSafe(|| handle(hwnd, msg, w, l, unsafe { &mut *view }))) {
        Ok(result) => result,
        Err(_) => {
            unsafe { (*view).canvas.mark_broken() };
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
    }
}

/// One message of the view; the default procedure gets what it does not return itself.
fn handle(hwnd: Hwnd, msg: u32, w: usize, l: isize, view: &mut View) -> isize {
    let button = |msg| match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => Button::Left,
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => Button::Right,
        _ => Button::Middle,
    };
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint: PaintStruct = unsafe { std::mem::zeroed() };
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            if view.canvas.on_cpu() {
                // The CPU mode draws at the pacer's frames (see `check`); painting only shows the last one.
                view.paint_cpu(hdc);
            } else {
                view.canvas.render();
            }
            unsafe { EndPaint(hwnd, &paint) };
            return 0;
        }
        WM_TIMER if w == TIMER_ID => {
            view.check(hwnd);
            return 0;
        }
        WM_FAST_TICK => {
            if let Some(t) = &view.ticker {
                t.shared.posted.store(false, std::sync::atomic::Ordering::Release);
            }
            view.check(hwnd);
            return 0;
        }
        WM_GETDLGCODE => return DLGC_WANTARROWS | DLGC_WANTTAB | DLGC_WANTALLKEYS | DLGC_WANTCHARS,
        WM_SETFOCUS | WM_KILLFOCUS => {
            if msg == WM_KILLFOCUS {
                // The host's window was deactivated (the focus leaves with it): unlock the pointer.
                view.end_lock();
            }
            view.input(hwnd, Input::Focus(msg == WM_SETFOCUS));
            return 0;
        }
        WM_ACTIVATE if (w & 0xffff) == 0 => view.end_lock(),
        WM_CAPTURECHANGED | WM_CANCELMODE => view.capture_lost(hwnd),
        WM_MOVE | WM_DISPLAYCHANGE => {
            // The window may be on another monitor now (or the display settings changed).
            view.canvas.set_refresh_hz(monitor_refresh_hz(hwnd));
        }
        WM_IME_STARTCOMPOSITION => view.place_ime(hwnd),
        WM_IME_COMPOSITION => {
            if (l as u32) & GCS_RESULTSTR != 0 {
                // The finished text: one character each (what the editor takes as text).
                if view.canvas.text_input().is_some() {
                    for ch in ime_result(hwnd).chars().filter(|c| !c.is_control()) {
                        view.input(hwnd, Input::Char(ch));
                    }
                    return 0;
                }
            } else {
                view.place_ime(hwnd);
            }
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP if w == VK_PROCESSKEY && view.canvas.text_input().is_some() => {
            // The input method is composing for the editor's text field: its keys stay here, the
            // host's window must not see them (the composed text arrives as WM_IME_COMPOSITION).
            return 0;
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let (key, modifiers) = (key_of(w), modifiers());
            let wanted = view.canvas.editor(|editor, _, _| editor.wants_key(key, modifiers)).unwrap_or(false);
            if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                TAKES_TEXT.with(|t| t.set(wanted));
            }
            if !wanted {
                // The host's window gets what the editor does not use (Windows
                // sends keys only to the focused window).
                let parent = unsafe { GetParent(hwnd) };
                if !parent.is_null() {
                    unsafe { PostMessageW(parent, msg, w, l) };
                }
                return 0;
            }
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let repeat = pressed && (l & (1 << 30)) != 0;
            view.input(hwnd, Input::Key { key, pressed, repeat, modifiers });
            return 0;
        }
        WM_CHAR => {
            // Characters outside the basic plane arrive as two UTF-16 units.
            thread_local! { static HIGH: std::cell::Cell<u16> = const { std::cell::Cell::new(0) }; }
            let unit = w as u16;
            let ch = match unit {
                0xD800..=0xDBFF => {
                    HIGH.with(|h| h.set(unit));
                    None
                }
                0xDC00..=0xDFFF => {
                    let high = HIGH.with(|h| h.replace(0));
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                }
                _ => char::from_u32(unit as u32),
            };
            // Control characters are keys (Key above), not text; text only follows a key the editor took.
            if let Some(ch) = ch.filter(|c| !c.is_control()).filter(|_| TAKES_TEXT.with(|t| t.get())) {
                view.input(hwnd, Input::Char(ch));
            }
            return 0;
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let (x, y) = view.position(l);
            view.pressed = button(msg);
            view.pos = (x, y);
            if view.held == 0 {
                unsafe { SetCapture(hwnd) };
            }
            view.held += 1;
            unsafe { SetFocus(hwnd) };
            view.input(hwnd, Input::Press { x, y, button: button(msg), modifiers: modifiers() });
            return 0;
        }
        WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK => {
            let (x, y) = view.position(l);
            view.input(hwnd, Input::DoubleClick { x, y, button: button(msg), modifiers: modifiers() });
            return 0;
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (x, y) = view.position(l);
            view.pos = (x, y);
            view.held = view.held.saturating_sub(1);
            if view.held == 0 {
                unsafe { ReleaseCapture() };
            }
            view.input(hwnd, Input::Release { x, y, button: button(msg), modifiers: modifiers() });
            return 0;
        }
        WM_MOUSEMOVE => {
            if !view.tracking {
                let mut track = TrackMouse { size: size_of::<TrackMouse>() as u32, flags: TME_LEAVE, hwnd, hover_time: 0 };
                view.tracking = unsafe { TrackMouseEvent(&mut track) } != 0;
            }
            if let Some(lock) = &view.lock {
                // Locked: report how far the mouse moved from the lock point and put the pointer back.
                let (px, py) = ((l & 0xffff) as u16 as i16 as i32, ((l >> 16) & 0xffff) as u16 as i16 as i32);
                let (dx, dy) = (px - lock.center.0, py - lock.center.1);
                if dx != 0 || dy != 0 {
                    let (cx, cy) = lock.center;
                    let mut origin = Point { x: 0, y: 0 };
                    unsafe {
                        ClientToScreen(hwnd, &mut origin);
                        SetCursorPos(origin.x + cx, origin.y + cy);
                    }
                    let s = view.canvas.scale;
                    view.input(hwnd, Input::PointerDelta { dx: dx as f64 / s, dy: dy as f64 / s, modifiers: modifiers() });
                }
                return 0;
            }
            let (x, y) = view.position(l);
            view.pos = (x, y);
            let before = view.canvas.cursor();
            view.input(hwnd, Input::Move { x, y, modifiers: modifiers() });
            if view.canvas.cursor() != before {
                apply_cursor(view.canvas.cursor());
            }
            return 0;
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            // Child windows seldom get these (the host's top-level window does); tell the editor when they arrive.
            view.input(hwnd, Input::ThemeChanged(system_dark()));
        }
        WM_MOUSELEAVE => {
            view.tracking = false;
            // While a button is held (or the pointer is locked) the editor still owns the mouse.
            if view.held == 0 && view.lock.is_none() {
                view.input(hwnd, Input::Leave);
            }
            return 0;
        }
        WM_SETCURSOR if (l & 0xffff) == HTCLIENT => {
            if view.lock.is_some() {
                unsafe { SetCursor(std::ptr::null_mut()) };
            } else {
                apply_cursor(view.canvas.cursor());
            }
            return 1;
        }
        WM_MOUSEWHEEL => {
            // Wheel positions are in screen coordinates.
            let mut point = Point { x: (l & 0xffff) as u16 as i16 as i32, y: ((l >> 16) & 0xffff) as u16 as i16 as i32 };
            unsafe { ScreenToClient(hwnd, &mut point) };
            let delta = ((w >> 16) & 0xffff) as u16 as i16 as f64 / 120.0;
            view.input(hwnd, Input::Wheel { x: point.x as f64 / view.canvas.scale, y: point.y as f64 / view.canvas.scale, delta, delta_x: 0.0, modifiers: modifiers() });
            return 0;
        }
        WM_MOUSEHWHEEL => {
            let mut point = Point { x: (l & 0xffff) as u16 as i16 as i32, y: ((l >> 16) & 0xffff) as u16 as i16 as i32 };
            unsafe { ScreenToClient(hwnd, &mut point) };
            let delta_x = ((w >> 16) & 0xffff) as u16 as i16 as f64 / 120.0;
            view.input(hwnd, Input::Wheel { x: point.x as f64 / view.canvas.scale, y: point.y as f64 / view.canvas.scale, delta: 0.0, delta_x, modifiers: modifiers() });
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// The window class of this copy of the backend. The name includes the
/// window procedure's address: two plugins with their own copies never share
/// (and never call into an unloaded) procedure.
fn class_name() -> Option<&'static [u16]> {
    static CLASS: OnceLock<Option<Vec<u16>>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let name: Vec<u16> = format!("FerritronGpuView{:x}", window_proc as *const () as usize).encode_utf16().chain([0]).collect();
            let class = WndClassExW {
                size: size_of::<WndClassExW>() as u32,
                style: CS_DBLCLKS | CS_OWNDC,
                proc_: window_proc,
                cls_extra: 0,
                wnd_extra: 0,
                instance: unsafe { GetModuleHandleW(std::ptr::null()) },
                icon: std::ptr::null_mut(),
                cursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16) },
                background: std::ptr::null_mut(),
                menu_name: std::ptr::null(),
                class_name: name.as_ptr(),
                icon_small: std::ptr::null_mut(),
            };
            // Already registered: the same name means the same window procedure (the name holds its
            // address), as when the host loads this library again at the same place.
            const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
            let ok = unsafe { RegisterClassExW(&class) } != 0 || unsafe { GetLastError() } == ERROR_CLASS_ALREADY_EXISTS;
            ok.then_some(name)
        })
        .as_deref()
}

fn create<P: Process + PluginMeta, E: GpuEditor>(parent: Hwnd, host: ViewHost, width: u32, height: u32, scale: f64) -> Option<Box<View>> {
    let class = class_name()?;
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
            0,
            0,
            width as i32,
            height as i32,
            parent,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        )
    };
    if hwnd.is_null() {
        return None;
    }
    let made = (|| {
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::Win32(Win32WindowHandle::new(NonZeroIsize::new(hwnd as isize)?)),
        };
        // SAFETY: the window outlives the surface (`close` drops the view first).
        let surface = unsafe { instance().create_surface_unsafe(target) }.ok()?;
        let mut canvas = Canvas::new::<P, E>(surface, host, (width, height), scale)?;
        canvas.set_refresh_hz(monitor_refresh_hz(hwnd));
        Some(Box::new(View { hwnd, canvas, held: 0, pressed: Button::Left, pos: (0.0, 0.0), tracking: false, ticker: None, lock: None }))
    })();
    let Some(mut view) = made else {
        unsafe { DestroyWindow(hwnd) };
        return None;
    };
    let ptr: *mut View = &mut *view;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        SetTimer(hwnd, TIMER_ID, TICK_MS, std::ptr::null_mut());
    }
    view.canvas.render();
    view.request_paint();
    Some(view)
}

pub(crate) unsafe extern "C" fn open<P: Process + PluginMeta, E: GpuEditor>(
    parent: *mut c_void,
    host: *const ViewHost,
    width: u32,
    height: u32,
    scale: f64,
) -> *mut c_void {
    open_view(parent, host, |host| create::<P, E>(parent, host, width, height, scale))
}

pub(crate) unsafe extern "C" fn set_size(view: *mut c_void, width: u32, height: u32, scale: f64) {
    let Some(view) = (unsafe { (view as *mut View).as_mut() }) else { return };
    let scale = if scale > 0.0 { scale } else { view.canvas.scale };
    let _ = catch_unwind(AssertUnwindSafe(|| view.resize(width, height, scale)));
}

pub(crate) unsafe extern "C" fn close(view: *mut c_void) {
    if view.is_null() {
        return;
    }
    let mut view = unsafe { Box::from_raw(view as *mut View) };
    let hwnd = view.hwnd;
    unsafe {
        KillTimer(hwnd, TIMER_ID);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    // The pointer goes back to where it was; the ticker thread ends first: it posts to the window.
    view.end_lock();
    view.ticker = None;
    view.canvas.close_editor();
    let _ = catch_unwind(AssertUnwindSafe(move || drop(view)));
    unsafe { DestroyWindow(hwnd) };
}
