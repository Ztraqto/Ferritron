//! The parts of the LV2 C API this adapter uses (LV2 1.18, ISC license),
//! written out: LV2 is a plain C interface with no library to link.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_void};

pub type LV2_Handle = *mut c_void;
pub type LV2_URID = u32;

#[repr(C)]
pub struct LV2_Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct LV2_Descriptor {
    pub uri: *const c_char,
    pub instantiate: unsafe extern "C" fn(*const LV2_Descriptor, f64, *const c_char, *const *const LV2_Feature) -> LV2_Handle,
    pub connect_port: unsafe extern "C" fn(LV2_Handle, u32, *mut c_void),
    pub activate: Option<unsafe extern "C" fn(LV2_Handle)>,
    pub run: unsafe extern "C" fn(LV2_Handle, u32),
    pub deactivate: Option<unsafe extern "C" fn(LV2_Handle)>,
    pub cleanup: unsafe extern "C" fn(LV2_Handle),
    pub extension_data: unsafe extern "C" fn(*const c_char) -> *const c_void,
}

#[repr(C)]
pub struct LV2_URID_Map {
    pub handle: *mut c_void,
    pub map: unsafe extern "C" fn(*mut c_void, *const c_char) -> LV2_URID,
}

#[repr(C)]
pub struct LV2_Options_Option {
    pub context: u32,
    pub subject: u32,
    pub key: LV2_URID,
    pub size: u32,
    pub kind: LV2_URID,
    pub value: *const c_void,
}

#[repr(C)]
pub struct LV2_Atom {
    pub size: u32,
    pub kind: u32,
}

/// `LV2_Atom_Sequence`: the atom header, then `unit` and `pad`, then events.
#[repr(C)]
pub struct LV2_Atom_Sequence {
    pub atom: LV2_Atom,
    pub unit: u32,
    pub pad: u32,
}

/// `LV2_Atom_Event`: the time in frames, then the body atom (and its data).
#[repr(C)]
pub struct LV2_Atom_Event {
    pub frames: i64,
    pub body: LV2_Atom,
}

pub type LV2_State_Handle = *mut c_void;
pub type LV2_State_Store_Function = unsafe extern "C" fn(LV2_State_Handle, u32, *const c_void, usize, u32, u32) -> u32;
pub type LV2_State_Retrieve_Function = unsafe extern "C" fn(LV2_State_Handle, u32, *mut usize, *mut u32, *mut u32) -> *const c_void;

#[repr(C)]
pub struct LV2_State_Interface {
    pub save: unsafe extern "C" fn(LV2_Handle, LV2_State_Store_Function, LV2_State_Handle, u32, *const *const LV2_Feature) -> u32,
    pub restore: unsafe extern "C" fn(LV2_Handle, LV2_State_Retrieve_Function, LV2_State_Handle, u32, *const *const LV2_Feature) -> u32,
}

pub const LV2_STATE_SUCCESS: u32 = 0;
pub const LV2_STATE_ERR_UNKNOWN: u32 = 1;
pub const LV2_STATE_ERR_NO_PROPERTY: u32 = 5;
pub const LV2_STATE_IS_POD: u32 = 1;
pub const LV2_STATE_IS_PORTABLE: u32 = 2;

pub type LV2UI_Handle = *mut c_void;
pub type LV2UI_Controller = *mut c_void;
pub type LV2UI_Write_Function = unsafe extern "C" fn(LV2UI_Controller, u32, u32, u32, *const c_void);

#[repr(C)]
pub struct LV2UI_Descriptor {
    pub uri: *const c_char,
    pub instantiate: unsafe extern "C" fn(
        *const LV2UI_Descriptor,
        *const c_char,
        *const c_char,
        LV2UI_Write_Function,
        LV2UI_Controller,
        *mut *mut c_void,
        *const *const LV2_Feature,
    ) -> LV2UI_Handle,
    pub cleanup: unsafe extern "C" fn(LV2UI_Handle),
    pub port_event: Option<unsafe extern "C" fn(LV2UI_Handle, u32, u32, u32, *const c_void)>,
    pub extension_data: Option<unsafe extern "C" fn(*const c_char) -> *const c_void>,
}

#[repr(C)]
pub struct LV2UI_Resize {
    pub handle: *mut c_void,
    pub ui_resize: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
}

#[repr(C)]
pub struct LV2UI_Touch {
    pub handle: *mut c_void,
    pub touch: unsafe extern "C" fn(*mut c_void, u32, bool),
}

#[repr(C)]
pub struct LV2UI_Idle_Interface {
    pub idle: unsafe extern "C" fn(LV2UI_Handle) -> c_int,
}

pub const URID_MAP: &str = "http://lv2plug.in/ns/ext/urid#map";
pub const OPTIONS: &str = "http://lv2plug.in/ns/ext/options#options";
pub const MAX_BLOCK_LENGTH: &str = "http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
pub const NOMINAL_BLOCK_LENGTH: &str = "http://lv2plug.in/ns/ext/buf-size#nominalBlockLength";
pub const ATOM_INT: &str = "http://lv2plug.in/ns/ext/atom#Int";
pub const ATOM_FLOAT: &str = "http://lv2plug.in/ns/ext/atom#Float";
pub const ATOM_CHUNK: &str = "http://lv2plug.in/ns/ext/atom#Chunk";
pub const MIDI_EVENT: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";
pub const STATE_INTERFACE: &str = "http://lv2plug.in/ns/ext/state#interface";
pub const INSTANCE_ACCESS: &str = "http://lv2plug.in/ns/ext/instance-access";
pub const UI_PARENT: &str = "http://lv2plug.in/ns/extensions/ui#parent";
pub const UI_RESIZE: &str = "http://lv2plug.in/ns/extensions/ui#resize";
pub const UI_TOUCH: &str = "http://lv2plug.in/ns/extensions/ui#touch";
pub const UI_IDLE: &str = "http://lv2plug.in/ns/extensions/ui#idleInterface";
pub const UI_SCALE_FACTOR: &str = "http://lv2plug.in/ns/extensions/ui#scaleFactor";

/// The feature with `uri`, if the host passed it.
///
/// # Safety
/// `features` is a host's null-terminated feature array (or null).
pub unsafe fn feature(features: *const *const LV2_Feature, uri: &str) -> Option<*mut c_void> {
    if features.is_null() {
        return None;
    }
    let mut at = features;
    loop {
        let f = unsafe { *at };
        if f.is_null() {
            return None;
        }
        let f = unsafe { &*f };
        if !f.uri.is_null() && unsafe { std::ffi::CStr::from_ptr(f.uri) }.to_bytes() == uri.as_bytes() {
            return Some(f.data);
        }
        at = unsafe { at.add(1) };
    }
}

/// Maps `uri` with the host's map (0 without one).
pub fn map(urid: Option<&LV2_URID_Map>, uri: &str) -> LV2_URID {
    let Some(urid) = urid else { return 0 };
    let Ok(c) = std::ffi::CString::new(uri) else { return 0 };
    unsafe { (urid.map)(urid.handle, c.as_ptr()) }
}
