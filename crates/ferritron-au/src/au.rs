//! The Audio Unit (v2) itself: an `AudioComponentPlugInInterface` whose
//! selectors map onto the format-independent ABI (`ferritron_abi.h`).
//!
//! - Effects have one input and one output bus, instruments one output;
//!   mono or stereo, 32-bit float, non-interleaved (the canonical format).
//! - Parameters are plain values (dB, Hz, an index for choices), shown with
//!   the plugin's own text (`ValuesHaveStrings`).
//! - State is the plugin's state inside the `ClassInfo` dictionary.
//! - The host's Audio Workgroup (render context observer, macOS 13) goes to
//!   Ferritron's worker threads.

// Apple's names, as in the headers.
#![allow(non_upper_case_globals)]

use crate::sys::*;
use ferritron_core::abi::{
    EVENT_MIDI, EVENT_NOTE_OFF, EVENT_NOTE_ON, EVENT_PARAM, EXT_EDITOR_SIZE, EXT_HOST, EXT_LATENCY, EXT_RENDER, EXT_VIEW, EditorSizeApi,
    Event, EventData, HostApi, LatencyApi, NoteData, ParamData, ParamInfoC, PluginApi, RenderApi, ViewApi,
};
use std::cell::UnsafeCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize};

const MAX_CHANNELS: usize = 2;
const MAX_EVENTS: usize = 1024;
/// Events the plugin may write per block (the `out_events` the render passes).
const MAX_OUT: usize = 256;
/// Parameter edits the render thread has reported and the main thread has not told the host yet.
const PARAM_EDITS: usize = 256;
/// What hosts assume until they set kAudioUnitProperty_MaximumFramesPerSlice.
const DEFAULT_MAX_FRAMES: u32 = 1156;

/// Instances alive, for work queued to the main thread.
static LIVE: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

fn is_live(unit: *const Unit) -> bool {
    LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().is_some_and(|l| l.contains(&(unit as usize)))
}

/// What hosts need to know about one parameter, made once.
struct ParamMeta {
    id: u32,
    name: String,
    cf_name: CFStringRef,
    cf_unit: CFStringRef,
    unit: u32,
    min: f32,
    max: f32,
    default: f32,
    flags: u32,
    bypass: bool,
    /// Choice names (indexed parameters).
    values: CFArrayRef,
}

/// Events for the next block, from any thread (parameter schedules, MIDI).
struct EventQueue {
    lock: AtomicBool,
    events: UnsafeCell<Vec<Event>>,
}

impl EventQueue {
    fn new() -> Self {
        Self { lock: AtomicBool::new(false), events: UnsafeCell::new(Vec::with_capacity(MAX_EVENTS)) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<Event>) -> R) -> R {
        while self.lock.swap(true, Acquire) {
            std::hint::spin_loop();
        }
        let r = f(unsafe { &mut *self.events.get() });
        self.lock.store(false, Release);
        r
    }

    fn push(&self, event: Event) {
        self.with(|events| {
            if events.len() < events.capacity() {
                events.push(event);
            }
        });
    }
}

/// Parameter IDs the plugin changed on the render thread, waiting for the main
/// thread to report them to the host. Single producer (the render thread), single
/// consumer (`drain_param_edits`, on the main thread); no locks.
struct ParamEdits {
    ids: [AtomicU32; PARAM_EDITS],
    read: AtomicUsize,
    write: AtomicUsize,
    /// A drain is queued on the main thread; the render thread queues at most one.
    scheduled: AtomicBool,
}

impl ParamEdits {
    fn new() -> Self {
        Self { ids: std::array::from_fn(|_| AtomicU32::new(0)), read: AtomicUsize::new(0), write: AtomicUsize::new(0), scheduled: AtomicBool::new(false) }
    }

    /// Render thread. Returns false (the edit is not reported) when the queue is full.
    fn push(&self, id: u32) -> bool {
        let (w, r) = (self.write.load(Relaxed), self.read.load(Acquire));
        let next = (w + 1) % PARAM_EDITS;
        if next == r {
            return false;
        }
        self.ids[w].store(id, Relaxed);
        self.write.store(next, Release);
        true
    }

    /// Main thread.
    fn pop(&self) -> Option<u32> {
        let (r, w) = (self.read.load(Relaxed), self.write.load(Acquire));
        if r == w {
            return None;
        }
        let id = self.ids[r].load(Relaxed);
        self.read.store((r + 1) % PARAM_EDITS, Release);
        Some(id)
    }
}

/// Render-thread state: buffers sized in `Initialize`.
struct RenderState {
    output: Vec<Vec<f32>>,
    input: Vec<Vec<f32>>,
    events: Vec<Event>,
    /// Room for sorting `events` without allocating.
    sort_scratch: Vec<Event>,
    /// The events the plugin writes (`out_events`): MIDI out and parameter edits.
    out: Vec<Event>,
}

#[repr(C)]
pub(crate) struct Unit {
    /// Must stay the first field: the host calls through it.
    interface: AudioComponentPlugInInterface,
    pub(crate) component: AtomicPtr<c_void>,
    desc: AudioComponentDescription,
    pub(crate) api: &'static PluginApi,
    pub(crate) core: AtomicPtr<c_void>,
    instrument: bool,
    params: Vec<ParamMeta>,
    sample_rate: AtomicU64,
    channels: AtomicU32,
    max_frames: AtomicU32,
    initialized: AtomicBool,
    in_place: AtomicU32,
    allocate: AtomicU32,
    callback: Mutex<Option<AURenderCallbackStruct>>,
    /// The host's MIDI output (`kAudioUnitProperty_MIDIOutputCallback`), called from the render thread.
    midi_out: Mutex<Option<AUMIDIOutputCallbackStruct>>,
    /// Parameter edits to report to the host from the main thread.
    param_edits: ParamEdits,
    connection: Mutex<Option<AudioUnitConnection>>,
    listeners: Mutex<Vec<(u32, AudioUnitPropertyListenerProc, usize)>>,
    notifies: Mutex<Vec<(AURenderCallback, usize)>>,
    events: EventQueue,
    render: UnsafeCell<RenderState>,
    preset: Mutex<(i32, String)>,
    host_callbacks: Mutex<Option<HostCallbackInfo>>,
    observer: block2::RcBlock<dyn Fn(*const c_void)>,
    link: Box<ferritron_core::mcp::HostCallbacks>,
    pub(crate) view_api: Option<&'static ViewApi>,
    pub(crate) size_api: Option<&'static EditorSizeApi>,
    latency_api: Option<&'static LatencyApi>,
    render_api: Option<&'static RenderApi>,
    /// The open editor (view.rs), closed with the instance.
    pub(crate) editor: AtomicPtr<c_void>,
}

// SAFETY: the host serializes calls per thread role; shared fields are atomics or locks.
unsafe impl Send for Unit {}
unsafe impl Sync for Unit {}

fn extension<T>(api: &PluginApi, id: &std::ffi::CStr) -> Option<&'static T> {
    unsafe { ((api.get_extension)(id.as_ptr()) as *const T).as_ref() }
}

fn params_of(api: &'static PluginApi) -> Vec<ParamMeta> {
    let count = unsafe { (api.param_count)() };
    (0..count)
        .filter_map(|index| {
            let mut info = ParamInfoC { id: 0, name: ferritron_core::abi::FerritronStr::new(""), unit: ferritron_core::abi::FerritronStr::new(""), step_count: 0, default_normalized: 0.0, flags: 0 };
            if !unsafe { (api.param_info)(index, &mut info) } {
                return None;
            }
            let text = |s: ferritron_core::abi::FerritronStr| unsafe { s.to_str() }.unwrap_or("").to_string();
            let (name, unit_text) = (text(info.name), text(info.unit));
            let (min, max) = unsafe { ((api.to_plain)(info.id, 0.0) as f32, (api.to_plain)(info.id, 1.0) as f32) };
            let default = unsafe { (api.to_plain)(info.id, info.default_normalized) } as f32;
            let list = info.flags & 1 != 0;
            let (unit, values) = if list {
                let names: Vec<CFStringRef> =
                    (0..=info.step_count).map(|i| cf_string(&format_value(api, info.id, i as f64 / info.step_count.max(1) as f64))).collect();
                let array = unsafe { CFArrayCreate(std::ptr::null(), names.as_ptr(), names.len() as isize, &kCFTypeArrayCallBacks as *const u8 as *const c_void) };
                for n in names {
                    unsafe { CFRelease(n) };
                }
                (kAudioUnitParameterUnit_Indexed, array)
            } else if info.step_count == 1 && min == 0.0 && max == 1.0 {
                (kAudioUnitParameterUnit_Boolean, std::ptr::null())
            } else if !unit_text.is_empty() && info.step_count == 0 {
                (kAudioUnitParameterUnit_CustomUnit, std::ptr::null())
            } else {
                (kAudioUnitParameterUnit_Generic, std::ptr::null())
            };
            let mut flags = kAudioUnitParameterFlag_IsReadable
                | kAudioUnitParameterFlag_IsWritable
                | kAudioUnitParameterFlag_HasCFNameString
                | kAudioUnitParameterFlag_ValuesHaveStrings;
            if info.flags & 4 != 0 {
                flags |= kAudioUnitParameterFlag_NonRealTime; // not for automation
            }
            Some(ParamMeta {
                id: info.id,
                cf_name: cf_string(&name),
                cf_unit: if unit_text.is_empty() { std::ptr::null() } else { cf_string(&unit_text) },
                name,
                unit,
                min,
                max,
                default,
                flags,
                bypass: info.flags & 2 != 0,
                values,
            })
        })
        .collect()
}

fn format_value(api: &PluginApi, id: u32, normalized: f64) -> String {
    let mut buf = [0u8; 256];
    let len = unsafe { (api.format_param)(id, normalized, buf.as_mut_ptr(), buf.len() as u32) };
    if len < 0 { String::new() } else { String::from_utf8_lossy(&buf[..(len as usize).min(buf.len())]).into_owned() }
}

impl Unit {
    fn new(api: &'static PluginApi, desc: AudioComponentDescription) -> Box<Self> {
        let instrument = unsafe { (*api.info).kind } == 1;
        let observer = block2::RcBlock::new(|context: *const c_void| {
            // AudioUnitRenderContext starts with the workgroup.
            let workgroup = if context.is_null() { std::ptr::null_mut() } else { unsafe { *(context as *const *mut c_void) } };
            unsafe { ferritron_core::__private::set_audio_workgroup(workgroup) };
        });
        Box::new(Self {
            interface: AudioComponentPlugInInterface { Open: open, Close: close, Lookup: lookup, reserved: std::ptr::null_mut() },
            component: AtomicPtr::new(std::ptr::null_mut()),
            desc,
            api,
            core: AtomicPtr::new(std::ptr::null_mut()),
            instrument,
            params: params_of(api),
            sample_rate: AtomicU64::new(44_100f64.to_bits()),
            channels: AtomicU32::new(2),
            max_frames: AtomicU32::new(DEFAULT_MAX_FRAMES),
            initialized: AtomicBool::new(false),
            in_place: AtomicU32::new(1),
            allocate: AtomicU32::new(1),
            callback: Mutex::new(None),
            midi_out: Mutex::new(None),
            param_edits: ParamEdits::new(),
            connection: Mutex::new(None),
            listeners: Mutex::new(Vec::new()),
            notifies: Mutex::new(Vec::new()),
            events: EventQueue::new(),
            render: UnsafeCell::new(RenderState {
                output: Vec::new(),
                input: Vec::new(),
                events: Vec::with_capacity(MAX_EVENTS),
                sort_scratch: vec![ferritron_core::abi::Event::midi(0, 0, 0, 0, 0); MAX_EVENTS],
                out: vec![Event::param(0, 0, 0.0); MAX_OUT],
            }),
            preset: Mutex::new((-1, "Untitled".into())),
            host_callbacks: Mutex::new(None),
            observer,
            link: Box::new(ferritron_core::mcp::HostCallbacks { ctx: std::ptr::null_mut(), edited: None }),
            view_api: extension(api, EXT_VIEW),
            size_api: extension(api, EXT_EDITOR_SIZE),
            latency_api: extension(api, EXT_LATENCY),
            render_api: extension(api, EXT_RENDER),
            editor: AtomicPtr::new(std::ptr::null_mut()),
        })
    }

    pub(crate) fn core(&self) -> *mut c_void {
        self.core.load(Relaxed)
    }

    fn param(&self, id: u32) -> Option<&ParamMeta> {
        self.params.iter().find(|p| p.id == id)
    }

    fn sample_rate(&self) -> f64 {
        f64::from_bits(self.sample_rate.load(Relaxed))
    }

    fn format(&self) -> AudioStreamBasicDescription {
        let channels = self.channels.load(Relaxed);
        AudioStreamBasicDescription {
            mSampleRate: self.sample_rate(),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    /// The AU value (plain) of a parameter.
    pub(crate) fn value(&self, id: u32) -> f32 {
        unsafe { (self.api.to_plain)(id, (self.api.get_param)(self.core(), id)) as f32 }
    }

    fn normalized(&self, id: u32, value: f32) -> f64 {
        unsafe { (self.api.to_normalized)(id, value as f64) }
    }

    /// Tells property listeners that a property changed.
    fn changed(&self, id: u32, scope: u32, element: u32) {
        let listeners: Vec<_> = self.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let component = self.component.load(Relaxed);
        for (listened, proc_, user) in listeners {
            if listened == id {
                unsafe { proc_(user as *mut c_void, component, id, scope, element) };
            }
        }
    }

    /// Tells the host (automation, other views) about an edit.
    pub(crate) fn notify(&self, kind: u32, id: u32) {
        let event = AudioUnitEvent {
            mEventType: kind,
            mArgument: AudioUnitParameter { mAudioUnit: self.component.load(Relaxed), mParameterID: id, mScope: kAudioUnitScope_Global, mElement: 0 },
        };
        unsafe { AUEventListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &event) };
    }

    /// An edit from the editor or the plugin itself.
    pub(crate) fn edit(&self, id: u32, normalized: f64) {
        unsafe { (self.api.set_param)(self.core(), id, normalized) };
        self.notify(kAudioUnitEvent_ParameterValueChange, id);
    }

    fn save_class_info(&self) -> CFDictionaryRef {
        let core = self.core();
        // The size can grow between the size query and the copy: try again, never save zeros.
        let mut saved = None;
        for _ in 0..3 {
            let size = unsafe { (self.api.save_state)(core, std::ptr::null_mut(), 0) };
            let mut buf = vec![0u8; size as usize];
            let written = unsafe { (self.api.save_state)(core, buf.as_mut_ptr(), size) };
            if written <= size {
                buf.truncate(written as usize);
                saved = Some(buf);
                break;
            }
        }
        // Still changing after three tries: no class info (the host keeps its previous state).
        let Some(state) = saved else { return std::ptr::null() };
        unsafe {
            let dict = CFDictionaryCreateMutable(
                std::ptr::null(),
                0,
                &kCFTypeDictionaryKeyCallBacks as *const u8 as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const u8 as *const c_void,
            );
            let name = self.preset.lock().unwrap_or_else(std::sync::PoisonError::into_inner).1.clone();
            let entries: [(&str, CFTypeRef); 6] = [
                ("version", cf_number(0)),
                ("type", cf_number(self.desc.componentType as i32)),
                ("subtype", cf_number(self.desc.componentSubType as i32)),
                ("manufacturer", cf_number(self.desc.componentManufacturer as i32)),
                ("name", cf_string(&name)),
                ("data", CFDataCreate(std::ptr::null(), state.as_ptr(), state.len() as isize)),
            ];
            for (key, value) in entries {
                let key = cf_string(key);
                CFDictionarySetValue(dict, key, value);
                CFRelease(key);
                CFRelease(value);
            }
            dict
        }
    }

    fn load_class_info(&self, dict: CFDictionaryRef) -> OSStatus {
        unsafe {
            if dict.is_null() || CFGetTypeID(dict) != CFDictionaryGetTypeID() {
                return kAudioUnitErr_InvalidPropertyValue;
            }
            let key = cf_string("data");
            let data = CFDictionaryGetValue(dict, key);
            CFRelease(key);
            if data.is_null() || CFGetTypeID(data) != CFDataGetTypeID() {
                return kAudioUnitErr_InvalidPropertyValue;
            }
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
            if !(self.api.load_state)(self.core(), bytes.as_ptr(), bytes.len() as u32) {
                return kAudioUnitErr_InvalidPropertyValue;
            }
            let key = cf_string("name");
            let name = CFDictionaryGetValue(dict, key);
            CFRelease(key);
            if !name.is_null() {
                *self.preset.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (-1, rust_string(name));
            }
        }
        // Every parameter may have changed.
        let any = AudioUnitParameter {
            mAudioUnit: self.component.load(Relaxed),
            mParameterID: kAUParameterListener_AnyParameter,
            mScope: kAudioUnitScope_Global,
            mElement: 0,
        };
        unsafe { AUParameterListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &any) };
        self.changed(kAudioUnitProperty_PresentPreset, kAudioUnitScope_Global, 0);
        #[cfg(target_os = "macos")]
        crate::view::restored(self);
        noErr
    }

    /// Size and writability of a property, or why there is none.
    fn info(&self, id: u32, scope: u32, element: u32) -> Result<(usize, bool), OSStatus> {
        let io_scope = |writable: bool| match scope {
            kAudioUnitScope_Input if !self.instrument && element == 0 => Ok(writable),
            kAudioUnitScope_Output if element == 0 => Ok(writable),
            kAudioUnitScope_Input | kAudioUnitScope_Output => Err(kAudioUnitErr_InvalidElement),
            _ => Err(kAudioUnitErr_InvalidScope),
        };
        let ptr = size_of::<*const c_void>();
        Ok(match id {
            kAudioUnitProperty_ClassInfo => (ptr, true),
            kAudioUnitProperty_MakeConnection if !self.instrument => (size_of::<AudioUnitConnection>(), io_scope(true)?),
            kAudioUnitProperty_SampleRate => (size_of::<f64>(), if scope == kAudioUnitScope_Global { true } else { io_scope(true)? }),
            kAudioUnitProperty_ParameterList => (if scope == kAudioUnitScope_Global { 4 * self.params.len() } else { 0 }, false),
            kAudioUnitProperty_ParameterInfo => {
                if scope != kAudioUnitScope_Global {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                self.param(element).ok_or(kAudioUnitErr_InvalidParameter)?;
                (size_of::<AudioUnitParameterInfo>(), false)
            }
            kAudioUnitProperty_ParameterValueStrings => {
                if self.param(element).is_none_or(|p| p.values.is_null()) {
                    return Err(kAudioUnitErr_InvalidProperty);
                }
                (ptr, false)
            }
            kAudioUnitProperty_StreamFormat => (size_of::<AudioStreamBasicDescription>(), io_scope(true)?),
            kAudioUnitProperty_ElementCount => (4, false),
            kAudioUnitProperty_Latency => (size_of::<f64>(), false),
            kAudioUnitProperty_TailTime => (size_of::<f64>(), false),
            kAudioUnitProperty_SupportedNumChannels => (2 * size_of::<AUChannelInfo>(), false),
            kAudioUnitProperty_MaximumFramesPerSlice => (4, true),
            kAudioUnitProperty_BypassEffect if !self.instrument => (4, true),
            kAudioUnitProperty_LastRenderError => (4, false),
            kAudioUnitProperty_SetRenderCallback if !self.instrument => (size_of::<AURenderCallbackStruct>(), io_scope(true)?),
            kAudioUnitProperty_HostCallbacks => (size_of::<HostCallbackInfo>(), true),
            kAudioUnitProperty_InPlaceProcessing => (4, true),
            kAudioUnitProperty_ShouldAllocateBuffer => (4, true),
            kAudioUnitProperty_PresentPreset => (size_of::<AUPreset>(), true),
            kAudioUnitProperty_OfflineRender => (4, true),
            kAudioUnitProperty_ParameterStringFromValue => (size_of::<AudioUnitParameterStringFromValue>(), false),
            kAudioUnitProperty_ParameterValueFromString => (size_of::<AudioUnitParameterValueFromString>(), false),
            kAudioUnitProperty_CocoaUI if self.view_api.is_some() => (size_of::<AudioUnitCocoaViewInfo>(), false),
            kAudioUnitProperty_RenderContextObserver => (ptr, false),
            kAudioUnitProperty_MIDIOutputCallbackInfo => (ptr, false),
            kAudioUnitProperty_MIDIOutputCallback => (size_of::<AUMIDIOutputCallbackStruct>(), true),
            kFerritronProperty_Instance => (ptr, false),
            _ => return Err(kAudioUnitErr_InvalidProperty),
        })
    }

    unsafe fn get(&self, id: u32, scope: u32, element: u32, data: *mut c_void) -> OSStatus {
        macro_rules! put {
            ($ty:ty, $value:expr) => {{
                unsafe { *(data as *mut $ty) = $value };
                noErr
            }};
        }
        match id {
            kAudioUnitProperty_ClassInfo => put!(CFDictionaryRef, self.save_class_info()),
            kAudioUnitProperty_MakeConnection => kAudioUnitErr_PropertyNotWritable,
            kAudioUnitProperty_SampleRate => put!(f64, self.sample_rate()),
            kAudioUnitProperty_ParameterList => {
                if scope == kAudioUnitScope_Global {
                    for (i, p) in self.params.iter().enumerate() {
                        unsafe { *(data as *mut u32).add(i) = p.id };
                    }
                }
                noErr
            }
            kAudioUnitProperty_ParameterInfo => {
                let Some(p) = self.param(element) else { return kAudioUnitErr_InvalidParameter };
                let mut info = AudioUnitParameterInfo {
                    name: [0; 52],
                    unitName: p.cf_unit,
                    clumpID: 0,
                    cfNameString: p.cf_name,
                    unit: p.unit,
                    minValue: p.min,
                    maxValue: p.max,
                    defaultValue: p.default,
                    flags: p.flags,
                };
                let bytes = p.name.as_bytes();
                let n = bytes.len().min(51);
                info.name[..n].copy_from_slice(&bytes[..n]);
                put!(AudioUnitParameterInfo, info)
            }
            kAudioUnitProperty_ParameterValueStrings => {
                let Some(p) = self.param(element).filter(|p| !p.values.is_null()) else { return kAudioUnitErr_InvalidProperty };
                // The host releases its copy.
                put!(CFArrayRef, CFRetain(p.values))
            }
            kAudioUnitProperty_StreamFormat => put!(AudioStreamBasicDescription, self.format()),
            kAudioUnitProperty_ElementCount => put!(
                u32,
                match scope {
                    kAudioUnitScope_Input => (!self.instrument) as u32,
                    _ => 1,
                }
            ),
            kAudioUnitProperty_Latency => {
                let samples = self.latency_api.map_or(0, |l| unsafe { (l.get)(self.core()) });
                put!(f64, samples as f64 / self.sample_rate())
            }
            kAudioUnitProperty_TailTime => put!(f64, 0.0),
            kAudioUnitProperty_SupportedNumChannels => {
                let input = if self.instrument { 0 } else { 1 };
                let configs = [AUChannelInfo { inChannels: input, outChannels: 1 }, AUChannelInfo { inChannels: input * 2, outChannels: 2 }];
                put!([AUChannelInfo; 2], configs)
            }
            kAudioUnitProperty_MaximumFramesPerSlice => put!(u32, self.max_frames.load(Relaxed)),
            kAudioUnitProperty_BypassEffect => {
                let on = self.params.iter().find(|p| p.bypass).is_some_and(|p| self.value(p.id) >= 0.5);
                put!(u32, on as u32)
            }
            kAudioUnitProperty_LastRenderError => put!(OSStatus, noErr),
            kAudioUnitProperty_SetRenderCallback => kAudioUnitErr_PropertyNotWritable,
            kAudioUnitProperty_HostCallbacks => match *self.host_callbacks.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                Some(info) => put!(HostCallbackInfo, info),
                None => kAudioUnitErr_InvalidProperty,
            },
            kAudioUnitProperty_InPlaceProcessing => put!(u32, self.in_place.load(Relaxed)),
            kAudioUnitProperty_ShouldAllocateBuffer => put!(u32, self.allocate.load(Relaxed)),
            kAudioUnitProperty_PresentPreset => {
                let (number, name) = self.preset.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                put!(AUPreset, AUPreset { presetNumber: number, presetName: cf_string(&name) })
            }
            kAudioUnitProperty_OfflineRender => kAudioUnitErr_InvalidProperty,
            kAudioUnitProperty_ParameterStringFromValue => {
                let request = unsafe { &mut *(data as *mut AudioUnitParameterStringFromValue) };
                if self.param(request.inParamID).is_none() {
                    return kAudioUnitErr_InvalidParameter;
                }
                let value = if request.inValue.is_null() { self.value(request.inParamID) } else { unsafe { *request.inValue } };
                let text = format_value(self.api, request.inParamID, self.normalized(request.inParamID, value));
                request.outString = cf_string(&text);
                noErr
            }
            kAudioUnitProperty_ParameterValueFromString => {
                let request = unsafe { &mut *(data as *mut AudioUnitParameterValueFromString) };
                let text = rust_string(request.inString);
                let mut normalized = 0.0;
                let str = ferritron_core::abi::FerritronStr { ptr: text.as_ptr(), len: text.len() as u32 };
                let ok = unsafe { (self.api.parse_param)(request.inParamID, str, &mut normalized) };
                if !ok {
                    return kAudioUnitErr_InvalidPropertyValue;
                }
                request.outValue = unsafe { (self.api.to_plain)(request.inParamID, normalized) } as f32;
                noErr
            }
            #[cfg(target_os = "macos")]
            kAudioUnitProperty_CocoaUI => match crate::view::cocoa_view_info() {
                Some(info) => put!(AudioUnitCocoaViewInfo, info),
                None => kAudioUnitErr_InvalidProperty,
            },
            kAudioUnitProperty_RenderContextObserver => put!(*const c_void, block2::RcBlock::as_ptr(&self.observer) as *const c_void),
            kAudioUnitProperty_MIDIOutputCallbackInfo => {
                // The name of the plugin's one MIDI output, as a CFArray of CFStrings.
                let name = unsafe { (*self.api.info).name.to_str() }.unwrap_or("Ferritron").to_string();
                let cf = cf_string(&name);
                let names = [cf];
                let array = unsafe { CFArrayCreate(std::ptr::null(), names.as_ptr(), 1, &kCFTypeArrayCallBacks as *const u8 as *const c_void) };
                unsafe { CFRelease(cf) };
                put!(CFArrayRef, array)
            }
            kAudioUnitProperty_MIDIOutputCallback => match *self.midi_out.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                Some(callback) => put!(AUMIDIOutputCallbackStruct, callback),
                None => kAudioUnitErr_InvalidProperty,
            },
            kFerritronProperty_Instance => put!(*const Unit, self as *const Unit),
            _ => kAudioUnitErr_InvalidProperty,
        }
    }

    unsafe fn set(&self, id: u32, scope: u32, element: u32, data: *const c_void) -> OSStatus {
        macro_rules! read {
            ($ty:ty) => {{
                #[allow(unused_unsafe)]
                let value = unsafe { std::ptr::read_unaligned(data as *const $ty) };
                value
            }};
        }
        match id {
            kAudioUnitProperty_ClassInfo => self.load_class_info(read!(CFDictionaryRef)),
            kAudioUnitProperty_MakeConnection => {
                let connection: AudioUnitConnection = read!(AudioUnitConnection);
                let value = (!connection.sourceAudioUnit.is_null()).then_some(connection);
                *self.connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = value;
                self.changed(id, scope, element);
                noErr
            }
            kAudioUnitProperty_SampleRate => {
                let rate: f64 = read!(f64);
                if rate.is_nan() || rate <= 0.0 {
                    return kAudioUnitErr_InvalidPropertyValue;
                }
                self.set_rate(rate);
                noErr
            }
            kAudioUnitProperty_StreamFormat => {
                let format: AudioStreamBasicDescription = read!(AudioStreamBasicDescription);
                let float = format.mFormatFlags & kAudioFormatFlagIsFloat != 0 && format.mBitsPerChannel == 32;
                let non_interleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 || format.mChannelsPerFrame == 1;
                if format.mFormatID != kAudioFormatLinearPCM || !float || !non_interleaved || !(1..=2).contains(&format.mChannelsPerFrame) {
                    return kAudioUnitErr_FormatNotSupported;
                }
                if self.initialized.load(SeqCst) && format.mChannelsPerFrame != self.channels.load(Relaxed) {
                    return kAudioUnitErr_Initialized;
                }
                // Input and output share the layout: changing one changes both.
                self.channels.store(format.mChannelsPerFrame, Relaxed);
                if format.mSampleRate > 0.0 {
                    self.set_rate(format.mSampleRate);
                }
                self.changed(id, kAudioUnitScope_Output, 0);
                if !self.instrument {
                    self.changed(id, kAudioUnitScope_Input, 0);
                }
                noErr
            }
            kAudioUnitProperty_MaximumFramesPerSlice => {
                if self.initialized.load(SeqCst) {
                    return kAudioUnitErr_Initialized;
                }
                self.max_frames.store(read!(u32).max(1), Relaxed);
                self.changed(id, scope, element);
                noErr
            }
            kAudioUnitProperty_BypassEffect => {
                let on = read!(u32) != 0;
                if let Some(p) = self.params.iter().find(|p| p.bypass) {
                    self.edit(p.id, if on { 1.0 } else { 0.0 });
                }
                self.changed(id, scope, element);
                noErr
            }
            kAudioUnitProperty_SetRenderCallback => {
                let callback: AURenderCallbackStruct = read!(AURenderCallbackStruct);
                *self.callback.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = callback.inputProc.is_some().then_some(callback);
                self.changed(id, scope, element);
                noErr
            }
            kAudioUnitProperty_HostCallbacks => {
                *self.host_callbacks.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(read!(HostCallbackInfo));
                noErr
            }
            kAudioUnitProperty_MIDIOutputCallback => {
                let callback: AUMIDIOutputCallbackStruct = read!(AUMIDIOutputCallbackStruct);
                *self.midi_out.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = callback.midiOutputCallback.is_some().then_some(callback);
                noErr
            }
            kAudioUnitProperty_InPlaceProcessing => {
                self.in_place.store(read!(u32), Relaxed);
                noErr
            }
            kAudioUnitProperty_ShouldAllocateBuffer => {
                self.allocate.store(read!(u32), Relaxed);
                noErr
            }
            kAudioUnitProperty_PresentPreset => {
                let preset: AUPreset = read!(AUPreset);
                *self.preset.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (preset.presetNumber, rust_string(preset.presetName));
                noErr
            }
            kAudioUnitProperty_OfflineRender => {
                if let Some(render) = self.render_api {
                    unsafe { (render.set_offline)(self.core(), read!(u32) != 0) };
                }
                noErr
            }
            _ => kAudioUnitErr_PropertyNotWritable,
        }
    }

    fn set_rate(&self, rate: f64) {
        if self.sample_rate() == rate {
            return;
        }
        self.sample_rate.store(rate.to_bits(), Relaxed);
        if self.initialized.load(SeqCst) {
            // Takes effect like a new setup.
            unsafe { (self.api.activate)(self.core(), rate, self.max_frames.load(Relaxed)) };
            self.changed(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
        }
        self.changed(kAudioUnitProperty_SampleRate, kAudioUnitScope_Output, 0);
    }

    fn initialize(&self) -> OSStatus {
        let frames = self.max_frames.load(Relaxed) as usize;
        let state = unsafe { &mut *self.render.get() };
        state.output = (0..MAX_CHANNELS).map(|_| vec![0.0; frames]).collect();
        state.input = (0..MAX_CHANNELS).map(|_| vec![0.0; frames]).collect();
        if !unsafe { (self.api.activate)(self.core(), self.sample_rate(), frames as u32) } {
            return kAudioUnitErr_FailedInitialization;
        }
        self.initialized.store(true, SeqCst);
        self.changed(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
        noErr
    }

    /// Fetches the input into `state.input` (effects): from the render
    /// callback or the connected unit.
    unsafe fn pull(&self, state: &mut RenderState, time: *const c_void, frames: u32, channels: usize) -> Result<(), OSStatus> {
        let mut list = AudioBufferList::<MAX_CHANNELS> {
            mNumberBuffers: channels as u32,
            mBuffers: std::array::from_fn(|i| AudioBuffer { mNumberChannels: 1, mDataByteSize: frames * 4, mData: state.input[i].as_mut_ptr() as *mut c_void }),
        };
        let header = &mut list as *mut AudioBufferList<MAX_CHANNELS> as *mut AudioBufferListHeader;
        let mut flags = 0u32;
        let status = if let Some(callback) = self.callback.try_lock().ok().and_then(|c| *c) {
            unsafe { (callback.inputProc.unwrap())(callback.inputProcRefCon, &mut flags, time, 0, frames, header) }
        } else if let Some(connection) = self.connection.try_lock().ok().and_then(|c| *c) {
            unsafe { AudioUnitRender(connection.sourceAudioUnit, &mut flags, time, connection.sourceOutputNumber, frames, header) }
        } else {
            return Err(kAudioUnitErr_NoConnection);
        };
        if status != noErr {
            return Err(status);
        }
        // The provider may have pointed the buffers at its own memory.
        for (i, buffer) in list.mBuffers.iter().enumerate().take(channels) {
            let source = buffer.mData as *const f32;
            if !source.is_null() && source != state.input[i].as_ptr() {
                unsafe { std::ptr::copy_nonoverlapping(source, state.input[i].as_mut_ptr(), frames as usize) };
            }
        }
        Ok(())
    }

    fn render_notify(&self, flags: u32, time: *const c_void, bus: u32, frames: u32, data: *mut AudioBufferListHeader) {
        let Ok(notifies) = self.notifies.try_lock() else { return };
        for &(proc_, user) in notifies.iter() {
            let mut f = flags;
            unsafe { proc_(user as *mut c_void, &mut f, time, bus, frames, data) };
        }
    }

    unsafe fn render(&self, flags: *mut u32, time: *const c_void, bus: u32, frames: u32, data: *mut AudioBufferListHeader) -> OSStatus {
        if !self.initialized.load(Acquire) {
            return kAudioUnitErr_Uninitialized;
        }
        if bus != 0 {
            return kAudioUnitErr_InvalidElement;
        }
        if frames > self.max_frames.load(Relaxed) {
            return kAudioUnitErr_TooManyFramesToProcess;
        }
        if data.is_null() {
            return kAudio_ParamError;
        }
        let state = unsafe { &mut *self.render.get() };
        let base = if flags.is_null() { 0 } else { unsafe { *flags } };
        self.render_notify(base | kAudioUnitRenderAction_PreRender, time, bus, frames, data);

        let channels = (self.channels.load(Relaxed) as usize).min(unsafe { (*data).mNumberBuffers } as usize).min(MAX_CHANNELS);
        let buffers = unsafe { std::slice::from_raw_parts_mut((*data).mBuffers.as_mut_ptr(), (*data).mNumberBuffers as usize) };
        let mut outputs = [std::ptr::null_mut::<f32>(); MAX_CHANNELS];
        for (i, buffer) in buffers.iter_mut().enumerate().take(channels) {
            if buffer.mData.is_null() {
                buffer.mData = state.output[i].as_mut_ptr() as *mut c_void;
            }
            buffer.mDataByteSize = frames * 4;
            outputs[i] = buffer.mData as *mut f32;
        }
        let n = frames as usize;
        let mut status = noErr;
        if self.instrument {
            for &out in outputs.iter().take(channels) {
                unsafe { std::ptr::write_bytes(out, 0, n) };
            }
        } else {
            match unsafe { self.pull(state, time, frames, channels) } {
                Ok(()) => {
                    for (i, &out) in outputs.iter().enumerate().take(channels) {
                        unsafe { std::ptr::copy_nonoverlapping(state.input[i].as_ptr(), out, n) };
                    }
                }
                Err(e) => {
                    for &out in outputs.iter().take(channels) {
                        unsafe { std::ptr::write_bytes(out, 0, n) };
                    }
                    status = e;
                }
            }
        }
        if status == noErr {
            state.events.clear();
            self.events.with(|queued| {
                for mut event in queued.drain(..) {
                    event.offset = event.offset.min(frames.saturating_sub(1));
                    state.events.push(event);
                }
            });
            // Stable (same-offset events keep their order), without allocating.
            let n = state.events.len();
            ferritron_core::abi::sort_events(&mut state.events, &mut state.sort_scratch[..n]);
            if let Some(t) = extension::<ferritron_core::abi::TransportApi>(self.api, ferritron_core::abi::EXT_TRANSPORT) {
                let c = self.transport();
                unsafe { (t.set)(self.core(), &c) };
            }
            let mut out_count = 0u32;
            let ok = unsafe {
                let mut d = ferritron_core::abi::ProcessData::new(outputs.as_ptr(), channels as u32, frames, state.events.as_ptr(), state.events.len() as u32, std::ptr::null());
                d.out_events = state.out.as_mut_ptr();
                d.out_capacity = state.out.len() as u32;
                d.out_count = &mut out_count;
                (self.api.process)(self.core(), &d)
            };
            if !ok {
                for &out in outputs.iter().take(channels) {
                    unsafe { std::ptr::write_bytes(out, 0, n) };
                }
            } else {
                let written = (out_count as usize).min(state.out.len());
                self.send_out(&state.out[..written], time);
            }
        }
        self.render_notify(base | kAudioUnitRenderAction_PostRender, time, bus, frames, data);
        status
    }

    /// The host's transport, from the callbacks it gave (`kAudioUnitProperty_HostCallbacks`).
    /// Written from the documentation of `HostCallbackInfo`; not run on a Mac.
    fn transport(&self) -> ferritron_core::transport::TransportC {
        use ferritron_core::transport::*;
        let mut t = TransportC::default();
        let Some(info) = *self.host_callbacks.try_lock().ok().as_deref().unwrap_or(&None) else { return t };
        let user = info.hostUserData;
        // beatAndTempoProc(userData, &currentBeat, &currentTempo)
        if !info.callbacks[0].is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> OSStatus = unsafe { std::mem::transmute(info.callbacks[0]) };
            let (mut beat, mut tempo) = (0.0f64, 0.0f64);
            if unsafe { f(user, &mut beat, &mut tempo) } == noErr {
                t.flags |= TRANSPORT_TEMPO | TRANSPORT_POSITION_BEATS;
                t.tempo = tempo;
                t.position_beats = beat;
            }
        }
        // musicalTimeLocationProc(userData, &deltaSampleOffsetToNextBeat, &timeSigNumerator, &timeSigDenominator, &currentMeasureDownBeat)
        if !info.callbacks[1].is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> OSStatus = unsafe { std::mem::transmute(info.callbacks[1]) };
            let (mut delta, mut num, mut den, mut bar) = (0u32, 0.0f32, 0u32, 0.0f64);
            if unsafe { f(user, &mut delta, &mut num, &mut den, &mut bar) } == noErr {
                t.flags |= TRANSPORT_TIME_SIGNATURE | TRANSPORT_BAR_START;
                t.numerator = num as u16;
                t.denominator = den as u16;
                t.bar_start_beats = bar;
            }
        }
        // transportStateProc(userData, &isPlaying, &changed, &currentSampleInTimeline, &isCycling, &cycleStart, &cycleEnd)
        if !info.callbacks[2].is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut u8, *mut u8, *mut f64, *mut u8, *mut f64, *mut f64) -> OSStatus = unsafe { std::mem::transmute(info.callbacks[2]) };
            let (mut playing, mut changed, mut sample, mut cycling, mut start, mut end) = (0u8, 0u8, 0.0f64, 0u8, 0.0f64, 0.0f64);
            if unsafe { f(user, &mut playing, &mut changed, &mut sample, &mut cycling, &mut start, &mut end) } == noErr {
                if playing != 0 {
                    t.flags |= TRANSPORT_PLAYING;
                }
                t.flags |= TRANSPORT_POSITION_SAMPLES | TRANSPORT_LOOP_RANGE;
                t.position_samples = sample as i64;
                t.loop_start = start;
                t.loop_end = end;
                if cycling != 0 {
                    t.flags |= TRANSPORT_LOOPING;
                }
            }
        }
        t
    }

    fn schedule(&self, events: &[AudioUnitParameterEvent]) {
        for e in events {
            let (offset, value) = match e.eventType {
                // { bufferOffset, value }
                kParameterEvent_Immediate => (e.eventValues[0], f32::from_bits(e.eventValues[1])),
                // { startBufferOffset, durationInFrames, startValue, endValue }: the end value at the end
                kParameterEvent_Ramped => ((e.eventValues[0] as i32).max(0) as u32 + e.eventValues[1], f32::from_bits(e.eventValues[3])),
                _ => continue,
            };
            if self.param(e.parameter).is_some() {
                let normalized = self.normalized(e.parameter, value);
                self.events.push(Event { offset, kind: EVENT_PARAM, data: EventData { param: ParamData { id: e.parameter, normalized } } });
            }
        }
    }

    /// Sends what the plugin wrote in this block. MIDI goes to the host's MIDI output right
    /// away, on the render thread (where the host's callback is meant to run); parameter
    /// edits are queued for the main thread (`drain_param_edits`). Allocates nothing; a busy
    /// callback or a full queue drops the event. Sample offsets are not used for MIDI yet:
    /// packets carry timestamp 0 (now), so they can land up to a block late.
    fn send_out(&self, events: &[Event], time: *const c_void) {
        let midi = self.midi_out.try_lock().ok().and_then(|callback| *callback);
        for event in events {
            if event.kind == EVENT_PARAM {
                // SAFETY: the kind says the `param` member is the one the plugin wrote.
                let id = unsafe { event.data.param.id };
                self.param_edits.push(id);
                if !self.param_edits.scheduled.swap(true, SeqCst) {
                    on_main_with(self as *const Unit as *mut c_void, drain_param_edits);
                }
            } else if event.kind == EVENT_MIDI {
                let Some(callback) = midi else { continue };
                // SAFETY: the kind says the `midi` member is the one the plugin wrote.
                let m = unsafe { event.data.midi };
                // The status byte without the channel; data bytes are 7 bits.
                let (len, kind): (u16, u8) = match m.status & 0xF0 {
                    kind @ (0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0) => (3, kind),
                    kind @ (0xC0 | 0xD0) => (2, kind),
                    _ => continue,
                };
                let mut data = [0u8; 256];
                data[0] = kind | (m.channel & 0x0F);
                data[1] = m.data1 & 0x7F;
                data[2] = m.data2 & 0x7F;
                let list = MIDIPacketList { numPackets: 1, packet: MIDIPacket { timeStamp: 0, length: len, data } };
                if let Some(send) = callback.midiOutputCallback {
                    unsafe { send(callback.userData, time, 0, &list) };
                }
            }
        }
    }

    fn midi(&self, status: u32, data1: u32, data2: u32, offset: u32) {
        let status_byte = status;
        let kind = match status & 0xF0 {
            0x90 if data2 > 0 => EVENT_NOTE_ON,
            0x80 | 0x90 => EVENT_NOTE_OFF,
            // Controllers, pressure, pitch bend, program changes.
            status @ (0xA0..=0xE0) => {
                let m = ferritron_core::abi::MidiData { status: status as u8, channel: (status_byte & 0x0F) as u8, data1: (data1 & 0x7F) as u8, data2: (data2 & 0x7F) as u8 };
                self.events.push(Event { offset, kind: ferritron_core::abi::EVENT_MIDI, data: EventData { midi: m } });
                return;
            }
            _ => return,
        };
        let note = NoteData { note_id: -1, channel: (status & 0x0F) as i16, key: (data1 & 0x7F) as i16, velocity: (data2 & 0x7F) as f32 / 127.0 };
        self.events.push(Event { offset, kind, data: EventData { note } });
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        for p in &self.params {
            for cf in [p.cf_name, p.cf_unit, p.values] {
                if !cf.is_null() {
                    unsafe { CFRelease(cf) };
                }
            }
        }
    }
}

// --- The component's entry points ---------------------------------------------

unsafe fn unit<'a>(this: *mut c_void) -> &'a Unit {
    unsafe { &*(this as *const Unit) }
}

/// Main thread: tells the host about the parameter edits the render thread queued (the
/// host records them as automation). `context` is the unit; it may be closed already,
/// which `is_live` checks before the unit is touched.
unsafe extern "C" fn drain_param_edits(context: *mut c_void) {
    let unit = context as *const Unit;
    if !is_live(unit) {
        return;
    }
    let unit = unsafe { &*unit };
    unit.param_edits.scheduled.store(false, SeqCst);
    while let Some(id) = unit.param_edits.pop() {
        unit.notify(kAudioUnitEvent_ParameterValueChange, id);
    }
}

/// Runs an entry point, turning a panic into an error instead of unwinding into the host.
fn guard(f: impl FnOnce() -> OSStatus) -> OSStatus {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(kAudioUnitErr_FailedInitialization)
}

unsafe extern "C" fn open(this: *mut c_void, component: *mut c_void) -> OSStatus {
    guard(|| {
        let unit = unsafe { &mut *(this as *mut Unit) };
        unit.component.store(component, SeqCst);
        let core = unsafe { (unit.api.create)() };
        if core.is_null() {
            return kAudioUnitErr_FailedInitialization;
        }
        unit.core.store(core, SeqCst);
        LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(HashSet::new).insert(this as usize);
        // Edits the plugin makes itself (AI clients through the MCP hub) reach the host on the main thread.
        if let Some(link) = extension::<HostApi>(unit.api, EXT_HOST) {
            unsafe extern "C" fn edited(ctx: *mut c_void, id: u32, normalized: f64) {
                let address = ctx as usize;
                on_main(Box::new(move || {
                    let unit = address as *const Unit;
                    if is_live(unit) {
                        unsafe { (*unit).edit(id, normalized) };
                    }
                }));
            }
            unit.link.ctx = this;
            unit.link.edited = Some(edited);
            unsafe { (link.attach)(core, &*unit.link) };
        }
        noErr
    })
}

unsafe extern "C" fn close(this: *mut c_void) -> OSStatus {
    guard(|| {
        LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(HashSet::new).remove(&(this as usize));
        let unit = unsafe { Box::from_raw(this as *mut Unit) };
        #[cfg(target_os = "macos")]
        crate::view::detach(&unit);
        let core = unit.core.swap(std::ptr::null_mut(), SeqCst);
        if !core.is_null() {
            unsafe { (unit.api.destroy)(core) };
        }
        drop(unit);
        noErr
    })
}

unsafe extern "C" fn initialize(this: *mut c_void) -> OSStatus {
    guard(|| unsafe { unit(this) }.initialize())
}

unsafe extern "C" fn uninitialize(this: *mut c_void) -> OSStatus {
    unsafe { unit(this) }.initialized.store(false, SeqCst);
    noErr
}

unsafe extern "C" fn get_property_info(this: *mut c_void, id: u32, scope: u32, element: u32, size: *mut u32, writable: *mut u8) -> OSStatus {
    guard(|| match unsafe { unit(this) }.info(id, scope, element) {
        Ok((bytes, can_write)) => {
            unsafe {
                if !size.is_null() {
                    *size = bytes as u32;
                }
                if !writable.is_null() {
                    *writable = can_write as u8;
                }
            }
            noErr
        }
        Err(e) => e,
    })
}

unsafe extern "C" fn get_property(this: *mut c_void, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OSStatus {
    guard(|| {
        let unit = unsafe { unit(this) };
        let needed = match unit.info(id, scope, element) {
            Ok((bytes, _)) => bytes,
            Err(e) => return e,
        };
        if data.is_null() || size.is_null() || (unsafe { *size } as usize) < needed {
            return kAudio_ParamError;
        }
        let status = unsafe { unit.get(id, scope, element, data) };
        if status == noErr {
            unsafe { *size = needed as u32 };
        }
        status
    })
}

unsafe extern "C" fn set_property(this: *mut c_void, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OSStatus {
    guard(|| {
        let unit = unsafe { unit(this) };
        let needed = match unit.info(id, scope, element) {
            Ok((bytes, true)) => bytes,
            Ok(_) => return kAudioUnitErr_PropertyNotWritable,
            Err(e) => return e,
        };
        // A null value removes a render callback or connection.
        if data.is_null() {
            return match id {
                kAudioUnitProperty_SetRenderCallback => {
                    *unit.callback.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    noErr
                }
                kAudioUnitProperty_MakeConnection => {
                    *unit.connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    noErr
                }
                _ => kAudio_ParamError,
            };
        }
        if (size as usize) < needed {
            return kAudio_ParamError;
        }
        unsafe { unit.set(id, scope, element, data) }
    })
}

unsafe extern "C" fn add_property_listener(this: *mut c_void, id: u32, proc_: Option<AudioUnitPropertyListenerProc>, user: *mut c_void) -> OSStatus {
    let Some(proc_) = proc_ else { return kAudio_ParamError };
    unsafe { unit(this) }.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((id, proc_, user as usize));
    noErr
}

unsafe extern "C" fn remove_property_listener(this: *mut c_void, id: u32, proc_: Option<AudioUnitPropertyListenerProc>) -> OSStatus {
    let target = proc_.map(|p| p as usize);
    unsafe { unit(this) }.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|&(i, p, _)| !(i == id && Some(p as usize) == target));
    noErr
}

unsafe extern "C" fn remove_property_listener_with_user_data(
    this: *mut c_void,
    id: u32,
    proc_: Option<AudioUnitPropertyListenerProc>,
    user: *mut c_void,
) -> OSStatus {
    let target = proc_.map(|p| p as usize);
    unsafe { unit(this) }
        .listeners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|&(i, p, u)| !(i == id && Some(p as usize) == target && u == user as usize));
    noErr
}

unsafe extern "C" fn get_parameter(this: *mut c_void, id: u32, _scope: u32, _element: u32, value: *mut f32) -> OSStatus {
    let unit = unsafe { unit(this) };
    if unit.param(id).is_none() || value.is_null() {
        return kAudioUnitErr_InvalidParameter;
    }
    unsafe { *value = unit.value(id) };
    noErr
}

unsafe extern "C" fn set_parameter(this: *mut c_void, id: u32, _scope: u32, _element: u32, value: f32, _offset: u32) -> OSStatus {
    let unit = unsafe { unit(this) };
    if unit.param(id).is_none() {
        return kAudioUnitErr_InvalidParameter;
    }
    unsafe { (unit.api.set_param)(unit.core(), id, unit.normalized(id, value)) };
    noErr
}

unsafe extern "C" fn schedule_parameters(this: *mut c_void, events: *const AudioUnitParameterEvent, count: u32) -> OSStatus {
    if !events.is_null() {
        unsafe { unit(this) }.schedule(unsafe { std::slice::from_raw_parts(events, count as usize) });
    }
    noErr
}

unsafe extern "C" fn reset(this: *mut c_void, _scope: u32, _element: u32) -> OSStatus {
    let unit = unsafe { unit(this) };
    if unit.initialized.load(SeqCst) {
        // Libraries built before 2.0.1 have no reset in their table: a fresh setup then clears through activate.
        if unit.api.struct_size as usize >= std::mem::offset_of!(ferritron_core::abi::PluginApi, reset) + size_of::<usize>() {
            unsafe { (unit.api.reset)(unit.core()) };
        } else {
            unsafe { (unit.api.activate)(unit.core(), unit.sample_rate(), unit.max_frames.load(Relaxed)) };
        }
    }
    noErr
}

unsafe extern "C" fn render(this: *mut c_void, flags: *mut u32, time: *const c_void, bus: u32, frames: u32, data: *mut AudioBufferListHeader) -> OSStatus {
    catch_unwind(AssertUnwindSafe(|| unsafe { unit(this).render(flags, time, bus, frames, data) })).unwrap_or(kAudio_ParamError)
}

unsafe extern "C" fn add_render_notify(this: *mut c_void, proc_: Option<AURenderCallback>, user: *mut c_void) -> OSStatus {
    let Some(proc_) = proc_ else { return kAudio_ParamError };
    unsafe { unit(this) }.notifies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((proc_, user as usize));
    noErr
}

unsafe extern "C" fn remove_render_notify(this: *mut c_void, proc_: Option<AURenderCallback>, user: *mut c_void) -> OSStatus {
    let target = proc_.map(|p| p as usize);
    unsafe { unit(this) }
        .notifies
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|&(p, u)| !(Some(p as usize) == target && u == user as usize));
    noErr
}

unsafe extern "C" fn midi_event(this: *mut c_void, status: u32, data1: u32, data2: u32, offset: u32) -> OSStatus {
    unsafe { unit(this) }.midi(status, data1, data2, offset);
    noErr
}

unsafe extern "C" fn sysex(_this: *mut c_void, _data: *const u8, _length: u32) -> OSStatus {
    noErr
}

unsafe extern "C" fn lookup(selector: i16) -> *const c_void {
    match selector {
        kAudioUnitInitializeSelect => initialize as *const c_void,
        kAudioUnitUninitializeSelect => uninitialize as *const c_void,
        kAudioUnitGetPropertyInfoSelect => get_property_info as *const c_void,
        kAudioUnitGetPropertySelect => get_property as *const c_void,
        kAudioUnitSetPropertySelect => set_property as *const c_void,
        kAudioUnitAddPropertyListenerSelect => add_property_listener as *const c_void,
        kAudioUnitRemovePropertyListenerSelect => remove_property_listener as *const c_void,
        kAudioUnitRemovePropertyListenerWithUserDataSelect => remove_property_listener_with_user_data as *const c_void,
        kAudioUnitGetParameterSelect => get_parameter as *const c_void,
        kAudioUnitSetParameterSelect => set_parameter as *const c_void,
        kAudioUnitScheduleParametersSelect => schedule_parameters as *const c_void,
        kAudioUnitResetSelect => reset as *const c_void,
        kAudioUnitRenderSelect => render as *const c_void,
        kAudioUnitAddRenderNotifySelect => add_render_notify as *const c_void,
        kAudioUnitRemoveRenderNotifySelect => remove_render_notify as *const c_void,
        kMusicDeviceMIDIEventSelect => midi_event as *const c_void,
        kMusicDeviceSysExSelect => sysex as *const c_void,
        _ => std::ptr::null(),
    }
}

/// The component's factory (named in Info.plist): one instance per call.
pub(crate) fn factory(api: &'static PluginApi, desc: *const AudioComponentDescription) -> *mut c_void {
    let desc = unsafe { desc.as_ref() }.copied().unwrap_or_default();
    Box::into_raw(Unit::new(api, desc)) as *mut c_void
}
