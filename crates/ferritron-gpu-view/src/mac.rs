//! macOS implementation: an NSView subview of the host's NSView with a wgpu
//! (Metal) surface. Its event methods turn mouse and key events into
//! [`Input`], and a run loop timer redraws when parameters change.
//!
//! Sizes from the host are points (logical); the surface is the size in
//! pixels at the window's backing scale (2 on Retina screens).

use super::common::{Canvas, instance, open_view};
use super::{Button, GpuEditor, Input, Key, Modifiers};
use ferritron_core::__private::{PluginMeta, UiTimer};
use ferritron_core::Process;
use ferritron_core::abi::ViewHost;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use raw_window_handle::{AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::OnceLock;

/// About 60 redraw checks per second.
const TICK_MS: u32 = 16;

// NSEventModifierFlags
const SHIFT: usize = 1 << 17;
const CONTROL: usize = 1 << 18;
const OPTION: usize = 1 << 19;
const COMMAND: usize = 1 << 20;

// NSTrackingAreaOptions: mouse moves whenever the view is visible.
const TRACKING_MOUSE_MOVED: usize = 0x02;
const TRACKING_ACTIVE_ALWAYS: usize = 0x80;
const TRACKING_IN_VISIBLE_RECT: usize = 0x200;

thread_local! {
    /// Views by their NSView. Events arrive on the main thread.
    static VIEWS: RefCell<HashMap<usize, *mut View>> = RefCell::new(HashMap::new());
    /// Whether the last key press was one the editor wanted (its characters then are text for it).
    static TAKES_TEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct View {
    /// Stops first: no tick may run while the view goes away.
    timer: Option<UiTimer>,
    canvas: Canvas,
    /// Points in the host's units (`set_size`).
    points: (u32, u32),
    /// Released last, after the surface drawn into it.
    ns_view: Retained<AnyObject>,
}

impl View {
    /// Applies the size in points and the window's current backing scale.
    fn layout(&mut self) {
        let (w, h) = self.points;
        let backing = backing_scale(&self.ns_view);
        unsafe {
            let _: () = msg_send![&*self.ns_view, setFrameSize: CGSize::new(w as f64, h as f64)];
            let layer: *mut AnyObject = msg_send![&*self.ns_view, layer];
            if let Some(layer) = layer.as_ref() {
                let _: () = msg_send![layer, setContentsScale: backing];
            }
        }
        self.canvas.resize(to_pixels(w, backing), to_pixels(h, backing), backing);
    }
}

fn to_pixels(points: u32, backing: f64) -> u32 {
    (points as f64 * backing).round() as u32
}

/// Pixels per point of the view's window (or the main screen's).
fn backing_scale(view: &AnyObject) -> f64 {
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        let scale: f64 = match window.as_ref() {
            Some(window) => msg_send![window, backingScaleFactor],
            None => {
                let screen: *mut AnyObject = msg_send![AnyClass::get(c"NSScreen").unwrap(), mainScreen];
                match screen.as_ref() {
                    Some(screen) => msg_send![screen, backingScaleFactor],
                    None => 1.0,
                }
            }
        };
        if scale > 0.0 { scale } else { 1.0 }
    }
}

fn with_view(this: &AnyObject, f: impl FnOnce(&mut View)) {
    let view = VIEWS.with(|v| v.borrow().get(&(this as *const AnyObject as usize)).copied());
    if let Some(view) = view {
        let _ = catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *view })));
    }
}

/// An event's position in the view, in points from the top left (the view is flipped).
fn position(this: &AnyObject, event: *mut AnyObject) -> (f64, f64) {
    unsafe {
        let window_point: CGPoint = msg_send![event, locationInWindow];
        let point: CGPoint = msg_send![this, convertPoint: window_point, fromView: std::ptr::null_mut::<AnyObject>()];
        (point.x, point.y)
    }
}

fn button_of(event: *mut AnyObject) -> Button {
    match unsafe { msg_send![event, buttonNumber] } {
        0isize => Button::Left,
        1isize => Button::Right,
        _ => Button::Middle,
    }
}

fn modifiers_of(event: *mut AnyObject) -> Modifiers {
    let flags: usize = unsafe { msg_send![event, modifierFlags] };
    // Command is the shortcut key on macOS: it counts as ctrl.
    Modifiers { shift: flags & SHIFT != 0, ctrl: flags & (CONTROL | COMMAND) != 0, alt: flags & OPTION != 0 }
}

/// The event's text (`characters` or `charactersIgnoringModifiers`).
fn text_of(event: *mut AnyObject, ignoring_modifiers: bool) -> String {
    unsafe {
        let string: *mut AnyObject =
            if ignoring_modifiers { msg_send![event, charactersIgnoringModifiers] } else { msg_send![event, characters] };
        let Some(string) = string.as_ref() else { return String::new() };
        let utf8: *const std::ffi::c_char = msg_send![string, UTF8String];
        if utf8.is_null() { String::new() } else { CStr::from_ptr(utf8).to_string_lossy().into_owned() }
    }
}

/// Virtual key codes of the keys with a meaning; letters and digits by their character.
fn key_of(event: *mut AnyObject) -> Key {
    let code: u16 = unsafe { msg_send![event, keyCode] };
    match code {
        123 => Key::Left,
        124 => Key::Right,
        125 => Key::Down,
        126 => Key::Up,
        115 => Key::Home,
        119 => Key::End,
        116 => Key::PageUp,
        121 => Key::PageDown,
        51 => Key::Backspace,
        117 => Key::Delete,
        36 | 76 => Key::Enter,
        53 => Key::Escape,
        48 => Key::Tab,
        49 => Key::Space,
        other => match text_of(event, true).chars().next() {
            Some(c) if c.is_ascii_alphanumeric() => Key::Character(c.to_ascii_uppercase()),
            _ => Key::Other(other as u32),
        },
    }
}

extern "C-unwind" fn yes(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

extern "C-unwind" fn yes_for(_this: &AnyObject, _cmd: Sel, _event: *mut AnyObject) -> Bool {
    Bool::YES
}

extern "C-unwind" fn mouse_down(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    unsafe {
        let window: *mut AnyObject = msg_send![this, window];
        if let Some(window) = window.as_ref() {
            let _: Bool = msg_send![window, makeFirstResponder: this];
        }
    }
    let (x, y) = position(this, event);
    let (button, clicks): (Button, isize) = (button_of(event), unsafe { msg_send![event, clickCount] });
    with_view(this, |view| view.canvas.input(if clicks == 2 { Input::DoubleClick { x, y, button } } else { Input::Press { x, y, button } }));
}

extern "C-unwind" fn mouse_up(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let (x, y) = position(this, event);
    let button = button_of(event);
    with_view(this, |view| view.canvas.input(Input::Release { x, y, button }));
}

/// Moves, with or without buttons held (AppKit keeps sending drags to the
/// view the press started in, also outside it).
extern "C-unwind" fn mouse_moved(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let (x, y) = position(this, event);
    with_view(this, |view| view.canvas.input(Input::Move { x, y }));
}

extern "C-unwind" fn scroll_wheel(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let (x, y) = position(this, event);
    let (dy, precise): (f64, Bool) = unsafe { (msg_send![event, scrollingDeltaY], msg_send![event, hasPreciseScrollingDeltas]) };
    // Trackpads report points; a wheel notch is about 10 of them.
    let delta = if precise.as_bool() { dy / 10.0 } else { dy };
    if delta != 0.0 {
        with_view(this, |view| view.canvas.input(Input::Wheel { x, y, delta }));
    }
}

fn key(this: &AnyObject, event: *mut AnyObject, pressed: bool, superclass: &AnyClass) {
    let (key, modifiers) = (key_of(event), modifiers_of(event));
    let mut wanted = false;
    with_view(this, |view| wanted = view.canvas.editor(|editor, _, _| editor.wants_key(key, modifiers)).unwrap_or(false));
    if pressed {
        TAKES_TEXT.with(|t| t.set(wanted));
    }
    if !wanted {
        // NSView passes keys it does not handle up the responder chain to the host.
        unsafe {
            if pressed {
                let _: () = msg_send![super(this, superclass), keyDown: event];
            } else {
                let _: () = msg_send![super(this, superclass), keyUp: event];
            }
        }
        return;
    }
    let repeat: Bool = unsafe { msg_send![event, isARepeat] };
    with_view(this, |view| {
        view.canvas.input(Input::Key { key, pressed, repeat: pressed && repeat.as_bool(), modifiers });
        if pressed && TAKES_TEXT.with(|t| t.get()) {
            // Text after the keyboard layout; control characters are keys, not text.
            for ch in text_of(event, false).chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c)) {
                view.canvas.input(Input::Char(ch));
            }
        }
    });
}

extern "C-unwind" fn key_down(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    key(this, event, true, view_class().superclass);
}

extern "C-unwind" fn key_up(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    key(this, event, false, view_class().superclass);
}

extern "C-unwind" fn become_first_responder(this: &AnyObject, _cmd: Sel) -> Bool {
    with_view(this, |view| view.canvas.input(Input::Focus(true)));
    Bool::YES
}

extern "C-unwind" fn resign_first_responder(this: &AnyObject, _cmd: Sel) -> Bool {
    with_view(this, |view| view.canvas.input(Input::Focus(false)));
    Bool::YES
}

/// Moved to a screen with another backing scale.
extern "C-unwind" fn backing_changed(this: &AnyObject, _cmd: Sel) {
    with_view(this, View::layout);
}

struct ViewClass {
    class: &'static AnyClass,
    superclass: &'static AnyClass,
}

/// The NSView subclass of this copy of the backend. The name includes a
/// function's address: two plugins with their own copies never share (and
/// never call into an unloaded) class.
fn view_class() -> &'static ViewClass {
    static CLASS: OnceLock<ViewClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let superclass = AnyClass::get(c"NSView").expect("AppKit is loaded");
        let name = CString::new(format!("FerritronGpuView{:x}", mouse_down as *const () as usize)).unwrap();
        let mut builder = ClassBuilder::new(&name, superclass).expect("the class name is unique to this library");
        unsafe {
            builder.add_method(sel!(isFlipped), yes as extern "C-unwind" fn(_, _) -> _);
            builder.add_method(sel!(acceptsFirstResponder), yes as extern "C-unwind" fn(_, _) -> _);
            builder.add_method(sel!(acceptsFirstMouse:), yes_for as extern "C-unwind" fn(_, _, _) -> _);
            for sel in [sel!(mouseDown:), sel!(rightMouseDown:), sel!(otherMouseDown:)] {
                builder.add_method(sel, mouse_down as extern "C-unwind" fn(_, _, _));
            }
            for sel in [sel!(mouseUp:), sel!(rightMouseUp:), sel!(otherMouseUp:)] {
                builder.add_method(sel, mouse_up as extern "C-unwind" fn(_, _, _));
            }
            for sel in [sel!(mouseMoved:), sel!(mouseDragged:), sel!(rightMouseDragged:), sel!(otherMouseDragged:)] {
                builder.add_method(sel, mouse_moved as extern "C-unwind" fn(_, _, _));
            }
            builder.add_method(sel!(scrollWheel:), scroll_wheel as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(keyDown:), key_down as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(keyUp:), key_up as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(becomeFirstResponder), become_first_responder as extern "C-unwind" fn(_, _) -> _);
            builder.add_method(sel!(resignFirstResponder), resign_first_responder as extern "C-unwind" fn(_, _) -> _);
            builder.add_method(sel!(viewDidChangeBackingProperties), backing_changed as extern "C-unwind" fn(_, _));
        }
        ViewClass { class: builder.register(), superclass }
    })
}

fn create<P: Process + PluginMeta, E: GpuEditor>(parent: *mut c_void, host: ViewHost, width: u32, height: u32) -> Option<Box<View>> {
    let parent = unsafe { (parent as *mut AnyObject).as_ref() }?;
    let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as f64, height as f64));
    let ns_view: Retained<AnyObject> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![view_class().class, alloc];
        msg_send![allocated, initWithFrame: frame]
    };
    unsafe {
        let _: () = msg_send![&*ns_view, setWantsLayer: Bool::YES];
        let tracking: Allocated<AnyObject> = msg_send![AnyClass::get(c"NSTrackingArea")?, alloc];
        let options = TRACKING_MOUSE_MOVED | TRACKING_ACTIVE_ALWAYS | TRACKING_IN_VISIBLE_RECT;
        let tracking: Retained<AnyObject> =
            msg_send![tracking, initWithRect: frame, options: options, owner: &*ns_view, userInfo: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![&*ns_view, addTrackingArea: &*tracking];
        let _: () = msg_send![parent, addSubview: &*ns_view];
    }
    let made = (|| {
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::AppKit(AppKitDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::AppKit(AppKitWindowHandle::new(NonNull::from(&*ns_view).cast())),
        };
        // SAFETY: the view outlives the surface (`close` drops the surface first).
        let surface = unsafe { instance().create_surface_unsafe(target) }.ok()?;
        let backing = backing_scale(&ns_view);
        Canvas::new::<P, E>(surface, host, (to_pixels(width, backing), to_pixels(height, backing)), backing)
    })();
    let Some(canvas) = made else {
        unsafe {
            let _: () = msg_send![&*ns_view, removeFromSuperview];
        }
        return None;
    };
    let mut view = Box::new(View { timer: None, canvas, points: (width, height), ns_view });
    let ptr: *mut View = &mut *view;
    VIEWS.with(|v| v.borrow_mut().insert(&*view.ns_view as *const AnyObject as usize, ptr));
    // SAFETY: the timer is the view's first field: it stops before the view goes.
    view.timer = UiTimer::new(TICK_MS, Box::new(move || unsafe { (*ptr).canvas.tick() }));
    view.layout();
    Some(view)
}

pub(crate) unsafe extern "C" fn open<P: Process + PluginMeta, E: GpuEditor>(
    parent: *mut c_void,
    host: *const ViewHost,
    width: u32,
    height: u32,
    _scale: f64,
) -> *mut c_void {
    open_view(parent, host, |host| create::<P, E>(parent, host, width, height))
}

/// `width` and `height` are points; the backing scale comes from the window.
pub(crate) unsafe extern "C" fn set_size(view: *mut c_void, width: u32, height: u32, _scale: f64) {
    let Some(view) = (unsafe { (view as *mut View).as_mut() }) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        view.points = (width, height);
        view.layout();
    }));
}

pub(crate) unsafe extern "C" fn close(view: *mut c_void) {
    if view.is_null() {
        return;
    }
    let mut view = unsafe { Box::from_raw(view as *mut View) };
    view.timer = None;
    VIEWS.with(|v| v.borrow_mut().remove(&(&*view.ns_view as *const AnyObject as usize)));
    let ns_view = view.ns_view.clone();
    view.canvas.close_editor();
    let _ = catch_unwind(AssertUnwindSafe(move || drop(view)));
    unsafe {
        let _: () = msg_send![&*ns_view, removeFromSuperview];
    }
}
