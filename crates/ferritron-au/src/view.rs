//! The Cocoa editor of an Audio Unit: the view factory (objc/view_factory.m)
//! asks `ferritron_au_view` for a view; it is a container NSView in which the
//! plugin's own editor (`ferritron.view.v1`: web or GPU) opens.
//!
//! The container closes the editor when it goes away (dealloc) or when the
//! instance closes first; host resizes reach the editor and are saved with
//! the state (`ferritron.editor-size.v1`).

use crate::au::Unit;
use crate::sys::*;
use ferritron_core::abi::ViewHost;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::SeqCst;

/// An open editor.
struct Editor {
    unit: *const Unit,
    container: usize,
    view: *mut c_void,
    host: ViewHost,
}

thread_local! {
    /// Editors by their container view. AppKit runs on the main thread.
    static EDITORS: RefCell<HashMap<usize, *mut Editor>> = RefCell::new(HashMap::new());
}

/// Where hosts find the view factory: this bundle and the class name.
pub(crate) fn cocoa_view_info() -> Option<AudioUnitCocoaViewInfo> {
    // <bundle>.component/Contents/MacOS/<binary> -> <bundle>.component
    let mut info: libc_dl::DlInfo = unsafe { std::mem::zeroed() };
    if unsafe { libc_dl::dladdr(cocoa_view_info as *const c_void, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    let binary = std::path::PathBuf::from(unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy().into_owned());
    let bundle = binary.parent()?.parent()?.parent()?;
    let path = bundle.as_os_str().as_encoded_bytes();
    let url = unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as isize, 1) };
    if url.is_null() {
        return None;
    }
    // Both are released by the host.
    Some(AudioUnitCocoaViewInfo { mCocoaAUViewBundleLocation: url, mCocoaAUViewClass: [cf_string(env!("FERRITRON_AU_VIEW_CLASS"))] })
}

mod libc_dl {
    use std::ffi::{c_char, c_void};

    #[repr(C)]
    pub struct DlInfo {
        pub dli_fname: *const c_char,
        pub dli_fbase: *mut c_void,
        pub dli_sname: *const c_char,
        pub dli_saddr: *mut c_void,
    }

    unsafe extern "C" {
        pub fn dladdr(address: *const c_void, info: *mut DlInfo) -> i32;
    }
}

unsafe extern "C" fn host_get(ctx: *mut c_void, id: u32) -> f64 {
    let unit = unsafe { &*(ctx as *const Unit) };
    unsafe { (unit.api.get_param)(unit.core(), id) }
}

unsafe extern "C" fn host_begin(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const Unit) }.notify(kAudioUnitEvent_BeginParameterChangeGesture, id);
}

unsafe extern "C" fn host_perform(ctx: *mut c_void, id: u32, normalized: f64) {
    unsafe { &*(ctx as *const Unit) }.edit(id, normalized);
}

unsafe extern "C" fn host_end(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const Unit) }.notify(kAudioUnitEvent_EndParameterChangeGesture, id);
}

/// Closes an editor and forgets it.
fn close(editor: *mut Editor) {
    let editor = unsafe { Box::from_raw(editor) };
    EDITORS.with(|e| e.borrow_mut().remove(&editor.container));
    let unit = unsafe { &*editor.unit };
    let _ = unit.editor.compare_exchange(&*editor as *const Editor as *mut c_void, std::ptr::null_mut(), SeqCst, SeqCst);
    if let Some(api) = unit.view_api
        && !editor.view.is_null()
    {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { (api.close)(editor.view) }));
    }
}

/// The instance closes: its editor goes first (the container may live on, empty).
pub(crate) fn detach(unit: &Unit) {
    let editor = unit.editor.swap(std::ptr::null_mut(), SeqCst) as *mut Editor;
    if !editor.is_null() {
        close(editor);
    }
}

/// State with an editor size was loaded: the container takes that size (the
/// host follows the view's frame).
pub(crate) fn restored(unit: &Unit) {
    let editor = unit.editor.load(SeqCst) as *mut Editor;
    let (Some(editor), Some(sizes)) = (unsafe { editor.as_ref() }, unit.size_api) else { return };
    let (mut w, mut h) = (0, 0);
    if unsafe { (sizes.get)(unit.core(), &mut w, &mut h) } {
        let container = editor.container as *mut AnyObject;
        unsafe {
            let _: () = msg_send![container, setFrameSize: CGSize::new(w as f64, h as f64)];
        }
    }
}

#[repr(C)]
struct ObjcSuper {
    receiver: *mut AnyObject,
    superclass: *const AnyClass,
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_msgSendSuper();
}

extern "C-unwind" fn is_flipped(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

/// The host resized the view: the editor follows, within its limits, and the
/// size is saved.
extern "C-unwind" fn set_frame_size(this: &AnyObject, _cmd: Sel, size: CGSize) {
    unsafe {
        let _: () = msg_send![super(this, container_class().1), setFrameSize: size];
    }
    let editor = EDITORS.with(|e| e.borrow().get(&(this as *const AnyObject as usize)).copied());
    let Some(editor) = editor.and_then(|e| unsafe { e.as_ref() }) else { return };
    let unit = unsafe { &*editor.unit };
    let Some(api) = unit.view_api else { return };
    let w = (size.width.round() as u32).clamp(api.min_width, api.max_width);
    let h = (size.height.round() as u32).clamp(api.min_height, api.max_height);
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { (api.set_size)(editor.view, w, h, 1.0) }));
    if let Some(sizes) = unit.size_api {
        unsafe { (sizes.set)(unit.core(), w, h) };
    }
}

extern "C-unwind" fn dealloc(this: &AnyObject, cmd: Sel) {
    let editor = EDITORS.with(|e| e.borrow().get(&(this as *const AnyObject as usize)).copied());
    if let Some(editor) = editor {
        close(editor);
    }
    let sup = ObjcSuper { receiver: this as *const AnyObject as *mut AnyObject, superclass: container_class().1 };
    let send: unsafe extern "C" fn(*const ObjcSuper, Sel) = unsafe { std::mem::transmute(objc_msgSendSuper as unsafe extern "C" fn()) };
    unsafe { send(&sup, cmd) };
}

/// The container class of this copy of the adapter (the name includes a
/// function's address, so plugins never share it).
fn container_class() -> (&'static AnyClass, &'static AnyClass) {
    static CLASS: OnceLock<(&'static AnyClass, &'static AnyClass)> = OnceLock::new();
    *CLASS.get_or_init(|| {
        let superclass = AnyClass::get(c"NSView").expect("AppKit is loaded");
        let name = CString::new(format!("FerritronAUContainer{:x}", dealloc as *const () as usize)).unwrap();
        let mut builder = ClassBuilder::new(&name, superclass).expect("the class name is unique to this library");
        unsafe {
            builder.add_method(sel!(isFlipped), is_flipped as extern "C-unwind" fn(_, _) -> _);
            builder.add_method(sel!(setFrameSize:), set_frame_size as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(dealloc), dealloc as extern "C-unwind" fn(_, _));
        }
        (builder.register(), superclass)
    })
}

/// Called by the view factory: a new (autoreleased) view with the editor of `au`.
#[unsafe(no_mangle)]
pub extern "C" fn ferritron_au_view(au: AudioUnit, _size: CGSize) -> *mut AnyObject {
    catch_unwind(AssertUnwindSafe(|| make_view(au))).ok().flatten().unwrap_or(std::ptr::null_mut())
}

fn make_view(au: AudioUnit) -> Option<*mut AnyObject> {
    let mut unit: *const Unit = std::ptr::null();
    let mut bytes = size_of::<*const Unit>() as u32;
    let status = unsafe {
        AudioUnitGetProperty(au, kFerritronProperty_Instance, kAudioUnitScope_Global, 0, &mut unit as *mut *const Unit as *mut c_void, &mut bytes)
    };
    let unit = if status == noErr { unsafe { unit.as_ref() }? } else { return None };
    let api = unit.view_api?;
    // One editor per instance: a new view replaces the old one's editor.
    detach(unit);
    let (mut w, mut h) = (api.width, api.height);
    if let Some(sizes) = unit.size_api {
        let (mut sw, mut sh) = (0, 0);
        if unsafe { (sizes.get)(unit.core(), &mut sw, &mut sh) } {
            (w, h) = (sw.clamp(api.min_width, api.max_width), sh.clamp(api.min_height, api.max_height));
        }
    }
    let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    let container: Retained<AnyObject> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![container_class().0, alloc];
        msg_send![allocated, initWithFrame: frame]
    };
    let editor = Box::into_raw(Box::new(Editor {
        unit,
        container: &*container as *const AnyObject as usize,
        view: std::ptr::null_mut(),
        host: ViewHost { ctx: unit as *const Unit as *mut c_void, get_param: host_get, begin_edit: host_begin, perform_edit: host_perform, end_edit: host_end },
    }));
    let parent = &*container as *const AnyObject as *mut c_void;
    let view = unsafe { (api.open)(parent, &(*editor).host, w, h, 1.0) };
    unsafe { (*editor).view = view };
    EDITORS.with(|e| e.borrow_mut().insert(unsafe { (*editor).container }, editor));
    unit.editor.store(editor as *mut c_void, SeqCst);
    Some(Retained::autorelease_return(container))
}
