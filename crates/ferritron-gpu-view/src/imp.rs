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
use std::sync::OnceLock;

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
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
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
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_SETCURSOR: u32 = 0x0020;
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

/// About 60 redraw checks per second.
const TICK_MS: u32 = 16;
const TIMER_ID: usize = 1;

struct View {
    hwnd: Hwnd,
    canvas: Canvas,
    /// Buttons held, so the editor keeps getting moves outside the window.
    held: u32,
    /// Windows was asked to report the mouse leaving (`WM_MOUSELEAVE`).
    tracking: bool,
}

impl View {
    fn resize(&mut self, width: u32, height: u32, scale: f64) {
        unsafe { SetWindowPos(self.hwnd, std::ptr::null_mut(), 0, 0, width as i32, height as i32, SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE) };
        self.canvas.resize(width, height, scale);
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

unsafe extern "system" fn window_proc(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> isize {
    let view = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut View;
    let Some(view) = (unsafe { view.as_mut() }) else { return unsafe { DefWindowProcW(hwnd, msg, w, l) } };
    let button = |msg| match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => Button::Left,
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => Button::Right,
        _ => Button::Middle,
    };
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint: PaintStruct = unsafe { std::mem::zeroed() };
            unsafe { BeginPaint(hwnd, &mut paint) };
            view.canvas.render();
            unsafe { EndPaint(hwnd, &paint) };
            return 0;
        }
        WM_TIMER if w == TIMER_ID => {
            view.canvas.tick();
            return 0;
        }
        WM_GETDLGCODE => return DLGC_WANTARROWS | DLGC_WANTTAB | DLGC_WANTALLKEYS | DLGC_WANTCHARS,
        WM_SETFOCUS | WM_KILLFOCUS => {
            view.canvas.input(Input::Focus(msg == WM_SETFOCUS));
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
            view.canvas.input(Input::Key { key, pressed, repeat, modifiers });
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
                view.canvas.input(Input::Char(ch));
            }
            return 0;
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let (x, y) = view.position(l);
            if view.held == 0 {
                unsafe { SetCapture(hwnd) };
            }
            view.held += 1;
            unsafe { SetFocus(hwnd) };
            view.canvas.input(Input::Press { x, y, button: button(msg) });
            return 0;
        }
        WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK => {
            let (x, y) = view.position(l);
            view.canvas.input(Input::DoubleClick { x, y, button: button(msg) });
            return 0;
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (x, y) = view.position(l);
            view.held = view.held.saturating_sub(1);
            if view.held == 0 {
                unsafe { ReleaseCapture() };
            }
            view.canvas.input(Input::Release { x, y, button: button(msg) });
            return 0;
        }
        WM_MOUSEMOVE => {
            if !view.tracking {
                let mut track = TrackMouse { size: size_of::<TrackMouse>() as u32, flags: TME_LEAVE, hwnd, hover_time: 0 };
                view.tracking = unsafe { TrackMouseEvent(&mut track) } != 0;
            }
            let (x, y) = view.position(l);
            let before = view.canvas.cursor();
            view.canvas.input(Input::Move { x, y });
            if view.canvas.cursor() != before {
                apply_cursor(view.canvas.cursor());
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            view.tracking = false;
            // While a button is held the editor still owns the mouse (capture).
            if view.held == 0 {
                view.canvas.input(Input::Leave);
            }
            return 0;
        }
        WM_SETCURSOR if (l & 0xffff) == HTCLIENT => {
            apply_cursor(view.canvas.cursor());
            return 1;
        }
        WM_MOUSEWHEEL => {
            // Wheel positions are in screen coordinates.
            let mut point = Point { x: (l & 0xffff) as u16 as i16 as i32, y: ((l >> 16) & 0xffff) as u16 as i16 as i32 };
            unsafe { ScreenToClient(hwnd, &mut point) };
            let delta = ((w >> 16) & 0xffff) as u16 as i16 as f64 / 120.0;
            view.canvas.input(Input::Wheel { x: point.x as f64 / view.canvas.scale, y: point.y as f64 / view.canvas.scale, delta });
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
            (unsafe { RegisterClassExW(&class) } != 0).then_some(name)
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
        let canvas = Canvas::new::<P, E>(surface, host, (width, height), scale)?;
        Some(Box::new(View { hwnd, canvas, held: 0, tracking: false }))
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
    view.canvas.close_editor();
    let _ = catch_unwind(AssertUnwindSafe(move || drop(view)));
    unsafe { DestroyWindow(hwnd) };
}
