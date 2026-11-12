//! The LV2 UI (`urn:ferritron:<id>#ui`): the plugin's editor embedded in the
//! host's window (`ui:parent`), on Windows (`ui:WindowsUI`) and macOS
//! (`ui:CocoaUI`). Edits go to the host through the control ports
//! (`write_function`), gestures through `ui:touch`.

use crate::sys::*;
use crate::{Instance, api, param_ids};
use ferritron_core::abi::{EXT_EDITOR_SIZE, EXT_VIEW, EditorSizeApi, PluginApi, ViewApi, ViewHost};
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

pub(crate) struct Ui {
    api: &'static PluginApi,
    /// The plugin instance (`instance-access`), or null.
    dsp: *mut Instance,
    ids: Vec<u32>,
    write: LV2UI_Write_Function,
    controller: LV2UI_Controller,
    touch: Option<(*mut c_void, unsafe extern "C" fn(*mut c_void, u32, bool))>,
    /// Normalized values from `port_event`, for editors without their instance.
    values: Vec<f64>,
    view_api: &'static ViewApi,
    view: *mut c_void,
    host: Box<ViewHost>,
    scale: f64,
}

impl Ui {
    fn port(&self, id: u32) -> Option<u32> {
        self.ids.iter().position(|&i| i == id).map(|p| p as u32)
    }

    fn get(&self, id: u32) -> f64 {
        if let Some(dsp) = unsafe { self.dsp.as_ref() } {
            return unsafe { (self.api.get_param)(dsp.instance, id) };
        }
        self.port(id).map_or(0.0, |p| self.values[p as usize])
    }

    fn touch(&self, id: u32, grabbed: bool) {
        if let (Some((handle, touch)), Some(port)) = (self.touch, self.port(id)) {
            unsafe { touch(handle, port, grabbed) };
        }
    }

    fn perform(&mut self, id: u32, normalized: f64) {
        let Some(port) = self.port(id) else { return };
        self.values[port as usize] = normalized;
        if let Some(dsp) = unsafe { self.dsp.as_ref() } {
            unsafe { (self.api.set_param)(dsp.instance, id, normalized) };
        }
        let plain = unsafe { (self.api.to_plain)(id, normalized) } as f32;
        unsafe { (self.write)(self.controller, port, 4, 0, (&raw const plain).cast()) };
    }
}

// The view host, when the editor reaches its instance: ctx is the instance
// (the same pointer it was attached with), which knows its open UI.
fn ui_of_instance(ctx: *mut c_void) -> Option<&'static mut Ui> {
    let dsp = unsafe { (ctx as *mut Instance).as_ref() }?;
    unsafe { dsp.ui.load(Ordering::Acquire).as_mut() }
}

unsafe extern "C" fn dsp_get(ctx: *mut c_void, id: u32) -> f64 {
    let dsp = unsafe { &*(ctx as *const Instance) };
    unsafe { (dsp.api.get_param)(dsp.instance, id) }
}
unsafe extern "C" fn dsp_begin(ctx: *mut c_void, id: u32) {
    ui_of_instance(ctx).inspect(|ui| ui.touch(id, true));
}
unsafe extern "C" fn dsp_perform(ctx: *mut c_void, id: u32, normalized: f64) {
    if let Some(ui) = ui_of_instance(ctx) {
        ui.perform(id, normalized);
    }
}
unsafe extern "C" fn dsp_end(ctx: *mut c_void, id: u32) {
    ui_of_instance(ctx).inspect(|ui| ui.touch(id, false));
}

// The view host without instance access: ctx is the UI.
unsafe extern "C" fn ui_get(ctx: *mut c_void, id: u32) -> f64 {
    unsafe { &*(ctx as *const Ui) }.get(id)
}
unsafe extern "C" fn ui_begin(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const Ui) }.touch(id, true);
}
unsafe extern "C" fn ui_perform(ctx: *mut c_void, id: u32, normalized: f64) {
    unsafe { &mut *(ctx as *mut Ui) }.perform(id, normalized);
}
unsafe extern "C" fn ui_end(ctx: *mut c_void, id: u32) {
    unsafe { &*(ctx as *const Ui) }.touch(id, false);
}

struct Descriptor(LV2UI_Descriptor);
// SAFETY: a static table of function pointers and a leaked string.
unsafe impl Sync for Descriptor {}
unsafe impl Send for Descriptor {}

/// The LV2 UI entry point (plugins with an editor, on Windows and macOS).
#[unsafe(no_mangle)]
pub extern "C" fn lv2ui_descriptor(index: u32) -> *const LV2UI_Descriptor {
    static DESCRIPTOR: OnceLock<Option<Descriptor>> = OnceLock::new();
    if index != 0 || !cfg!(any(windows, target_os = "macos")) {
        return std::ptr::null();
    }
    let descriptor = DESCRIPTOR.get_or_init(|| {
        let api = api()?;
        let view = unsafe { (api.get_extension)(EXT_VIEW.as_ptr()) };
        if view.is_null() {
            return None;
        }
        let uri = CString::new(format!("{}#ui", crate::plugin_uri(api))).ok()?;
        Some(Descriptor(LV2UI_Descriptor {
            uri: Box::leak(uri.into_boxed_c_str()).as_ptr(),
            instantiate,
            cleanup,
            port_event: Some(port_event),
            extension_data: Some(extension_data),
        }))
    });
    descriptor.as_ref().map_or(std::ptr::null(), |d| &d.0)
}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;
    unsafe extern "system" {
        fn GetDpiForWindow(window: *mut c_void) -> u32;
        fn GetWindow(window: *mut c_void, command: u32) -> *mut c_void;
    }
    const GW_CHILD: u32 = 5;

    pub fn scale(parent: *mut c_void) -> f64 {
        match unsafe { GetDpiForWindow(parent) } {
            0 => 1.0,
            dpi => dpi as f64 / 96.0,
        }
    }

    /// The editor's window: the newest child of the parent.
    pub fn widget(parent: *mut c_void) -> *mut c_void {
        let child = unsafe { GetWindow(parent, GW_CHILD) };
        if child.is_null() { parent } else { child }
    }
}

#[cfg(not(windows))]
mod native {
    use std::ffi::c_void;
    pub fn scale(_parent: *mut c_void) -> f64 {
        1.0
    }
    pub fn widget(parent: *mut c_void) -> *mut c_void {
        parent
    }
}

unsafe extern "C" fn instantiate(
    _descriptor: *const LV2UI_Descriptor,
    _plugin_uri: *const c_char,
    _bundle: *const c_char,
    write: LV2UI_Write_Function,
    controller: LV2UI_Controller,
    widget: *mut *mut c_void,
    features: *const *const LV2_Feature,
) -> LV2UI_Handle {
    let Some(api) = api() else { return std::ptr::null_mut() };
    let view_api = unsafe { ((api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi).as_ref() };
    let (Some(view_api), Some(parent)) = (view_api, unsafe { feature(features, UI_PARENT) }) else { return std::ptr::null_mut() };
    let dsp = unsafe { feature(features, INSTANCE_ACCESS) }.unwrap_or(std::ptr::null_mut()) as *mut Instance;
    let touch = unsafe { feature(features, UI_TOUCH) }.and_then(|t| unsafe { (t as *const LV2UI_Touch).as_ref() }).map(|t| (t.handle, t.touch));
    let resize = unsafe { feature(features, UI_RESIZE) }.and_then(|r| unsafe { (r as *const LV2UI_Resize).as_ref() });
    let urid = unsafe { feature(features, URID_MAP) }.and_then(|m| unsafe { (m as *const LV2_URID_Map).as_ref() });
    let mut scale = native::scale(parent);
    if let Some(options) = unsafe { feature(features, OPTIONS) } {
        let (key, float) = (map(urid, UI_SCALE_FACTOR), map(urid, ATOM_FLOAT));
        let mut at = options as *const LV2_Options_Option;
        while let Some(o) = unsafe { at.as_ref() }.filter(|o| o.key != 0) {
            if o.key == key && o.kind == float && !o.value.is_null() {
                scale = unsafe { *(o.value as *const f32) } as f64;
            }
            at = unsafe { at.add(1) };
        }
    }
    let ids = param_ids(api);
    let values = ids.iter().map(|&id| unsafe { (api.to_normalized)(id, 0.0) }).collect();
    let mut ui = Box::new(Ui {
        api,
        dsp,
        ids,
        write,
        controller,
        touch,
        values,
        view_api,
        view: std::ptr::null_mut(),
        host: Box::new(ViewHost { ctx: std::ptr::null_mut(), get_param: ui_get, begin_edit: ui_begin, perform_edit: ui_perform, end_edit: ui_end, request_resize: None }),
        scale,
    });
    let ui_ptr: *mut Ui = &raw mut *ui;
    *ui.host = if let Some(dsp) = unsafe { dsp.as_ref() } {
        dsp.ui.store(ui_ptr, Ordering::Release);
        ViewHost { ctx: dsp as *const Instance as *mut c_void, get_param: dsp_get, begin_edit: dsp_begin, perform_edit: dsp_perform, end_edit: dsp_end, request_resize: None }
    } else {
        ViewHost { ctx: ui_ptr.cast(), get_param: ui_get, begin_edit: ui_begin, perform_edit: ui_perform, end_edit: ui_end, request_resize: None }
    };
    // The size the user chose last time (saved with the instance), else the default.
    let (mut width, mut height) = (view_api.width, view_api.height);
    if let Some(dsp) = unsafe { dsp.as_ref() } {
        let sizes = unsafe { ((api.get_extension)(EXT_EDITOR_SIZE.as_ptr()) as *const EditorSizeApi).as_ref() };
        if let Some(sizes) = sizes {
            let (mut w, mut h) = (0, 0);
            if unsafe { (sizes.get)(dsp.instance, &mut w, &mut h) } {
                (width, height) = (w, h);
            }
        }
    }
    let (pw, ph) = ((width as f64 * scale).round() as u32, (height as f64 * scale).round() as u32);
    ui.view = unsafe { (view_api.open)(parent, &*ui.host, pw, ph, scale) };
    if ui.view.is_null() {
        if let Some(dsp) = unsafe { dsp.as_ref() } {
            dsp.ui.store(std::ptr::null_mut(), Ordering::Release);
        }
        return std::ptr::null_mut();
    }
    if let Some(resize) = resize {
        unsafe { (resize.ui_resize)(resize.handle, pw as c_int, ph as c_int) };
    }
    if !widget.is_null() {
        unsafe { *widget = native::widget(parent) };
    }
    Box::into_raw(ui).cast()
}

unsafe extern "C" fn cleanup(handle: LV2UI_Handle) {
    let ui = unsafe { Box::from_raw(handle as *mut Ui) };
    if let Some(dsp) = unsafe { ui.dsp.as_ref() } {
        let _ = dsp.ui.compare_exchange(handle.cast(), std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
    }
    unsafe { (ui.view_api.close)(ui.view) };
}

unsafe extern "C" fn port_event(handle: LV2UI_Handle, port: u32, size: u32, format: u32, buffer: *const c_void) {
    let ui = unsafe { &mut *(handle as *mut Ui) };
    if format != 0 || size != 4 || buffer.is_null() {
        return;
    }
    if let Some(&id) = ui.ids.get(port as usize) {
        let plain = unsafe { *(buffer as *const f32) } as f64;
        ui.values[port as usize] = unsafe { (ui.api.to_normalized)(id, plain) };
    }
}

unsafe extern "C" fn idle(_handle: LV2UI_Handle) -> c_int {
    0 // the editor runs on the host's own event loop
}

/// `ui:resize` offered by the UI: the host resizes the editor.
unsafe extern "C" fn resize(handle: *mut c_void, width: c_int, height: c_int) -> c_int {
    let ui = unsafe { &*(handle as *const Ui) };
    unsafe { (ui.view_api.set_size)(ui.view, width.max(1) as u32, height.max(1) as u32, ui.scale) };
    if let Some(dsp) = unsafe { ui.dsp.as_ref() } {
        let sizes = unsafe { ((ui.api.get_extension)(EXT_EDITOR_SIZE.as_ptr()) as *const EditorSizeApi).as_ref() };
        if let Some(sizes) = sizes {
            let logical = |px: c_int| (px.max(1) as f64 / ui.scale).round() as u32;
            unsafe { (sizes.set)(dsp.instance, logical(width), logical(height)) };
        }
    }
    0
}

static IDLE: LV2UI_Idle_Interface = LV2UI_Idle_Interface { idle };

struct Shared<T>(T);
// SAFETY: a null handle and a function pointer.
unsafe impl<T> Sync for Shared<T> {}
static RESIZE: Shared<LV2UI_Resize> = Shared(LV2UI_Resize { handle: std::ptr::null_mut(), ui_resize: resize });

unsafe extern "C" fn extension_data(uri: *const c_char) -> *const c_void {
    if uri.is_null() {
        return std::ptr::null();
    }
    match unsafe { std::ffi::CStr::from_ptr(uri) }.to_bytes() {
        u if u == UI_IDLE.as_bytes() => (&raw const IDLE).cast(),
        u if u == UI_RESIZE.as_bytes() => (&raw const RESIZE.0).cast(),
        _ => std::ptr::null(),
    }
}
