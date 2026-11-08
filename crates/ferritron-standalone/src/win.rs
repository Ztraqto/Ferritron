//! The app's window on Windows: the plugin's editor fills it; the window
//! follows the editor's size limits and the monitor's DPI.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use crate::engine::{Shared, view_host};
use ferritron_core::abi::{EXT_VIEW, ViewApi, ViewHost};
use std::ffi::c_void;

type HWND = *mut c_void;

#[repr(C)]
struct WNDCLASSEXW {
    size: u32,
    style: u32,
    proc_: unsafe extern "system" fn(HWND, u32, usize, isize) -> isize,
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
struct MSG {
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: [i32; 2],
    private: u32,
}

#[repr(C)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MINMAXINFO {
    reserved: [i32; 2],
    max_size: [i32; 2],
    max_position: [i32; 2],
    min_track: [i32; 2],
    max_track: [i32; 2],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WNDCLASSEXW) -> u16;
    fn CreateWindowExW(ex: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: HWND, menu: *mut c_void, instance: *mut c_void, param: *mut c_void) -> HWND;
    fn DefWindowProcW(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize;
    fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const MSG) -> i32;
    fn DispatchMessageW(msg: *const MSG) -> isize;
    fn PostQuitMessage(code: i32);
    fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
    fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
    fn AdjustWindowRectExForDpi(rect: *mut RECT, style: u32, menu: i32, ex: u32, dpi: u32) -> i32;
    fn GetDpiForWindow(hwnd: HWND) -> u32;
    fn GetDpiForSystem() -> u32;
    fn SetProcessDpiAwarenessContext(context: isize) -> i32;
    fn SetWindowPos(hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
    fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn GetSysColorBrush(index: i32) -> *mut c_void;
    fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
    fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const WS_MAXIMIZEBOX: u32 = 0x0001_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const SS_CENTER: u32 = 0x1;
const SS_CENTERIMAGE: u32 = 0x200;
const CW_USEDEFAULT: i32 = i32::MIN;
const WM_DESTROY: u32 = 0x0002;
const WM_SIZE: u32 = 0x0005;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_DPICHANGED: u32 = 0x02E0;
const GWLP_USERDATA: i32 = -21;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
const IDC_ARROW: usize = 32512;
const COLOR_WINDOW: i32 = 5;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Window {
    view_api: Option<&'static ViewApi>,
    view: *mut c_void,
    style: u32,
}

fn scale_of(hwnd: HWND) -> f64 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 1.0,
        dpi => dpi as f64 / 96.0,
    }
}

/// The window size for a client area of `w` x `h` logical pixels at `dpi`.
fn outer(w: u32, h: u32, style: u32, dpi: u32) -> (i32, i32) {
    let scale = dpi as f64 / 96.0;
    let mut r = RECT { left: 0, top: 0, right: (w as f64 * scale).round() as i32, bottom: (h as f64 * scale).round() as i32 };
    unsafe { AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi) };
    (r.right - r.left, r.bottom - r.top)
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    let window = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Window).as_mut() };
    match (msg, window) {
        (WM_SIZE, Some(win)) if !win.view.is_null() => {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            unsafe { GetClientRect(hwnd, &mut r) };
            if let Some(api) = win.view_api {
                unsafe { (api.set_size)(win.view, (r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32, scale_of(hwnd)) };
            }
            0
        }
        (WM_GETMINMAXINFO, Some(win)) => {
            if let Some(api) = win.view_api {
                let info = unsafe { &mut *(l as *mut MINMAXINFO) };
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
                info.min_track = outer(api.min_width, api.min_height, win.style, dpi).into();
                if api.max_width > 0 && api.max_height > 0 {
                    info.max_track = outer(api.max_width, api.max_height, win.style, dpi).into();
                }
            }
            0
        }
        (WM_DPICHANGED, _) => {
            let r = unsafe { &*(l as *const RECT) };
            unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        (WM_DESTROY, _) => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

pub fn error(message: &str) {
    let (text, caption) = (wide(message), wide("Ferritron"));
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10) };
}

/// Shows the editor in a window and runs until the window closes.
pub fn run(shared: &'static Shared, title: &str) {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide("FerritronStandalone");
    let wc = WNDCLASSEXW {
        size: size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        proc_,
        cls_extra: 0,
        wnd_extra: 0,
        instance: module,
        icon: std::ptr::null_mut(),
        cursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16) },
        background: unsafe { GetSysColorBrush(COLOR_WINDOW) },
        menu_name: std::ptr::null(),
        class_name: class.as_ptr(),
        icon_small: std::ptr::null_mut(),
    };
    unsafe { RegisterClassExW(&wc) };
    let view_api = unsafe { ((shared.api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi).as_ref() };
    let (w, h) = view_api.map_or((420, 160), |v| (v.width, v.height));
    let resizable = view_api.is_some_and(|v| v.min_width != v.max_width || v.min_height != v.max_height);
    let style = if resizable { WS_OVERLAPPEDWINDOW } else { WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX) } | WS_CLIPCHILDREN;
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let (ow, oh) = outer(w, h, style, dpi);
    let title = wide(title);
    let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), title.as_ptr(), style, CW_USEDEFAULT, CW_USEDEFAULT, ow, oh, std::ptr::null_mut(), std::ptr::null_mut(), module, std::ptr::null_mut()) };
    if hwnd.is_null() {
        return;
    }
    let mut window = Box::new(Window { view_api, view: std::ptr::null_mut(), style });
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &raw mut *window as isize) };
    // The view host stays valid until the editor closes (below).
    let host: Box<ViewHost> = Box::new(view_host::for_shared(shared));
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(hwnd, &mut r) };
    if let Some(api) = view_api {
        window.view = unsafe { (api.open)(hwnd, &*host, (r.right - r.left) as u32, (r.bottom - r.top) as u32, scale_of(hwnd)) };
    }
    if window.view.is_null() {
        // No editor: say so in the window; the plugin still plays.
        let (text, kind) = (wide(if crate::japanese() { "このプラグインにはエディタがありません。" } else { "This plugin has no editor." }), wide("STATIC"));
        unsafe {
            CreateWindowExW(0, kind.as_ptr(), text.as_ptr(), WS_CHILD | WS_VISIBLE | SS_CENTER | SS_CENTERIMAGE, 0, 0, r.right, r.bottom, hwnd, std::ptr::null_mut(), module, std::ptr::null_mut())
        };
    }
    unsafe { ShowWindow(hwnd, 1) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if let (Some(api), false) = (view_api, window.view.is_null()) {
        unsafe { (api.close)(window.view) };
    }
    drop(host);
}
