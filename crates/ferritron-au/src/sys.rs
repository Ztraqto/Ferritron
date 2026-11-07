//! The CoreFoundation and AudioToolbox declarations the adapter needs, from
//! Apple's public headers (no SDK crate: the C ABI is stable).

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type AudioUnit = *mut c_void;
pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFURLRef = *const c_void;

// --- Errors -------------------------------------------------------------------
pub const noErr: OSStatus = 0;
pub const kAudio_ParamError: OSStatus = -50;
pub const kAudioUnitErr_InvalidProperty: OSStatus = -10879;
pub const kAudioUnitErr_InvalidParameter: OSStatus = -10878;
pub const kAudioUnitErr_InvalidElement: OSStatus = -10877;
pub const kAudioUnitErr_NoConnection: OSStatus = -10876;
pub const kAudioUnitErr_FailedInitialization: OSStatus = -10875;
pub const kAudioUnitErr_TooManyFramesToProcess: OSStatus = -10874;
pub const kAudioUnitErr_FormatNotSupported: OSStatus = -10868;
pub const kAudioUnitErr_Uninitialized: OSStatus = -10867;
pub const kAudioUnitErr_InvalidScope: OSStatus = -10866;
pub const kAudioUnitErr_PropertyNotWritable: OSStatus = -10865;
pub const kAudioUnitErr_InvalidPropertyValue: OSStatus = -10851;
pub const kAudioUnitErr_Initialized: OSStatus = -10849;
pub const badComponentSelector: OSStatus = 0x8000_8002u32 as i32;

// --- Selectors ----------------------------------------------------------------
pub const kAudioUnitInitializeSelect: i16 = 0x0001;
pub const kAudioUnitUninitializeSelect: i16 = 0x0002;
pub const kAudioUnitGetPropertyInfoSelect: i16 = 0x0003;
pub const kAudioUnitGetPropertySelect: i16 = 0x0004;
pub const kAudioUnitSetPropertySelect: i16 = 0x0005;
pub const kAudioUnitGetParameterSelect: i16 = 0x0006;
pub const kAudioUnitSetParameterSelect: i16 = 0x0007;
pub const kAudioUnitResetSelect: i16 = 0x0009;
pub const kAudioUnitAddPropertyListenerSelect: i16 = 0x000A;
pub const kAudioUnitRemovePropertyListenerSelect: i16 = 0x000B;
pub const kAudioUnitRenderSelect: i16 = 0x000E;
pub const kAudioUnitAddRenderNotifySelect: i16 = 0x000F;
pub const kAudioUnitRemoveRenderNotifySelect: i16 = 0x0010;
pub const kAudioUnitScheduleParametersSelect: i16 = 0x0011;
pub const kAudioUnitRemovePropertyListenerWithUserDataSelect: i16 = 0x0012;
pub const kMusicDeviceMIDIEventSelect: i16 = 0x0101;
pub const kMusicDeviceSysExSelect: i16 = 0x0102;

// --- Properties, scopes, flags ----------------------------------------------------
pub const kAudioUnitProperty_ClassInfo: u32 = 0;
pub const kAudioUnitProperty_MakeConnection: u32 = 1;
pub const kAudioUnitProperty_SampleRate: u32 = 2;
pub const kAudioUnitProperty_ParameterList: u32 = 3;
pub const kAudioUnitProperty_ParameterInfo: u32 = 4;
pub const kAudioUnitProperty_StreamFormat: u32 = 8;
pub const kAudioUnitProperty_ElementCount: u32 = 11;
pub const kAudioUnitProperty_Latency: u32 = 12;
pub const kAudioUnitProperty_SupportedNumChannels: u32 = 13;
pub const kAudioUnitProperty_MaximumFramesPerSlice: u32 = 14;
pub const kAudioUnitProperty_ParameterValueStrings: u32 = 16;
pub const kAudioUnitProperty_TailTime: u32 = 20;
pub const kAudioUnitProperty_BypassEffect: u32 = 21;
pub const kAudioUnitProperty_LastRenderError: u32 = 22;
pub const kAudioUnitProperty_SetRenderCallback: u32 = 23;
pub const kAudioUnitProperty_HostCallbacks: u32 = 27;
pub const kAudioUnitProperty_InPlaceProcessing: u32 = 29;
pub const kAudioUnitProperty_CocoaUI: u32 = 31;
pub const kAudioUnitProperty_ParameterStringFromValue: u32 = 33;
pub const kAudioUnitProperty_PresentPreset: u32 = 36;
pub const kAudioUnitProperty_OfflineRender: u32 = 37;
pub const kAudioUnitProperty_ParameterValueFromString: u32 = 38;
pub const kAudioUnitProperty_ShouldAllocateBuffer: u32 = 51;
pub const kAudioUnitProperty_RenderContextObserver: u32 = 60;
/// Private (IDs from 64000 are free for audio units): the adapter's own
/// instance, for the Cocoa view factory.
pub const kFerritronProperty_Instance: u32 = 64_000 + 0x46;

pub const kAudioUnitScope_Global: u32 = 0;
pub const kAudioUnitScope_Input: u32 = 1;
pub const kAudioUnitScope_Output: u32 = 2;

pub const kAudioUnitRenderAction_PreRender: u32 = 1 << 2;
pub const kAudioUnitRenderAction_PostRender: u32 = 1 << 3;
pub const kAudioUnitRenderAction_OutputIsSilence: u32 = 1 << 4;

pub const kAudioFormatLinearPCM: u32 = u32::from_be_bytes(*b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

pub const kAudioUnitParameterUnit_Generic: u32 = 0;
pub const kAudioUnitParameterUnit_Indexed: u32 = 1;
pub const kAudioUnitParameterUnit_Boolean: u32 = 2;
pub const kAudioUnitParameterUnit_CustomUnit: u32 = 26;

pub const kAudioUnitParameterFlag_ValuesHaveStrings: u32 = 1 << 21;
pub const kAudioUnitParameterFlag_HasCFNameString: u32 = 1 << 27;
pub const kAudioUnitParameterFlag_NonRealTime: u32 = 1 << 24;
pub const kAudioUnitParameterFlag_IsReadable: u32 = 1 << 30;
pub const kAudioUnitParameterFlag_IsWritable: u32 = 1 << 31;

pub const kAudioUnitEvent_ParameterValueChange: u32 = 0;
pub const kAudioUnitEvent_BeginParameterChangeGesture: u32 = 1;
pub const kAudioUnitEvent_EndParameterChangeGesture: u32 = 2;
pub const kAUParameterListener_AnyParameter: u32 = 0xFFFF_FFFF;

pub const kParameterEvent_Immediate: u32 = 1;
pub const kParameterEvent_Ramped: u32 = 2;

// --- Structures -----------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioComponentDescription {
    pub componentType: u32,
    pub componentSubType: u32,
    pub componentManufacturer: u32,
    pub componentFlags: u32,
    pub componentFlagsMask: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// `AudioBufferList` with room for `N` buffers.
#[repr(C)]
pub struct AudioBufferList<const N: usize> {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; N],
}

/// The header only; buffers follow (`mBuffers[mNumberBuffers]`).
#[repr(C)]
pub struct AudioBufferListHeader {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

pub type AURenderCallback = unsafe extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferListHeader) -> OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AURenderCallbackStruct {
    pub inputProc: Option<AURenderCallback>,
    pub inputProcRefCon: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitConnection {
    pub sourceAudioUnit: AudioUnit,
    pub sourceOutputNumber: u32,
    pub destInputNumber: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AUChannelInfo {
    pub inChannels: i16,
    pub outChannels: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AUPreset {
    pub presetNumber: i32,
    pub presetName: CFStringRef,
}

#[repr(C)]
pub struct AudioUnitParameterInfo {
    pub name: [u8; 52],
    pub unitName: CFStringRef,
    pub clumpID: u32,
    pub cfNameString: CFStringRef,
    pub unit: u32,
    pub minValue: f32,
    pub maxValue: f32,
    pub defaultValue: f32,
    pub flags: u32,
}

#[repr(C)]
pub struct AudioUnitParameterStringFromValue {
    pub inParamID: u32,
    pub inValue: *const f32,
    pub outString: CFStringRef,
}

#[repr(C)]
pub struct AudioUnitParameterValueFromString {
    pub inParamID: u32,
    pub inString: CFStringRef,
    pub outValue: f32,
}

#[repr(C)]
pub struct AudioUnitCocoaViewInfo {
    pub mCocoaAUViewBundleLocation: CFURLRef,
    pub mCocoaAUViewClass: [CFStringRef; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameterEvent {
    pub scope: u32,
    pub element: u32,
    pub parameter: u32,
    pub eventType: u32,
    /// `ramp { startBufferOffset: i32, durationInFrames: u32, startValue: f32, endValue: f32 }`
    /// or `immediate { bufferOffset: u32, value: f32 }`.
    pub eventValues: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameter {
    pub mAudioUnit: AudioUnit,
    pub mParameterID: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
pub struct AudioUnitEvent {
    pub mEventType: u32,
    pub mArgument: AudioUnitParameter,
}

/// `HostCallbackInfo`, kept as the host gives it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostCallbackInfo {
    pub hostUserData: *mut c_void,
    pub callbacks: [*mut c_void; 4],
}

pub type AudioUnitPropertyListenerProc = unsafe extern "C" fn(*mut c_void, AudioUnit, u32, u32, u32);

/// `AudioComponentPlugInInterface`: the first field of every instance.
#[repr(C)]
pub struct AudioComponentPlugInInterface {
    pub Open: unsafe extern "C" fn(*mut c_void, *mut c_void) -> OSStatus,
    pub Close: unsafe extern "C" fn(*mut c_void) -> OSStatus,
    pub Lookup: unsafe extern "C" fn(i16) -> *const c_void,
    pub reserved: *mut c_void,
}

// --- Functions --------------------------------------------------------------------
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: isize = 3;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFTypeDictionaryKeyCallBacks: u8;
    pub static kCFTypeDictionaryValueCallBacks: u8;
    pub static kCFTypeArrayCallBacks: u8;
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
    pub fn CFDataGetTypeID() -> usize;
    pub fn CFDictionaryGetTypeID() -> usize;
    pub fn CFStringCreateWithBytes(alloc: *const c_void, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFStringRef;
    pub fn CFStringGetLength(s: CFStringRef) -> isize;
    pub fn CFStringGetMaximumSizeForEncoding(len: isize, encoding: u32) -> isize;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut u8, size: isize, encoding: u32) -> u8;
    pub fn CFDataCreate(alloc: *const c_void, bytes: *const u8, len: isize) -> CFDataRef;
    pub fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    pub fn CFDataGetLength(data: CFDataRef) -> isize;
    pub fn CFDictionaryCreateMutable(alloc: *const c_void, capacity: isize, keys: *const c_void, values: *const c_void) -> *mut c_void;
    pub fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFNumberCreate(alloc: *const c_void, kind: isize, value: *const c_void) -> CFNumberRef;
    pub fn CFArrayCreate(alloc: *const c_void, values: *const *const c_void, count: isize, callbacks: *const c_void) -> CFArrayRef;
    pub fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, path: *const u8, len: isize, directory: u8) -> CFURLRef;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    pub fn AudioUnitRender(unit: AudioUnit, flags: *mut u32, time: *const c_void, bus: u32, frames: u32, data: *mut AudioBufferListHeader) -> OSStatus;
    pub fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OSStatus;
    pub fn AUEventListenerNotify(listener: *mut c_void, object: *mut c_void, event: *const AudioUnitEvent) -> OSStatus;
    pub fn AUParameterListenerNotify(listener: *mut c_void, object: *mut c_void, parameter: *const AudioUnitParameter) -> OSStatus;
}

#[link(name = "System")]
unsafe extern "C" {
    pub static _dispatch_main_q: u8;
    pub fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: unsafe extern "C" fn(*mut c_void));
}

/// A new CFString (+1) from UTF-8.
pub fn cf_string(s: &str) -> CFStringRef {
    unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, kCFStringEncodingUTF8, 0) }
}

/// The text of a CFString.
pub fn rust_string(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe {
        let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0u8; size.max(1) as usize];
        if CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8) == 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }
}

pub fn cf_number(value: i32) -> CFNumberRef {
    unsafe { CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, &value as *const i32 as *const c_void) }
}

/// Runs `f` on the main thread, later.
pub fn on_main(f: Box<dyn FnOnce() + Send>) {
    unsafe extern "C" fn run(context: *mut c_void) {
        let f = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce() + Send>) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }
    let context = Box::into_raw(Box::new(f)) as *mut c_void;
    unsafe { dispatch_async_f(&_dispatch_main_q as *const u8 as *const c_void, context, run) };
}
