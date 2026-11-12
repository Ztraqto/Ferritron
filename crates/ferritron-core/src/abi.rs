//! Rust side of `include/ferritron_abi.h`: exposes a [`Process`] plugin as a
//! C function table that format shims call.

use crate::ParamEvent;
use crate::__private::PluginMeta;
use crate::{
    ExpressionKind, VoiceEvent, VoiceEventKind, MidiEvent, MidiKind, AudioIo, Context, NoteEvent, NoteKind, ParamCell, ParamInfo, ParamKind, ParamScale, ParamSet, PluginInfo, PluginKind, Process, Quality, Setup,
    Tail, state,
};
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub const ABI_VERSION: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FerritronStr {
    pub ptr: *const u8,
    pub len: u32,
}

impl FerritronStr {
    pub const fn new(s: &'static str) -> Self {
        Self { ptr: s.as_ptr(), len: s.len() as u32 }
    }

    /// # Safety
    /// `ptr` must point to `len` readable bytes.
    pub unsafe fn to_str<'a>(self) -> Option<&'a str> {
        if self.ptr.is_null() {
            return Some("");
        }
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }).ok()
    }
}

#[repr(C)]
pub struct PluginInfoC {
    pub id: FerritronStr,
    pub name: FerritronStr,
    pub vendor: FerritronStr,
    pub version: FerritronStr,
    pub url: FerritronStr,
    pub email: FerritronStr,
    pub kind: u32,
    pub class_id: [u8; 16],
    pub isolation: u32,
    /// Added in 1.3.1 (at the end, so older offsets hold).
    pub vst3_subcategories: FerritronStr,
    pub clap_features: FerritronStr,
    /// `AUDIO_*` bits.
    pub audio_flags: u32,
    pub min_channels: u32,
    pub max_channels: u32,
    /// Sidechain channels; 0 = as many as the main bus (only with `AUDIO_SIDECHAIN`).
    pub sidechain_channels: u32,
    /// VST 2 plugin ID; 0 = derive one.
    pub vst2_id: u32,
}

/// [`PluginInfoC::audio_flags`]: the plugin takes MIDI (an effect with `midi_input`).
pub const AUDIO_MIDI_INPUT: u32 = 1 << 0;
/// [`PluginInfoC::audio_flags`]: the plugin has a sidechain input.
pub const AUDIO_SIDECHAIN: u32 = 1 << 1;
/// [`PluginInfoC::audio_flags`]: layouts out of range pass through.
pub const AUDIO_PASSTHROUGH: u32 = 1 << 2;
/// [`PluginInfoC::audio_flags`]: VST3 controllers are left to the host's mapping.
pub const AUDIO_HOST_CC: u32 = 1 << 3;

// SAFETY: only points to immutable 'static data.
unsafe impl Sync for PluginInfoC {}

impl PluginInfoC {
    pub const fn new(info: &'static PluginInfo) -> Self {
        Self {
            id: FerritronStr::new(info.id),
            name: FerritronStr::new(info.name),
            vendor: FerritronStr::new(info.vendor),
            version: FerritronStr::new(info.version),
            url: FerritronStr::new(info.url),
            email: FerritronStr::new(info.email),
            kind: match info.kind {
                PluginKind::Effect => 0,
                PluginKind::Instrument => 1,
            },
            class_id: info.class_id,
            isolation: info.isolation as u32
                | if info.process.per_instance { crate::isolation::PER_INSTANCE } else { 0 }
                | if info.process.pipeline { crate::isolation::PIPELINE } else { 0 },
            vst3_subcategories: FerritronStr::new(info.vst3_subcategories),
            clap_features: FerritronStr::new(info.clap_features),
            audio_flags: (info.audio.midi_input as u32 * AUDIO_MIDI_INPUT)
                | (info.audio.sidechain.is_some() as u32 * AUDIO_SIDECHAIN)
                | (info.audio.passthrough as u32 * AUDIO_PASSTHROUGH)
                | (info.audio.host_cc as u32 * AUDIO_HOST_CC),
            min_channels: info.audio.min_channels,
            max_channels: info.audio.max_channels,
            sidechain_channels: match info.audio.sidechain {
                Some(n) => n,
                None => 0,
            },
            vst2_id: info.vst2_id,
        }
    }
}

const PARAM_LIST: u32 = 1 << 0;
const PARAM_BYPASS: u32 = 1 << 1;
const PARAM_NOT_AUTOMATABLE: u32 = 1 << 2;
const PARAM_HIDDEN: u32 = 1 << 3;
/// The built-in "Program" parameter (plugins with `[presets]`): VST3 makes it the program list's
/// `kIsProgramChange` parameter; other formats keep it hidden and offer the presets their own way.
pub const PARAM_PROGRAM: u32 = 1 << 4;
/// `poly = true`: hosts may modulate it per note (CLAP `CLAP_PARAM_IS_MODULATABLE_PER_*`).
pub const PARAM_POLY: u32 = 1 << 5;

/// The "AI Access" switch of plugins with `[mcp]`: while on, AI clients may
/// reach the instance through the Ferritron MCP hub. Off for every new
/// instance and not automatable; saved with the project like any parameter
/// (formats require state to restore every parameter).
static MCP_SWITCH: ParamInfo = ParamInfo {
    id: crate::__private::reserved_id("ferritron.mcp"),
    key: "ferritron.mcp",
    name: "AI Access",
    unit: "",
    kind: ParamKind::Bool,
    default: 0.0,
    scale: ParamScale::Linear,
    text: None,
};

/// The "Quality" switch of plugins with `[quality]`: Auto (best when the host
/// renders offline, normal otherwise), Normal or Best. Not automatable;
/// saved with the project.
static QUALITY: ParamInfo = ParamInfo {
    id: crate::__private::reserved_id(crate::i18n::QUALITY_KEY),
    key: crate::i18n::QUALITY_KEY,
    name: "Quality",
    unit: "",
    kind: ParamKind::Enum { choices: &["Auto", "Normal", "Best"] },
    default: 0.0,
    scale: ParamScale::Linear,
    text: None,
};

/// The bypass parameter every plugin gets. It sits after the plugin's own
/// parameters; while it is on, input passes through unprocessed.
static BYPASS: ParamInfo = ParamInfo {
    id: crate::__private::BYPASS_ID,
    key: "ferritron.bypass",
    name: "Bypass",
    unit: "",
    kind: ParamKind::Bool,
    default: 0.0,
    scale: ParamScale::Linear,
    text: None,
};

#[repr(C)]
pub struct ParamInfoC {
    pub id: u32,
    pub name: FerritronStr,
    pub unit: FerritronStr,
    pub step_count: u32,
    pub default_normalized: f64,
    pub flags: u32,
}

pub const EXT_VIEW: &std::ffi::CStr = c"ferritron.view.v1";
pub const EXT_LATENCY: &std::ffi::CStr = c"ferritron.latency.v1";
pub const EXT_RENDER: &std::ffi::CStr = c"ferritron.render.v1";
pub const EXT_HOST: &std::ffi::CStr = c"ferritron.host.v1";
pub const EXT_EDITOR_SIZE: &std::ffi::CStr = c"ferritron.editor-size.v1";
pub const EXT_ARA: &std::ffi::CStr = c"ferritron.ara.v1";
pub const EXT_SLEEP: &std::ffi::CStr = c"ferritron.sleep.v1";
pub const EXT_HOST2: &std::ffi::CStr = c"ferritron.host.v2";
pub const EXT_TRACK: &std::ffi::CStr = c"ferritron.track.v1";
pub const EXT_TRANSPORT: &std::ffi::CStr = c"ferritron.transport.v1";
pub const EXT_MAIN: &std::ffi::CStr = c"ferritron.main.v1";
pub const EXT_SIDECHAIN: &std::ffi::CStr = c"ferritron.sidechain.v1";
pub const EXT_LAYOUT: &std::ffi::CStr = c"ferritron.layout.v1";
/// (2.0) `ferritron_voices_api`: how many voices the instrument plays (`Process::VOICES`).
pub const EXT_VOICES: &std::ffi::CStr = c"ferritron.voices.v1";
/// (2.0, audit fix f4) `ferritron_role_api`: marks an instance as the controller copy of a `[vst3] controller_id` plugin.
pub const EXT_ROLE: &std::ffi::CStr = c"ferritron.role.v1";

/// `ferritron_voices_api` (`ferritron.voices.v1`); offered only when `Process::VOICES > 0`.
#[repr(C)]
pub struct VoicesApi {
    /// The most voices the plugin plays at once. `main`
    pub capacity: unsafe extern "C" fn(*mut c_void) -> u32,
}

unsafe extern "C" fn voice_capacity<P: Process>(_ptr: *mut c_void) -> u32 {
    P::VOICES
}

/// The role of an instance that is not the one people use: the controller copy (`[vst3] controller_id`).
pub const ROLE_CONTROLLER: u32 = 1;

/// `ferritron_role_api` (`ferritron.role.v1`): the shim tells the core what an instance is for, right after
/// `create`. Main thread.
#[repr(C)]
pub struct RoleApi {
    /// `sizeof(RoleApi)` as the library was built; a shim checks it before it calls `set_role`.
    pub struct_size: u32,
    /// `role`: 0 = the usual instance, `ROLE_CONTROLLER` = the controller copy. `main`
    pub set_role: unsafe extern "C" fn(*mut c_void, u32),
}

/// Controller copy: out of `instances()`, `instances_watch` and the MCP hub. Other roles change nothing.
unsafe extern "C" fn set_role<P: Process>(ptr: *mut c_void, role: u32) {
    if ptr.is_null() || role != ROLE_CONTROLLER {
        return;
    }
    let instance = unsafe { instance::<P>(ptr) };
    instance.handle.0.hide();
    #[cfg(any(windows, unix))]
    if let Some(shared) = &instance.mcp_shared {
        crate::mcp::unregister(shared.handle);
    }
}

/// `ferritron_sidechain_api` (`ferritron.sidechain.v1`): the sidechain buffers of
/// the next block, handed over on the audio thread right before `process`
/// (`n == 0` or a NULL array: not connected). The buffers must stay valid
/// until `process` returns.
#[repr(C)]
pub struct SidechainApi {
    pub set: unsafe extern "C" fn(*mut c_void, *const *const f32, u32),
}

/// `ferritron_layout_api` (`ferritron.layout.v1`): the speaker of each channel
/// of the main bus (`Speaker::id`), set by shims before `activate` whenever the host
/// changes the arrangement.
#[repr(C)]
pub struct LayoutApi {
    pub set: unsafe extern "C" fn(*mut c_void, *const u32, u32),
}

/// Sidechain buffers for the next block.
pub(crate) struct SidechainSlot {
    ptrs: [*const f32; MAX_CHANNELS],
    len: usize,
}

unsafe extern "C" fn set_sidechain<P: Process>(ptr: *mut c_void, channels: *const *const f32, n: u32) {
    // SAFETY: audio thread only, between blocks.
    let slot = unsafe { &mut *instance::<P>(ptr).sidechain.get() };
    slot.len = 0;
    if channels.is_null() {
        return;
    }
    for i in 0..(n as usize).min(MAX_CHANNELS) {
        let p = unsafe { *channels.add(i) };
        if p.is_null() {
            slot.len = 0;
            return;
        }
        slot.ptrs[i] = p;
        slot.len = i + 1;
    }
}

unsafe extern "C" fn set_layout<P: Process>(ptr: *mut c_void, speakers: *const u32, n: u32) {
    let mut layout = crate::midi::SpeakerLayout::default();
    if !speakers.is_null() {
        for i in 0..(n as usize).min(MAX_CHANNELS) {
            layout.set(i, crate::midi::Speaker::from_id(unsafe { *speakers.add(i) }));
        }
    }
    // SAFETY: main thread, while the plugin is not processing.
    unsafe { *instance::<P>(ptr).layout.get() = layout };
}

/// [`MainApi::take_requests`]: the latency changed; ask the host to take the new value.
pub const REQUEST_LATENCY: u32 = 1 << 0;
/// [`MainApi::take_requests`]: many parameter values changed at once (a preset); tell the host.
pub const REQUEST_VALUES: u32 = 1 << 1;
/// Codes from `InstanceHandle::notify` wait. `take_requests` runs their handler itself (it is
/// called on the main thread) and never returns this bit, so shims need not know it.
pub const REQUEST_NOTIFY: u32 = 1 << 31;

/// `ferritron_main_api` (`ferritron.main.v1`): work the plugin asked of the
/// host's main thread from other threads (latency changes, parameter values
/// changed in bulk). Shims poll it from the main thread on a timer (or after
/// the host's callback request) and act on the bits.
#[repr(C)]
pub struct MainApi {
    /// Returns and clears the `REQUEST_*` bits. `main`
    pub take_requests: unsafe extern "C" fn(*mut c_void) -> u32,
    /// Whether any request is waiting; does not clear them. [audio, any]
    pub pending: unsafe extern "C" fn(*mut c_void) -> bool,
}

unsafe extern "C" fn pending_requests<P: Process>(ptr: *mut c_void) -> bool {
    let link = &unsafe { instance::<P>(ptr) }.handle.0;
    link.polls_on_demand.store(true, Ordering::Relaxed);
    link.main_requests.load(Ordering::Acquire) != 0
}

unsafe extern "C" fn take_requests<P: Process>(ptr: *mut c_void) -> u32 {
    let handle = &unsafe { instance::<P>(ptr) }.handle;
    handle.0.last_poll.store(crate::instance::clock_ms(), Ordering::Release);
    handle.0.polled.store(true, Ordering::Release);
    let mut bits = handle.0.main_requests.swap(0, Ordering::AcqRel);
    if bits & REQUEST_NOTIFY != 0 {
        handle.deliver_notifications();
        // What the delivery asked for (a preset's values) goes to the host in this same poll;
        // codes notified meanwhile wait for the next one.
        let more = handle.0.main_requests.swap(0, Ordering::AcqRel);
        if more & REQUEST_NOTIFY != 0 {
            handle.0.main_requests.fetch_or(REQUEST_NOTIFY, Ordering::AcqRel);
        }
        bits |= more;
    }
    bits & !REQUEST_NOTIFY
}
pub const EXT_DESCRIBE: &std::ffi::CStr = c"ferritron.describe.v1";

/// `ferritron_describe_api` (`ferritron.describe.v1`): the plugin as JSON
/// (see the `describe` module): parameters with their keys, kinds, ranges
/// and localized names, and the editor's messages.
#[repr(C)]
pub struct DescribeApi {
    /// Writes the UTF-8 JSON into `buf` if it fits in `cap`; returns its full length.
    pub describe: unsafe extern "C" fn(*mut u8, u32) -> u32,
}

unsafe extern "C" fn describe<P: Process + PluginMeta>(buf: *mut u8, cap: u32) -> u32 {
    let json = guarded(crate::describe::json::<P>).unwrap_or_default();
    if !buf.is_null() && json.len() <= cap as usize {
        unsafe { std::ptr::copy_nonoverlapping(json.as_ptr(), buf, json.len()) };
    }
    json.len() as u32
}

/// `ferritron_sleep_api`: silence (see [`Tail`]). Shims report the tail to
/// the host and tell it when the output is silence.
#[repr(C)]
pub struct SleepApi {
    /// How long the output may go on after the input fell silent, in
    /// samples; [`TAIL_UNKNOWN`] when the plugin finds out by itself (or never stops).
    pub tail: unsafe extern "C" fn(*mut c_void) -> u32,
    /// The last `process` call slept: the output is silence.
    pub silent: unsafe extern "C" fn(*mut c_void) -> bool,
}

/// [`SleepApi::tail`] when no length is known.
pub const TAIL_UNKNOWN: u32 = u32::MAX;

unsafe extern "C" fn tail_of<P: Process>(ptr: *mut c_void) -> u32 {
    unsafe { instance::<P>(ptr) }.tail.load(Ordering::Relaxed)
}

unsafe extern "C" fn silent_of<P: Process>(ptr: *mut c_void) -> bool {
    unsafe { instance::<P>(ptr) }.silent.load(Ordering::Relaxed)
}

/// What hosts are told of a tail (the plugin's latency included).
fn tail_samples(tail: Tail, latency: u32) -> u32 {
    match tail {
        Tail::Samples(n) => n.saturating_add(latency).min(TAIL_UNKNOWN - 1),
        Tail::Auto | Tail::Infinite => TAIL_UNKNOWN,
    }
}

/// `ferritron_ara_api`: ARA 2 (extension `ferritron.ara.v1`), implemented by
/// the `ferritron-ara` crate. ARA types cross as plain pointers.
#[repr(C)]
pub struct AraApi {
    /// `const ARAFactory*`.
    pub get_factory: unsafe extern "C" fn() -> *const c_void,
    /// Binds an instance to a document controller (`ARADocumentControllerRef`)
    /// with ARA roles; returns `const ARAPlugInExtensionInstance*`.
    pub bind: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32) -> *const c_void,
    /// Rendering starts (`true`, with sample rate, channels and maximum block) or stops.
    pub set_rendering: unsafe extern "C" fn(*mut c_void, bool, f64, u32, u32),
    /// Renders the instance's playback regions into the channels at song
    /// position `position` (samples), before `process`. Returns false if the
    /// instance is not a playback renderer (the channels are left alone).
    pub render: unsafe extern "C" fn(*mut c_void, *const *mut f32, u32, u32, i64, bool) -> bool,
    /// Called by Ferritron when the instance is destroyed (not by shims).
    pub release: unsafe extern "C" fn(*mut c_void),
}

/// The plugin of instance `ptr` (for extensions such as ARA).
///
/// # Safety
/// `ptr` must be an instance of `P` created by this table, used on the audio
/// thread (or where the host does not process at the same time).
#[doc(hidden)]
pub unsafe fn plugin_mut<'a, P: Process>(ptr: *mut c_void) -> &'a mut P {
    unsafe { &mut *instance::<P>(ptr).plugin.get() }
}

/// The parameters of instance `ptr` (for extensions such as ARA).
///
/// # Safety
/// `ptr` must be an instance of `P` created by this table.
#[doc(hidden)]
pub unsafe fn params_of<'a, P: Process>(ptr: *mut c_void) -> &'a P::Params {
    unsafe { &instance::<P>(ptr).params }
}

/// The handle of instance `ptr` (for extensions such as ARA).
///
/// # Safety
/// `ptr` must be an instance of `P` created by this table, and the reference
/// must not be used after the instance is destroyed.
#[doc(hidden)]
pub unsafe fn handle_of<'a, P: Process>(ptr: *mut c_void) -> &'a crate::InstanceHandle {
    unsafe { &instance::<P>(ptr).handle }
}

/// A slot an extension may use for per-instance data (ARA's plug-in extension).
///
/// # Safety
/// `ptr` must be an instance of `P` created by this table.
#[doc(hidden)]
pub unsafe fn extension_slot<'a, P: Process>(ptr: *mut c_void) -> &'a std::sync::atomic::AtomicPtr<c_void> {
    unsafe { &instance::<P>(ptr).extension }
}

/// The instance's [`InstanceId`](crate::InstanceId), saved as two 32-bit halves.
const INSTANCE_ID_HIGH: u32 = crate::__private::reserved_id("ferritron.instance.high");
const INSTANCE_ID_LOW: u32 = crate::__private::reserved_id("ferritron.instance.low");

/// The editor size the user chose, saved with the project (logical pixels).
const EDITOR_WIDTH: u32 = crate::__private::reserved_id("ferritron.editor.width");
const EDITOR_HEIGHT: u32 = crate::__private::reserved_id("ferritron.editor.height");

/// `ferritron_editor_size_api`: the editor size kept in the plugin state, so
/// a reopened project opens the editor at the size the user left it.
#[repr(C)]
pub struct EditorSizeApi {
    pub get: unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> bool,
    pub set: unsafe extern "C" fn(*mut c_void, u32, u32),
}

unsafe extern "C" fn get_editor_size<P: Process>(ptr: *mut c_void, width: *mut u32, height: *mut u32) -> bool {
    let size = unsafe { instance::<P>(ptr) }.editor_size.load(Ordering::Relaxed);
    if size == 0 || width.is_null() || height.is_null() {
        return false;
    }
    unsafe {
        *width = (size >> 32) as u32;
        *height = size as u32;
    }
    true
}

unsafe extern "C" fn set_editor_size<P: Process>(ptr: *mut c_void, width: u32, height: u32) {
    let size = if width == 0 || height == 0 { 0 } else { (u64::from(width) << 32) | u64::from(height) };
    unsafe { instance::<P>(ptr) }.editor_size.store(size, Ordering::Relaxed);
}

/// `ferritron_host_api` (`ferritron.host.v1`).
#[repr(C)]
pub struct HostApi {
    pub attach: unsafe extern "C" fn(*mut c_void, *const crate::instance::HostCallbacks),
    pub set_track_name: unsafe extern "C" fn(*mut c_void, FerritronStr),
}

/// `ferritron_host_api2` (`ferritron.host.v2`): edits as gestures, waking.
#[repr(C)]
pub struct HostApi2 {
    pub attach: unsafe extern "C" fn(*mut c_void, *const crate::instance::HostCallbacks2),
}

/// `ferritron_track_api` (`ferritron.track.v1`): the track the instance sits on.
#[repr(C)]
pub struct TrackApi {
    pub set_track_info: unsafe extern "C" fn(*mut c_void, *const crate::instance::TrackInfoC),
}

/// `ferritron_transport_api` (`ferritron.transport.v1`): the host's transport,
/// handed over on the audio thread right before each `process`.
#[repr(C)]
pub struct TransportApi {
    pub set: unsafe extern "C" fn(*mut c_void, *const crate::transport::TransportC),
}

unsafe extern "C" fn set_transport<P: Process>(ptr: *mut c_void, transport: *const crate::transport::TransportC) {
    // SAFETY: only the audio thread calls this and `process`, one after the other.
    let slot = unsafe { &mut *instance::<P>(ptr).transport.get() };
    *slot = unsafe { transport.as_ref() }.copied().unwrap_or_default();
}

/// `ferritron_render_api`: offline rendering. [`Context::quality`] follows
/// at once; [`Setup::offline`] and [`Setup::quality`] at the next `activate`.
#[repr(C)]
pub struct RenderApi {
    pub set_offline: unsafe extern "C" fn(*mut c_void, bool),
}

/// `ferritron_latency_api`: the delay the host should compensate, in samples.
#[repr(C)]
pub struct LatencyApi {
    pub get: unsafe extern "C" fn(*mut c_void) -> u32,
}

struct Latency<P>(std::marker::PhantomData<P>);

impl<P: Process> Latency<P> {
    const API: LatencyApi = LatencyApi { get: latency::<P> };
    const RENDER: RenderApi = RenderApi { set_offline: set_offline::<P> };
    const EDITOR_SIZE: EditorSizeApi = EditorSizeApi { get: get_editor_size::<P>, set: set_editor_size::<P> };
    const SLEEP: SleepApi = SleepApi { tail: tail_of::<P>, silent: silent_of::<P> };
    const HOST: HostApi = HostApi { attach: attach_host::<P>, set_track_name: set_track_name::<P> };
    const HOST2: HostApi2 = HostApi2 { attach: attach_host2::<P> };
    const TRACK: TrackApi = TrackApi { set_track_info: set_track_info::<P> };
    const TRANSPORT: TransportApi = TransportApi { set: set_transport::<P> };
    const SIDECHAIN: SidechainApi = SidechainApi { set: set_sidechain::<P> };
    const LAYOUT: LayoutApi = LayoutApi { set: set_layout::<P> };
    const MAIN: MainApi = MainApi { take_requests: take_requests::<P>, pending: pending_requests::<P> };
    const VOICES: VoicesApi = VoicesApi { capacity: voice_capacity::<P> };
    const ROLE: RoleApi = RoleApi { struct_size: size_of::<RoleApi>() as u32, set_role: set_role::<P> };
}

impl<P: Process + PluginMeta> Latency<P> {
    const DESCRIBE: DescribeApi = DescribeApi { describe: describe::<P> };
}

unsafe extern "C" fn attach_host<P: Process>(ptr: *mut c_void, host: *const crate::instance::HostCallbacks) {
    let instance = unsafe { instance::<P>(ptr) };
    if let Some(host) = unsafe { host.as_ref() } {
        instance.handle.0.attach_v1(*host);
    }
}

unsafe extern "C" fn attach_host2<P: Process>(ptr: *mut c_void, host: *const crate::instance::HostCallbacks2) {
    let instance = unsafe { instance::<P>(ptr) };
    if let Some(host) = unsafe { host.as_ref() } {
        instance.handle.0.attach_v2(*host);
    }
}

unsafe extern "C" fn set_track_name<P: Process>(ptr: *mut c_void, name: FerritronStr) {
    let instance = unsafe { instance::<P>(ptr) };
    if let Some(name) = unsafe { name.to_str() } {
        instance.handle.0.set_track_name(name);
    }
}

unsafe extern "C" fn set_track_info<P: Process>(ptr: *mut c_void, info: *const crate::instance::TrackInfoC) {
    use crate::instance::{PluginLocation, TRACK_HAS_CHANNELS, TRACK_HAS_COLOR, TRACK_HAS_INDEX, TRACK_HAS_KIND, TRACK_HAS_LOCATION, TRACK_HAS_NAME, TrackKind};
    let instance = unsafe { instance::<P>(ptr) };
    let Some(info) = (unsafe { info.as_ref() }) else { return };
    let name = if info.flags & TRACK_HAS_NAME != 0 { unsafe { info.name.to_str() }.filter(|n| !n.is_empty()).map(str::to_string) } else { None };
    instance.handle.0.set_track(crate::TrackInfo {
        name,
        color: (info.flags & TRACK_HAS_COLOR != 0).then_some(info.color),
        index: (info.flags & TRACK_HAS_INDEX != 0).then_some(info.index),
        kind: (info.flags & TRACK_HAS_KIND != 0)
            .then(|| match info.kind {
                1 => Some(TrackKind::Return),
                2 => Some(TrackKind::Bus),
                3 => Some(TrackKind::Master),
                0 => Some(TrackKind::Track),
                _ => None,
            })
            .flatten(),
        location: (info.flags & TRACK_HAS_LOCATION != 0)
            .then(|| match info.location {
                0 => Some(PluginLocation::PreFader),
                1 => Some(PluginLocation::PostFader),
                2 => Some(PluginLocation::Panner),
                _ => None,
            })
            .flatten(),
        channels: (info.flags & TRACK_HAS_CHANNELS != 0).then_some(info.channels),
    });
}

unsafe extern "C" fn set_offline<P: Process>(ptr: *mut c_void, offline: bool) {
    unsafe { instance::<P>(ptr) }.offline.store(offline, Ordering::Relaxed);
}

unsafe extern "C" fn latency<P: Process>(ptr: *mut c_void) -> u32 {
    unsafe { instance::<P>(ptr) }.latency.load(Ordering::Relaxed)
}

/// `ferritron_executor`: a host thread pool offered by the shim for one
/// `process` call. `run` executes `task(data, 0..count)` and returns once all
/// tasks finished, or returns false without running anything.
#[repr(C)]
pub struct Executor {
    pub ctx: *mut c_void,
    pub run: unsafe extern "C" fn(*mut c_void, u32, unsafe extern "C" fn(*mut c_void, u32), *mut c_void) -> bool,
}

/// `ferritron_view_host`: callbacks from a view to the shim (UI thread).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ViewHost {
    pub ctx: *mut c_void,
    pub get_param: unsafe extern "C" fn(*mut c_void, u32) -> f64,
    pub begin_edit: unsafe extern "C" fn(*mut c_void, u32),
    pub perform_edit: unsafe extern "C" fn(*mut c_void, u32, f64),
    pub end_edit: unsafe extern "C" fn(*mut c_void, u32),
    /// Asks the host window to become `width` x `height` physical pixels (the
    /// host answers through `ViewApi::set_size`); `None` where the format cannot.
    pub request_resize: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> bool>,
}

/// `ferritron_view_api`: the plugin's editor (extension `ferritron.view.v1`).
/// The sizes here are logical; `open` and `set_size` take physical pixels
/// plus the scale factor.
#[repr(C)]
pub struct ViewApi {
    pub width: u32,
    pub height: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub open: unsafe extern "C" fn(*mut c_void, *const ViewHost, u32, u32, f64) -> *mut c_void,
    pub close: unsafe extern "C" fn(*mut c_void),
    pub set_size: unsafe extern "C" fn(*mut c_void, u32, u32, f64),
}

/// The `ferritron_plugin_api` function table.
/// The function table of `ferritron_abi.h` (`ferritron_plugin_api`): what
/// format adapters call. Its fields are public for adapters written in Rust
/// (Audio Unit); the C header documents each entry.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginApi {
    pub struct_size: u32,
    pub abi_version: u32,
    pub info: *const PluginInfoC,
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(*mut c_void),
    pub param_count: unsafe extern "C" fn() -> u32,
    pub param_info: unsafe extern "C" fn(u32, *mut ParamInfoC) -> bool,
    pub get_param: unsafe extern "C" fn(*mut c_void, u32) -> f64,
    pub set_param: unsafe extern "C" fn(*mut c_void, u32, f64) -> bool,
    pub to_plain: unsafe extern "C" fn(u32, f64) -> f64,
    pub to_normalized: unsafe extern "C" fn(u32, f64) -> f64,
    pub format_param: unsafe extern "C" fn(u32, f64, *mut u8, u32) -> i32,
    pub parse_param: unsafe extern "C" fn(u32, FerritronStr, *mut f64) -> bool,
    pub activate: unsafe extern "C" fn(*mut c_void, f64, u32) -> bool,
    pub process: unsafe extern "C" fn(*mut c_void, *const ProcessData) -> bool,
    pub save_state: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> u32,
    pub load_state: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> bool,
    pub get_extension: unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void,
    pub reset: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: only points to immutable 'static data and functions.
unsafe impl Sync for PluginApi {}
unsafe impl Send for PluginApi {}

impl PluginApi {
    pub const fn of<P: Process + PluginMeta>(info: &'static PluginInfoC) -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            info,
            create: create::<P>,
            destroy: destroy::<P>,
            param_count: param_count::<P>,
            param_info: param_info::<P>,
            get_param: get_param::<P>,
            set_param: set_param::<P>,
            to_plain: to_plain::<P>,
            to_normalized: to_normalized::<P>,
            format_param: format_param::<P>,
            parse_param: parse_param::<P>,
            activate: activate::<P>,
            process: process::<P>,
            save_state: save_state::<P>,
            load_state: load_state::<P>,
            get_extension: get_extension::<P>,
            reset: reset::<P>,
        }
    }
}

/// Clears the plugin's state (delay lines, voices, smoothing) without reallocating. Audio thread;
/// never while `process` runs.
pub(crate) unsafe extern "C" fn reset<P: Process + PluginMeta>(ptr: *mut c_void) {
    let instance = unsafe { instance::<P>(ptr) };
    // SAFETY: hosts do not call reset while process runs.
    let plugin = unsafe { &mut *instance.plugin.get() };
    if guarded(|| plugin.reset()).is_none() {
        instance.failed.store(true, Ordering::Relaxed);
        return;
    }
    instance.smooth.snap(&*instance.params);
    unsafe { *instance.sleep.get() = Sleep::default() };
    instance.bypassfx.snap(instance.bypass.get() >= 0.5);
}

pub(crate) unsafe extern "C" fn get_extension<P: Process + PluginMeta>(id: *const std::ffi::c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = unsafe { std::ffi::CStr::from_ptr(id) };
    if id == EXT_LATENCY {
        return &Latency::<P>::API as *const LatencyApi as *const c_void;
    }
    if id == EXT_RENDER {
        return &Latency::<P>::RENDER as *const RenderApi as *const c_void;
    }
    if id == EXT_SLEEP {
        return &Latency::<P>::SLEEP as *const SleepApi as *const c_void;
    }
    if id == EXT_ARA {
        return P::ara().map_or(std::ptr::null(), |ara| ara as *const AraApi as *const c_void);
    }
    if id == EXT_HOST {
        return &Latency::<P>::HOST as *const HostApi as *const c_void;
    }
    if id == EXT_HOST2 {
        return &Latency::<P>::HOST2 as *const HostApi2 as *const c_void;
    }
    if id == EXT_TRACK {
        return &Latency::<P>::TRACK as *const TrackApi as *const c_void;
    }
    if id == EXT_SIDECHAIN {
        return &Latency::<P>::SIDECHAIN as *const SidechainApi as *const c_void;
    }
    if id == EXT_LAYOUT {
        return &Latency::<P>::LAYOUT as *const LayoutApi as *const c_void;
    }
    if id == EXT_MAIN {
        return &Latency::<P>::MAIN as *const MainApi as *const c_void;
    }
    if id == EXT_VOICES {
        return if P::VOICES > 0 { &Latency::<P>::VOICES as *const VoicesApi as *const c_void } else { std::ptr::null() };
    }
    if id == EXT_ROLE {
        return &Latency::<P>::ROLE as *const RoleApi as *const c_void;
    }
    if id == EXT_TRANSPORT {
        return &Latency::<P>::TRANSPORT as *const TransportApi as *const c_void;
    }
    if id == EXT_DESCRIBE {
        return &Latency::<P>::DESCRIBE as *const DescribeApi as *const c_void;
    }
    match P::view() {
        Some(view) if id == EXT_VIEW => crate::isolation::editor(view, P::VIEW_PROCESS),
        Some(_) if id == EXT_EDITOR_SIZE => &Latency::<P>::EDITOR_SIZE as *const EditorSizeApi as *const c_void,
        _ => std::ptr::null(),
    }
}

struct Instance<P: Process> {
    params: std::sync::Arc<P::Params>,
    /// What handles, editors and the host link share.
    handle: crate::InstanceHandle,
    /// Touched only by `activate` (main thread) and `process` (audio
    /// thread), which hosts never run concurrently.
    plugin: UnsafeCell<P>,
    bypass: ParamCell,
    sample_rate: AtomicU64,
    /// Set when plugin code panicked; the instance then outputs silence.
    failed: AtomicBool,
    /// `Process::latency` as of the last `activate`.
    latency: AtomicU32,
    /// Offline rendering, as last told by the shim (used at `activate`).
    offline: AtomicBool,
    /// The "AI Access" switch (plugins with `[mcp]`).
    mcp: ParamCell,
    /// The "Quality" switch (plugins with `[quality]`).
    quality: ParamCell,
    /// Touched only by `process` (audio thread) and `activate`.
    sleep: UnsafeCell<Sleep>,
    /// Parameter versions the plugin has seen (audio thread only).
    seen: crate::plugin::Seen,
    smooth: crate::smooth::SmoothBank,
    /// Crossfade and delay of the built-in bypass (audio thread only).
    bypassfx: crate::bypass::Bypass,
    /// Events the plugin writes during a block (audio thread only).
    out: crate::plugin::OutQueue,
    scratch: crate::plugin::Scratch,
    /// A state load asks the audio thread to jump the smoothers to the new values.
    snap: AtomicBool,
    /// The sidechain buffers of the next block (audio thread only).
    sidechain: UnsafeCell<SidechainSlot>,
    /// The main bus's speakers (set before `activate`).
    layout: UnsafeCell<crate::midi::SpeakerLayout>,
    /// The host's transport for the next block (audio thread only).
    transport: UnsafeCell<crate::transport::TransportC>,
    /// The tail as hosts are told ([`tail_samples`]), as of the last block.
    tail: crate::util::CachePadded<AtomicU32>,
    /// The last block slept.
    silent: crate::util::CachePadded<AtomicBool>,
    /// Editor size as `width << 32 | height`; 0 until the user resizes.
    editor_size: AtomicU64,
    /// Per-instance data of an extension (ARA).
    extension: std::sync::atomic::AtomicPtr<c_void>,
    /// The instance's worker (`#[ferritron::plugin(worker = ...)]`), registered with the worker thread.
    worker: Option<std::sync::Arc<crate::worker::Slot>>,
    #[cfg(any(windows, unix))]
    mcp_shared: Option<std::sync::Arc<crate::mcp::Shared>>,
    /// `Harness::set_automation_mode`: the mode (u8::MAX: the project file's) and control period.
    automation: std::sync::atomic::AtomicU8,
    control_block: AtomicU32,
}

impl<P: Process> Instance<P> {
    fn automation_mode(&self) -> u8
    where
        P: PluginMeta,
    {
        match self.automation.load(Ordering::Relaxed) {
            u8::MAX => P::AUTOMATION,
            mode => mode,
        }
    }

    fn control_period(&self) -> u32
    where
        P: PluginMeta,
    {
        match self.control_block.load(Ordering::Relaxed) {
            0 => P::CONTROL_BLOCK,
            n => n,
        }
    }
}

/// Changes how parameter changes are applied for one instance (tests: `Harness::set_automation_mode`).
/// `mode`: 0 control period, 1 every change, 2 events; `control_block` 0 keeps the project file's.
///
/// # Safety
/// `ptr` must come from `create::<P>()`; not while the instance processes.
pub unsafe fn set_automation<P: Process + PluginMeta>(ptr: *mut c_void, mode: u8, control_block: u32) {
    let instance = unsafe { instance::<P>(ptr) };
    instance.automation.store(mode, Ordering::Relaxed);
    instance.control_block.store(control_block, Ordering::Relaxed);
}

/// What one instance holds in memory, as far as Ferritron knows (`Harness::memory_report`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryReport {
    /// Ferritron's per-instance structure, the plugin value inside it included.
    pub instance: usize,
    /// The plugin type itself (`size_of::<P>()`, inside `instance`; its own heap is not counted).
    pub plugin: usize,
    /// The parameter set (shared with editors and handles).
    pub params: usize,
    /// Heap of the framework's audio-thread helpers: change tables, smoothers,
    /// bypass delay and crossfade, scratch memory.
    pub seen: usize,
    pub smoothing: usize,
    pub bypass: usize,
    pub scratch: usize,
}

impl MemoryReport {
    /// Everything Ferritron holds for the instance, the plugin value included.
    pub fn total(&self) -> usize {
        self.instance + self.params + self.seen + self.smoothing + self.bypass + self.scratch
    }

    /// What the framework adds: `total` without the plugin value.
    pub fn framework(&self) -> usize {
        self.total() - self.plugin
    }
}

/// The memory report of one instance.
///
/// # Safety
/// `ptr` must come from `create::<P>()`; not while the instance processes.
pub unsafe fn memory_of<P: Process + PluginMeta>(ptr: *mut c_void) -> MemoryReport {
    let instance = unsafe { instance::<P>(ptr) };
    MemoryReport {
        instance: size_of::<Instance<P>>(),
        plugin: size_of::<P>(),
        params: size_of::<P::Params>(),
        seen: instance.seen.heap_bytes(),
        smoothing: instance.smooth.heap_bytes(),
        bypass: instance.bypassfx.heap_bytes(),
        scratch: instance.scratch.heap_bytes(),
    }
}

/// Sets flush-to-zero / denormals-are-zero for the current thread while alive,
/// so denormal numbers cannot slow processing down.
pub(crate) struct DenormalGuard {
    #[cfg(target_arch = "x86_64")]
    saved: u32,
    #[cfg(target_arch = "aarch64")]
    saved: u64,
}

impl DenormalGuard {
    pub(crate) fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            const FTZ_DAZ: u32 = 0x8040;
            let mut saved = 0u32;
            // SAFETY: reading and writing MXCSR only changes floating-point modes of this thread.
            unsafe {
                std::arch::asm!("stmxcsr [{}]", in(reg) &mut saved, options(nostack));
                let flushed = saved | FTZ_DAZ;
                std::arch::asm!("ldmxcsr [{}]", in(reg) &flushed, options(nostack, readonly));
            }
            Self { saved }
        }
        // FPCR.FZ (bit 24) flushes denormals to zero. Type-checked only for
        // aarch64 here; not run on Apple Silicon or an Android phone.
        #[cfg(target_arch = "aarch64")]
        {
            let saved: u64;
            // SAFETY: FPCR only holds floating-point modes of this thread.
            unsafe {
                std::arch::asm!("mrs {}, fpcr", out(reg) saved, options(nostack, nomem));
                let flushed = saved | (1 << 24);
                std::arch::asm!("msr fpcr, {}", in(reg) flushed, options(nostack, nomem));
            }
            Self { saved }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        Self {}
    }
}

impl Drop for DenormalGuard {
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &self.saved, options(nostack, readonly));
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) self.saved, options(nostack, nomem));
        }
    }
}

/// Runs plugin code, turning a panic into `None` instead of unwinding into the host.
fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

/// `denormalize` that survives a panic in a plugin's own scale (`scale = "custom"`): the
/// value stays where it was. `catch_unwind` costs nothing unless something panics.
fn denormalize_or(info: &ParamInfo, normalized: f64, fallback: f64) -> f64 {
    guarded(|| info.denormalize(normalized)).unwrap_or(fallback)
}

/// `normalize` that survives a panic in a plugin's own scale (`scale = "custom"`).
fn normalize_or(info: &ParamInfo, plain: f64, fallback: f64) -> f64 {
    guarded(|| info.normalize(plain)).unwrap_or(fallback)
}

unsafe fn instance<'a, P: Process>(ptr: *mut c_void) -> &'a Instance<P> {
    unsafe { &*(ptr as *const Instance<P>) }
}

/// Index of the AI Access switch (`[mcp]`).
fn mcp_index<P: Process + PluginMeta>() -> Option<usize> {
    P::MCP.then(|| P::Params::infos().len() + 1)
}

/// Index of the Quality switch (`[quality]`), after AI Access.
fn quality_index<P: Process + PluginMeta>() -> Option<usize> {
    P::QUALITY.then(|| P::Params::infos().len() + 1 + P::MCP as usize)
}

/// Index of the Program parameter (`[presets]`), last.
fn program_index<P: Process + PluginMeta>() -> Option<usize> {
    P::program_param().map(|_| P::Params::infos().len() + 1 + P::MCP as usize + P::QUALITY as usize)
}

/// Parameter at `index`: the plugin's own, then bypass, then (with `[mcp]`)
/// the AI Access switch, then (with `[quality]`) the Quality switch, then (with
/// `[presets]`) the Program parameter.
fn info_at<P: Process + PluginMeta>(index: usize) -> Option<&'static ParamInfo> {
    let infos = P::Params::infos();
    match index {
        i if i == infos.len() => Some(&BYPASS),
        i if Some(i) == mcp_index::<P>() => Some(&MCP_SWITCH),
        i if Some(i) == quality_index::<P>() => Some(&QUALITY),
        i if Some(i) == program_index::<P>() => P::program_param(),
        i => infos.get(i),
    }
}

fn lookup<P: Process + PluginMeta>(id: u32) -> Option<(usize, &'static ParamInfo)> {
    if id == BYPASS.id {
        return Some((P::Params::infos().len(), &BYPASS));
    }
    if let Some(index) = mcp_index::<P>().filter(|_| id == MCP_SWITCH.id) {
        return Some((index, &MCP_SWITCH));
    }
    if let Some(index) = quality_index::<P>().filter(|_| id == QUALITY.id) {
        return Some((index, &QUALITY));
    }
    if id == crate::__private::PROGRAM_ID {
        return Some((program_index::<P>()?, P::program_param()?));
    }
    let index = P::Params::index_of(id)?;
    Some((index, &P::Params::infos()[index]))
}

/// Every parameter hosts see, built-in switches included, with its index:
/// the plugin's own, bypass, AI Access (`[mcp]`), Quality (`[quality]`).
#[doc(hidden)]
pub fn all_params<P: Process + PluginMeta>() -> impl Iterator<Item = (usize, &'static ParamInfo)> {
    (0..unsafe { param_count::<P>() } as usize).filter_map(|i| info_at::<P>(i).map(|info| (i, info)))
}

impl<P: Process + PluginMeta> Instance<P> {
    fn cell(&self, index: usize) -> &ParamCell {
        let n = P::Params::infos().len();
        match index {
            i if i == n => &self.bypass,
            i if Some(i) == mcp_index::<P>() => &self.mcp,
            i if Some(i) == quality_index::<P>() => &self.quality,
            i if Some(i) == program_index::<P>() => &self.handle.0.program,
            i => self.params.cell(i),
        }
    }

    /// The quality to process at now: the user's switch, or Auto (best
    /// while the host renders offline).
    fn quality(&self) -> Quality {
        let chosen = if P::QUALITY { self.quality.get().round() as i64 } else { 0 };
        match chosen {
            1 => Quality::Normal,
            2 => Quality::Best,
            _ if self.offline.load(Ordering::Relaxed) => Quality::Best,
            _ => Quality::Normal,
        }
    }
}

/// Below this (-120 dBFS) a sample counts as silence.
const QUIET: f32 = 1e-6;

/// [`Tail::Auto`]: how long input and output must stay silent before the plugin sleeps.
const AUTO_HOLD_SECONDS: f64 = 2.0;

/// Whether every sample of the channels is silence. Works in chunks of 64
/// samples and returns at the first loud one, so a busy block costs little.
fn is_quiet(channels: impl IntoIterator<Item = *const f32>, frames: usize) -> bool {
    channels.into_iter().all(|p| {
        // SAFETY: the shim passes `frames` valid samples per channel.
        let samples = unsafe { std::slice::from_raw_parts(p, frames) };
        // A plain maximum per chunk vectorizes; NaN counts as quiet (it is not sound).
        samples.chunks(64).all(|chunk| chunk.iter().fold(0.0f32, |m, s| m.max(s.abs())) <= QUIET)
    })
}

/// When `process` may be skipped (see [`Tail`]).
#[derive(Default)]
struct Sleep {
    asleep: bool,
    /// Samples in a row that met the tail's condition for silence.
    quiet: u64,
    /// Notes held, by channel and key.
    held: [u128; 16],
}

impl Sleep {
    fn holding(&self) -> bool {
        self.held.iter().any(|&keys| keys != 0)
    }

    /// Counts one processed block; returns whether the plugin may sleep now.
    fn update(&mut self, tail: Tail, busy: bool, output_quiet: impl Fn() -> bool, frames: usize, sample_rate: f64, latency: u32) -> bool {
        let quiet = match tail {
            Tail::Infinite => false,
            Tail::Samples(_) => !busy,
            Tail::Auto => !busy && output_quiet(),
        };
        self.quiet = if quiet { self.quiet.saturating_add(frames as u64) } else { 0 };
        let needed = match tail {
            Tail::Infinite => return false,
            Tail::Samples(n) => u64::from(n),
            Tail::Auto => (sample_rate.max(1.0) * AUTO_HOLD_SECONDS) as u64,
        } + u64::from(latency);
        // Never cut off sound, whatever the tail says.
        self.asleep = self.quiet >= needed && output_quiet();
        self.asleep
    }
}

/// What the MCP agent sees of an instance (see `crate::mcp`).
#[cfg(any(windows, unix))]
struct Endpoint<P: Process> {
    instance: *const Instance<P>,
    shared: std::sync::Arc<crate::mcp::Shared>,
}

// SAFETY: the agent only uses the instance's atomics and locks, and the
// endpoint is unregistered before the instance is freed.
#[cfg(any(windows, unix))]
unsafe impl<P: Process> Send for Endpoint<P> {}
#[cfg(any(windows, unix))]
unsafe impl<P: Process> Sync for Endpoint<P> {}

#[cfg(any(windows, unix))]
impl<P: Process + PluginMeta> crate::mcp::Endpoint for Endpoint<P> {
    fn shared(&self) -> &crate::mcp::Shared {
        &self.shared
    }

    fn enabled(&self) -> bool {
        unsafe { &*self.instance }.mcp.get() >= 0.5
    }

    fn sample_rate(&self) -> f64 {
        f64::from_bits(unsafe { &*self.instance }.sample_rate.load(Ordering::Relaxed))
    }

    fn identity(&self) -> (&'static str, &'static str, &'static str) {
        (P::INFO.id, P::INFO.name, P::INFO.vendor)
    }

    fn parameters(&self) -> Vec<serde_json::Value> {
        let instance = unsafe { &*self.instance };
        // The plugin's parameters and bypass; never the AI Access switch.
        (0..=P::Params::infos().len())
            .filter_map(|index| {
                let info = info_at::<P>(index)?;
                let value = instance.cell(index).get();
                Some(serde_json::json!({
                    "key": info.key,
                    "id": info.id,
                    "name": P::localized().param_name(index),
                    "unit": info.unit,
                    "min": denormalize_or(info, 0.0, value),
                    "max": denormalize_or(info, 1.0, value),
                    "steps": info.step_count(),
                    "value": value,
                    "text": P::localized().format(index, info, value),
                }))
            })
            .collect()
    }

    fn set(&self, parameter: &str, value: f64) -> Result<serde_json::Value, String> {
        let instance = unsafe { &*self.instance };
        let index = (0..=P::Params::infos().len())
            .find(|&i| {
                info_at::<P>(i).is_some_and(|info| {
                    info.key == parameter || info.id.to_string() == parameter || P::localized().param_name(i).eq_ignore_ascii_case(parameter)
                })
            })
            .ok_or_else(|| format!("no parameter `{parameter}`"))?;
        let info = info_at::<P>(index).expect("found above");
        let plain = info.sanitize(value);
        instance.cell(index).set(plain);
        // Tell the DAW, as an edit from the editor would.
        self.shared.link.edited(info.id, normalize_or(info, plain, 0.0));
        instance.handle.0.wake.store(true, Ordering::Release);
        Ok(serde_json::json!({ "key": info.key, "value": plain, "text": P::localized().format(index, info, plain) }))
    }
}

pub(crate) unsafe extern "C" fn create<P: Process + PluginMeta>() -> *mut c_void {
    let Some(mut plugin) = guarded(P::default) else { return std::ptr::null_mut() };
    let Some(params) = guarded(|| std::sync::Arc::new(P::Params::default())) else { return std::ptr::null_mut() };
    let link = crate::instance::Link::new::<P>(P::INFO.id, params.clone(), P::presets());
    let handle = crate::InstanceHandle(link.clone());
    // The worker exists before `init`, so `init` can take what the worker hands over.
    let worker = {
        let any: std::sync::Arc<dyn std::any::Any + Send + Sync> = params.clone();
        guarded(|| P::make_worker(&any, &handle)).flatten().map(crate::worker::Slot::new)
    };
    if guarded(|| plugin.init(&handle)).is_none() {
        link.destroy();
        return std::ptr::null_mut();
    }
    if P::PARALLEL {
        crate::parallel::acquire();
    }
    let instance = Instance::<P> {
        params,
        handle,
        plugin: UnsafeCell::new(plugin),
        bypass: ParamCell::new(BYPASS.default),
        sample_rate: AtomicU64::new(44_100f64.to_bits()),
        failed: AtomicBool::new(false),
        latency: AtomicU32::new(0),
        offline: AtomicBool::new(false),
        mcp: ParamCell::new(MCP_SWITCH.default),
        quality: ParamCell::new(QUALITY.default),
        sleep: UnsafeCell::new(Sleep::default()),
        seen: crate::plugin::Seen::new(),
        smooth: crate::smooth::SmoothBank::new(),
        bypassfx: crate::bypass::Bypass::new(),
        out: crate::plugin::OutQueue::new(),
        scratch: crate::plugin::Scratch::new(),
        snap: AtomicBool::new(false),
        transport: UnsafeCell::new(crate::transport::TransportC::default()),
        sidechain: UnsafeCell::new(SidechainSlot { ptrs: [std::ptr::null(); MAX_CHANNELS], len: 0 }),
        layout: UnsafeCell::new(crate::midi::SpeakerLayout::default()),
        tail: crate::util::CachePadded::new(AtomicU32::new(TAIL_UNKNOWN)),
        silent: crate::util::CachePadded::new(AtomicBool::new(false)),
        editor_size: AtomicU64::new(0),
        extension: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        worker,
        #[cfg(any(windows, unix))]
        mcp_shared: P::MCP.then(|| std::sync::Arc::new(crate::mcp::Shared::new(link.clone()))),
        automation: std::sync::atomic::AtomicU8::new(u8::MAX),
        control_block: AtomicU32::new(0),
    };
    if let Some(slot) = &instance.worker {
        crate::worker::register(slot);
    }
    let ptr = Box::into_raw(Box::new(instance));
    // Starts the editor's browser now, without waiting for it (`[view] preload`).
    let _ = guarded(|| P::preload_view(true));
    #[cfg(any(windows, unix))]
    if let Some(shared) = unsafe { &(*ptr).mcp_shared } {
        crate::mcp::register(std::sync::Arc::new(Endpoint::<P> { instance: ptr, shared: shared.clone() }));
    }
    ptr.cast()
}

pub(crate) unsafe extern "C" fn destroy<P: Process + PluginMeta>(ptr: *mut c_void) {
    if !ptr.is_null() {
        #[cfg(any(windows, unix))]
        if let Some(shared) = unsafe { &(*(ptr as *const Instance<P>)).mcp_shared } {
            crate::mcp::unregister(shared.handle);
        }
        if let Some(ara) = P::ara() {
            unsafe { (ara.release)(ptr) };
        }
        let instance = unsafe { Box::from_raw(ptr as *mut Instance<P>) };
        // The worker leaves the pool first (its running call finishes), then is dropped here, not on the audio thread.
        if let Some(slot) = &instance.worker {
            crate::worker::unregister(slot);
        }
        instance.handle.0.destroy();
        // Plugin destructors are plugin code too.
        let _ = guarded(move || drop(instance));
        let _ = guarded(|| P::preload_view(false));
        if P::PARALLEL {
            crate::parallel::release();
        }
    }
}

pub(crate) unsafe extern "C" fn param_count<P: Process + PluginMeta>() -> u32 {
    P::Params::infos().len() as u32 + 1 + P::MCP as u32 + P::QUALITY as u32 + P::program_param().is_some() as u32
}

/// `param_info` of plugin `P` (for the description).
pub(crate) unsafe fn param_info_of<P: Process + PluginMeta>(index: u32, out: *mut ParamInfoC) -> bool {
    unsafe { param_info::<P>(index, out) }
}

unsafe extern "C" fn param_info<P: Process + PluginMeta>(index: u32, out: *mut ParamInfoC) -> bool {
    let Some(info) = info_at::<P>(index as usize) else { return false };
    if out.is_null() {
        return false;
    }
    let mut flags = match info.kind {
        _ if info.id == BYPASS.id => PARAM_BYPASS,
        _ if info.id == MCP_SWITCH.id => PARAM_NOT_AUTOMATABLE,
        _ if info.id == QUALITY.id => PARAM_LIST | PARAM_NOT_AUTOMATABLE,
        _ if info.id == crate::__private::PROGRAM_ID => PARAM_LIST | PARAM_NOT_AUTOMATABLE | PARAM_HIDDEN | PARAM_PROGRAM,
        ParamKind::Enum { .. } => PARAM_LIST,
        _ => 0,
    };
    if (index as usize) < P::Params::infos().len() {
        let options = P::Params::options(index as usize);
        if !options.automatable {
            flags |= PARAM_NOT_AUTOMATABLE;
        }
        if options.hidden {
            flags |= PARAM_HIDDEN;
        }
        if options.poly {
            flags |= PARAM_POLY;
        }
    }
    unsafe {
        out.write(ParamInfoC {
            id: info.id,
            name: FerritronStr::new(if info.id == MCP_SWITCH.id {
                MCP_SWITCH.name
            } else {
                P::localized().name_of(index as usize, info)
            }),
            unit: FerritronStr::new(info.unit),
            step_count: info.step_count(),
            default_normalized: normalize_or(info, info.default, 0.0),
            flags,
        })
    };
    true
}

pub(crate) unsafe extern "C" fn get_param<P: Process + PluginMeta>(ptr: *mut c_void, id: u32) -> f64 {
    let instance = unsafe { instance::<P>(ptr) };
    lookup::<P>(id).map_or(0.0, |(index, info)| {
        let plain = if index < P::Params::infos().len() { instance.handle.0.pending_value(index) } else { None };
        guarded(|| info.normalize(plain.unwrap_or_else(|| instance.cell(index).get()))).unwrap_or(0.0)
    })
}

pub(crate) unsafe extern "C" fn set_param<P: Process + PluginMeta>(ptr: *mut c_void, id: u32, normalized: f64) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    let Some((index, info)) = lookup::<P>(id) else { return false };
    let Some(plain) = guarded(|| info.denormalize(normalized)) else { return false };
    instance.cell(index).set(plain);
    instance.handle.0.bump_params();
    if id == crate::__private::PROGRAM_ID {
        instance.handle.0.check_program();
    }
    true
}

pub(crate) unsafe extern "C" fn to_plain<P: Process + PluginMeta>(id: u32, normalized: f64) -> f64 {
    lookup::<P>(id).and_then(|(_, info)| guarded(|| info.denormalize(normalized))).unwrap_or(0.0)
}

pub(crate) unsafe extern "C" fn to_normalized<P: Process + PluginMeta>(id: u32, plain: f64) -> f64 {
    lookup::<P>(id).and_then(|(_, info)| guarded(|| info.normalize(plain))).unwrap_or(0.0)
}

pub(crate) unsafe extern "C" fn format_param<P: Process + PluginMeta>(id: u32, normalized: f64, buf: *mut u8, cap: u32) -> i32 {
    let Some((index, info)) = lookup::<P>(id) else { return -1 };
    if buf.is_null() {
        return -1;
    }
    // A panic in the plugin's text function is not sent to the host.
    let Some(text) = guarded(|| P::localized().format(index, info, info.denormalize(normalized))) else { return -1 };
    // Truncate at a character boundary.
    let mut len = text.len().min(cap as usize);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), buf, len) };
    len as i32
}

pub(crate) unsafe extern "C" fn parse_param<P: Process + PluginMeta>(id: u32, text: FerritronStr, out: *mut f64) -> bool {
    let Some((index, info)) = lookup::<P>(id) else { return false };
    let Some(text) = (unsafe { text.to_str() }) else { return false };
    match guarded(|| P::localized().parse(index, info, text)).flatten() {
        Some(plain) if !out.is_null() => {
            let Some(normalized) = guarded(|| info.normalize(plain)) else { return false };
            unsafe { *out = normalized };
            true
        }
        _ => false,
    }
}

pub(crate) unsafe extern "C" fn activate<P: Process + PluginMeta>(ptr: *mut c_void, sample_rate: f64, max_block_size: u32) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    // SAFETY: hosts do not process while activating.
    let plugin = unsafe { &mut *instance.plugin.get() };
    let offline = instance.offline.load(Ordering::Relaxed);
    let setup = Setup {
        sample_rate,
        max_block_size: max_block_size as usize,
        offline,
        quality: instance.quality(),
        layout: unsafe { *instance.layout.get() },
        latency: instance.latency.load(Ordering::Relaxed),
    };
    instance.sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
    if let Some(slot) = &instance.worker {
        slot.activated(setup);
    }
    unsafe { *instance.sleep.get() = Sleep::default() };
    instance.seen.prepare(&*instance.params);
    instance.smooth.prepare(&*instance.params, sample_rate);
    // Whatever the values are when the first block comes, start there (no ramp from the defaults).
    instance.snap.store(true, Ordering::Release);
    instance.silent.store(false, Ordering::Relaxed);
    let ok = guarded(|| {
        plugin.activate(&setup);
        plugin.reset();
        let latency = plugin.latency();
        instance.latency.store(latency, Ordering::Relaxed);
        instance.tail.store(tail_samples(plugin.tail(), latency), Ordering::Relaxed);
        let channels = match setup.layout.len() {
            0 => 2,
            n => n,
        };
        instance.bypassfx.prepare(channels, max_block_size as usize, latency, sample_rate);
        instance.scratch.prepare(plugin.scratch(&setup).bytes);
    })
    .is_some();
    instance.failed.store(!ok, Ordering::Relaxed);
    ok
}

/// Most channels `process` accepts (one bus).
pub const MAX_CHANNELS: usize = 32;

pub const EVENT_PARAM: u32 = 0;
pub const EVENT_NOTE_ON: u32 = 1;
pub const EVENT_NOTE_OFF: u32 = 2;
/// A MIDI message that is not a note (`EventData::midi`).
pub const EVENT_MIDI: u32 = 3;
/// (2.0) A host's per-note modulation of a parameter (`EventData::voice`: `target` is the
/// parameter ID, `value` the offset in plain units). CLAP `CLAP_EVENT_PARAM_MOD`.
pub const EVENT_NOTE_MOD: u32 = 4;
/// (2.0) A host's note expression (`EventData::voice`: `target` is the expression,
/// `EXPRESSION_*`, `value` its value). CLAP `CLAP_EVENT_NOTE_EXPRESSION`, VST3 Note Expression.
pub const EVENT_NOTE_EXPRESSION: u32 = 5;
/// (2.0) The voice of a note ended, plugin to host only (`EventData::note`, velocity 0). CLAP
/// `CLAP_EVENT_NOTE_END`. Shims that do not know it ignore it.
pub const EVENT_NOTE_END: u32 = 6;

/// One timed event of the block (`ferritron_event`): a parameter change or a note.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub offset: u32,
    pub kind: u32,
    pub data: EventData,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union EventData {
    pub param: ParamData,
    pub note: NoteData,
    pub midi: MidiData,
    pub voice: VoiceData,
}

/// Which note a per-note event is for, and what it says (`EVENT_NOTE_MOD`,
/// `EVENT_NOTE_EXPRESSION`). `note_id`, `key` and `channel` may each be -1 (any); with all
/// three -1 the event is for every note (CLAP's monophonic modulation).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VoiceData {
    pub note_id: i32,
    pub target: u32,
    pub value: f32,
    pub key: i8,
    pub channel: i8,
    pub reserved: u16,
}

/// A raw MIDI message: `status` is the status byte without the channel.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MidiData {
    pub status: u8,
    pub channel: u8,
    pub data1: u8,
    pub data2: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ParamData {
    pub id: u32,
    pub normalized: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoteData {
    pub note_id: i32,
    pub channel: i16,
    pub key: i16,
    pub velocity: f32,
}

impl Event {
    pub fn param(offset: u32, id: u32, normalized: f64) -> Self {
        Self { offset, kind: EVENT_PARAM, data: EventData { param: ParamData { id, normalized } } }
    }

    pub fn midi(offset: u32, status: u8, channel: u8, data1: u8, data2: u8) -> Self {
        Self { offset, kind: EVENT_MIDI, data: EventData { midi: MidiData { status, channel, data1, data2 } } }
    }

    /// A per-note modulation of parameter `param_id` by `amount` (plain units).
    pub fn note_mod(offset: u32, note_id: i32, key: i8, channel: i8, param_id: u32, amount: f32) -> Self {
        Self { offset, kind: EVENT_NOTE_MOD, data: EventData { voice: VoiceData { note_id, target: param_id, value: amount, key, channel, reserved: 0 } } }
    }

    /// A note expression (`expression` is `ExpressionKind as u32`).
    pub fn note_expression(offset: u32, note_id: i32, key: i8, channel: i8, expression: u32, value: f32) -> Self {
        Self { offset, kind: EVENT_NOTE_EXPRESSION, data: EventData { voice: VoiceData { note_id, target: expression, value, key, channel, reserved: 0 } } }
    }

    pub fn note(offset: u32, on: bool, key: i16, channel: i16, velocity: f32, note_id: i32) -> Self {
        let kind = if on { EVENT_NOTE_ON } else { EVENT_NOTE_OFF };
        Self { offset, kind, data: EventData { note: NoteData { note_id, channel, key, velocity } } }
    }

    /// The voice of a note ended (`EVENT_NOTE_END`, plugin to host).
    pub fn note_end(offset: u32, note_id: i32, channel: i16, key: i16) -> Self {
        Self { offset, kind: EVENT_NOTE_END, data: EventData { note: NoteData { note_id, channel, key, velocity: 0.0 } } }
    }
}

/// Sorts `events` by offset the way the C shims do (`ferritron_sort_events` in
/// `ferritron_events.h`): stable (events at one offset keep their order), O(n log n),
/// one pass when already in order, and no allocation, so shims written in Rust can
/// call it on the audio thread. `scratch` must hold at least `events.len()` events.
pub fn sort_events(events: &mut [Event], scratch: &mut [Event]) {
    let n = events.len();
    if events.windows(2).all(|w| w[0].offset <= w[1].offset) {
        return;
    }
    assert!(scratch.len() >= n, "sort_events: scratch holds {} events, {n} needed", scratch.len());
    let scratch = &mut scratch[..n];
    let mut in_events = true; // where the sorted runs are
    let mut width = 1;
    while width < n {
        let (from, to): (&[Event], &mut [Event]) = if in_events { (&*events, &mut *scratch) } else { (&*scratch, &mut *events) };
        let mut lo = 0;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            let (mut a, mut b, mut k) = (lo, mid, lo);
            while a < mid && b < hi {
                if from[b].offset < from[a].offset {
                    to[k] = from[b];
                    b += 1;
                } else {
                    to[k] = from[a];
                    a += 1;
                }
                k += 1;
            }
            to[k..k + (mid - a)].copy_from_slice(&from[a..mid]);
            k += mid - a;
            to[k..k + (hi - b)].copy_from_slice(&from[b..hi]);
            lo += 2 * width;
        }
        in_events = !in_events;
        width *= 2;
    }
    if !in_events {
        events.copy_from_slice(scratch);
    }
}

/// Notes delivered to one processing call.
const MAX_NOTES: usize = 512;
/// MIDI messages delivered to one processing call.
const MAX_MIDI: usize = 1024;
/// Per-note events (modulation, expression) one block holds.
const MAX_VOICE_EVENTS: usize = 512;
/// Parameter events handed over in `[process] automation = "events"` mode.
const MAX_PARAM_EVENTS: usize = 1024;

/// Everything one `process` call gets (`ferritron_process_data`). `size` is
/// the size the caller built it with, so fields can be added later: a field
/// beyond `size` is not there.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessData {
    pub size: u32,
    pub frames: u32,
    pub num_channels: u32,
    pub num_events: u32,
    pub channels: *const *mut f32,
    /// Sorted by offset.
    pub events: *const Event,
    pub executor: *const Executor,
    /// Events the plugin may write (MIDI out, notifications); null if the caller has no room.
    pub out_events: *mut Event,
    pub out_capacity: u32,
    /// The plugin writes here how many of `out_events` it filled (null: no room).
    pub out_count: *mut u32,
}

impl ProcessData {
    pub fn new(channels: *const *mut f32, num_channels: u32, frames: u32, events: *const Event, num_events: u32, executor: *const Executor) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            frames,
            num_channels,
            num_events,
            channels,
            events,
            executor,
            out_events: std::ptr::null_mut(),
            out_capacity: 0,
            out_count: std::ptr::null_mut(),
        }
    }
}

pub(crate) unsafe extern "C" fn process<P: Process + PluginMeta>(ptr: *mut c_void, data: *const ProcessData) -> bool {
    let Some(d) = (unsafe { data.as_ref() }).filter(|d| d.size as usize >= std::mem::offset_of!(ProcessData, out_events)) else { return false };
    let instance = unsafe { instance::<P>(ptr) };
    // Events the plugin produces (`Context::midi_out`, `set_param`) go straight to the caller's buffer.
    let has_out = d.size as usize >= std::mem::size_of::<ProcessData>() && !d.out_count.is_null();
    instance.out.begin(if has_out { d.out_events } else { std::ptr::null_mut() }, if has_out { d.out_capacity as usize } else { 0 });
    let ok = unsafe { process_with::<P>(ptr, d.channels, d.num_channels, d.frames, d.events, d.num_events, d.executor) };
    if P::program_param().is_some() {
        // The host chose a preset through the Program parameter: the main thread loads it.
        instance.handle.0.check_program();
    }
    let n = instance.out.finish();
    if has_out {
        unsafe { *d.out_count = n as u32 };
    }
    ok
}

unsafe fn process_with<P: Process + PluginMeta>(
    ptr: *mut c_void,
    channels: *const *mut f32,
    num_channels: u32,
    frames: u32,
    events: *const Event,
    num_events: u32,
    executor: *const Executor,
) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    // Values set in bulk (a preset) switch over together, at the start of the block.
    if instance.handle.0.apply_bulk() | instance.snap.swap(false, Ordering::AcqRel) {
        instance.smooth.snap(&*instance.params);
        instance.bypassfx.snap(instance.bypass.get() >= 0.5);
    }
    let executor = unsafe { executor.as_ref() };
    let events: &[Event] =
        if num_events == 0 || events.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(events, num_events as usize) } };
    let apply = |event: &Event| {
        if event.kind != EVENT_PARAM {
            return;
        }
        let param = unsafe { event.data.param };
        if let Some((index, info)) = lookup::<P>(param.id) {
            let cell = instance.cell(index);
            let v = denormalize_or(info, param.normalized, cell.get());
            // A value the parameter already has is not a move (a host may resend it every block).
            if cell.get() != v {
                cell.set(v);
                instance.handle.0.bump_params();
            }
        }
    };
    if instance.failed.load(Ordering::Relaxed) || num_channels as usize > MAX_CHANNELS || (num_channels > 0 && channels.is_null()) {
        events.iter().for_each(apply);
        return false;
    }
    let channels: &[*mut f32] =
        if num_channels == 0 { &[] } else { unsafe { std::slice::from_raw_parts(channels, num_channels as usize) } };
    if channels.iter().any(|p| p.is_null()) {
        events.iter().for_each(apply);
        return false;
    }
    // AI access on: keep the audio before and after the plugin (crate::mcp).
    #[cfg(any(windows, unix))]
    let tap = match &instance.mcp_shared {
        Some(shared) if instance.mcp.get() >= 0.5 => shared.tap(),
        _ => None,
    };
    #[cfg(any(windows, unix))]
    if let Some(tap) = tap {
        for (c, &p) in channels.iter().enumerate() {
            tap.write(c, false, unsafe { std::slice::from_raw_parts(p, frames as usize) });
        }
    }
    // SAFETY: only the audio thread touches the plugin while processing.
    let plugin = unsafe { &mut *instance.plugin.get() };
    let sample_rate = f64::from_bits(instance.sample_rate.load(Ordering::Relaxed));
    let _denormals = DenormalGuard::new();

    // Sleep while there is nothing to do (see `Tail`): silent input, no
    // note held or started, no parameter changed.
    // SAFETY: like the plugin, the sleep state belongs to the audio thread.
    let sleep = unsafe { &mut *instance.sleep.get() };
    // Woken from outside (`InstanceHandle::wake`, edits through a handle).
    let mut woken = instance.handle.0.wake.swap(false, Ordering::AcqRel);
    for event in events {
        match event.kind {
            EVENT_PARAM => {
                let param = unsafe { event.data.param };
                // Hosts may resend unchanged values every block.
                if !woken {
                    woken = lookup::<P>(param.id).is_some_and(|(index, info)| denormalize_or(info, param.normalized, instance.cell(index).get()) != instance.cell(index).get());
                }
            }
            EVENT_MIDI => {
                woken = true;
                // All Sound Off (120) and All Notes Off (123) end the notes held on the channel,
                // so a note whose note-off never came does not keep the instance awake.
                let m = unsafe { event.data.midi };
                if m.status & 0xF0 == 0xB0 && matches!(m.data1 & 0x7F, 120 | 123) {
                    if let Some(keys) = sleep.held.get_mut(usize::from(m.channel)) {
                        *keys = 0;
                    }
                }
            }
            EVENT_NOTE_ON | EVENT_NOTE_OFF => {
                let note = unsafe { event.data.note };
                if let (Ok(key @ 0..=127), Ok(channel @ 0..=15)) = (u32::try_from(note.key), usize::try_from(note.channel)) {
                    if event.kind == EVENT_NOTE_ON {
                        sleep.held[channel] |= 1 << key;
                        woken = true;
                    } else {
                        sleep.held[channel] &= !(1 << key);
                    }
                }
            }
            _ => {}
        }
    }
    let input_quiet = is_quiet(channels.iter().map(|&p| p as *const f32), frames as usize);
    // SAFETY: the sidechain slot belongs to the audio thread.
    let sidechain_quiet = {
        let sc = unsafe { &*instance.sidechain.get() };
        is_quiet(sc.ptrs[..sc.len].iter().copied(), frames as usize)
    };
    let busy = woken || !input_quiet || !sidechain_quiet || sleep.holding();
    let rest = sleep.asleep && !busy;
    instance.silent.store(rest, Ordering::Relaxed);
    if rest {
        events.iter().for_each(apply);
        for &p in channels {
            unsafe { std::ptr::write_bytes(p, 0, frames as usize) };
        }
        #[cfg(any(windows, unix))]
        if let Some(tap) = tap {
            for (c, &p) in channels.iter().enumerate() {
                tap.write(c, true, unsafe { std::slice::from_raw_parts(p, frames as usize) });
            }
            tap.advance(frames as usize);
        }
        return true;
    }
    sleep.asleep = false;

    // Parameter changes reach the plugin by `[process] automation`:
    // "control" (default) applies them at the start of each control period and
    // joins periods without a change into one `process` call; "sample" splits
    // the block at every change; "events" never splits and hands the changes to
    // the plugin (`ctx.param_events()`). Each piece gets the notes inside it.
    let bypassed = instance.bypass.get() >= 0.5;
    let plan = instance.bypassfx.begin(bypassed, channels, frames as usize);
    // The plugin was not called while fully bypassed: reset it before it runs again, so notes
    // held across the bypass stop and old delay contents do not come back.
    if instance.bypassfx.take_resume(&plan) {
        if guarded(|| plugin.reset()).is_none() {
            instance.failed.store(true, Ordering::Relaxed);
        }
        instance.smooth.snap(&*instance.params);
    }
    let ran = guarded(|| {
        // SAFETY: the sidechain slot belongs to the audio thread.
        let sc = unsafe { &*instance.sidechain.get() };
        let mut run = |start: usize, end: usize, notes: &[NoteEvent], midi: &[MidiEvent], voice: &[VoiceEvent], pev: &[ParamEvent]| {
            // Bypassed: the shim already copied the input to the output.
            if end <= start || plan.skip_plugin {
                return;
            }
            let mut ptrs = [std::ptr::null_mut::<f32>(); MAX_CHANNELS];
            for (slot, &ch) in ptrs.iter_mut().zip(channels) {
                *slot = unsafe { ch.add(start) };
            }
            let io = unsafe { AudioIo::from_raw(&ptrs[..channels.len()], end - start) };
            let mut sc_ptrs = [std::ptr::null::<f32>(); MAX_CHANNELS];
            for (slot, &p) in sc_ptrs.iter_mut().zip(&sc.ptrs[..sc.len]) {
                *slot = unsafe { p.add(start) };
            }
            let sidechain = (sc.len > 0).then(|| unsafe { crate::midi::Sidechain::new(&sc_ptrs[..sc.len], end - start) });
            // Read per piece: the user's switch may change inside the block.
            let quality = instance.quality();
            let mut transport = unsafe { *instance.transport.get() }.to_transport();
            // A piece that starts inside the host's block sees the transport at its own first sample.
            if start > 0 && transport.playing {
                if let Some(p) = transport.position_samples.as_mut() {
                    *p += start as i64;
                }
                if let (Some(beats), Some(tempo)) = (transport.position_beats.as_mut(), transport.tempo) {
                    *beats += start as f64 / sample_rate * tempo / 60.0;
                }
                // The bar start moves on when the piece starts in a later bar.
                if let (Some(pos), Some(bar), Some((num, den))) = (transport.position_beats, transport.bar_start_beats.as_mut(), transport.time_signature) {
                    let bar_len = f64::from(num) * 4.0 / f64::from(den);
                    if bar_len > 0.0 && pos >= *bar + bar_len {
                        *bar += ((pos - *bar) / bar_len).floor() * bar_len;
                    }
                }
            }
            let mut ctx = Context { params: &*instance.params, io, notes, sample_rate, quality, transport, midi, sidechain, latency_cell: &instance.latency, main_requests: &instance.handle.0.main_requests, executor, instance: &instance.handle, param_events: pev, voice_events: voice, control_block: instance.control_period(), block_offset: start as u32, seen: &instance.seen, smooth: &instance.smooth, out: &instance.out, scratch: &instance.scratch };
            instance.scratch.rewind();
            instance.smooth.begin(&*instance.params);
            plugin.process(&mut ctx);
            instance.smooth.advance((end - start) as u32);
            drop(ctx);
            instance.seen.refresh(&*instance.params);
        };
        let frames = frames as usize;
        // Left uninitialized: filling 24 KB of note and MIDI slots cost about 1 us on every block.
        // Only the first `count` / `mcount` are written, and only those are read.
        let mut notes = [const { std::mem::MaybeUninit::<NoteEvent>::uninit() }; MAX_NOTES];
        let mut midi = [const { std::mem::MaybeUninit::<MidiEvent>::uninit() }; MAX_MIDI];
        let mut voice = [const { std::mem::MaybeUninit::<VoiceEvent>::uninit() }; MAX_VOICE_EVENTS];
        let mut vcount = 0;
        macro_rules! voice_upto {
            ($n:expr) => {
                unsafe { std::slice::from_raw_parts(voice.as_ptr().cast::<VoiceEvent>(), $n) }
            };
        }
        // SAFETY (both): slots below the count were written (`MaybeUninit::new`) before.
        macro_rules! notes_upto {
            ($n:expr) => {
                unsafe { std::slice::from_raw_parts(notes.as_ptr().cast::<NoteEvent>(), $n) }
            };
        }
        macro_rules! midi_upto {
            ($n:expr) => {
                unsafe { std::slice::from_raw_parts(midi.as_ptr().cast::<MidiEvent>(), $n) }
            };
        }
        let (mut count, mut mcount, mut start) = (0, 0, 0);
        // Applies a parameter event; true when the value changed.
        let apply_changed = |event: &Event| -> bool {
            let param = unsafe { event.data.param };
            match lookup::<P>(param.id) {
                Some((index, info)) => {
                    let v = info.denormalize(param.normalized);
                    let cell = instance.cell(index);
                    let changed = cell.get() != v;
                    if changed {
                        cell.set(v);
                        instance.handle.0.bump_params();
                    }
                    changed
                }
                None => false,
            }
        };
        let would_change = |event: &Event| -> bool {
            event.kind == EVENT_PARAM && {
                let param = unsafe { event.data.param };
                lookup::<P>(param.id).is_some_and(|(index, info)| info.denormalize(param.normalized) != instance.cell(index).get())
            }
        };
        macro_rules! ingest {
            ($event:expr, $at:expr) => {{
                let (event, at): (&Event, usize) = ($event, $at);
                match event.kind {
                    EVENT_MIDI => {
                        let m = unsafe { event.data.midi };
                        if mcount < MAX_MIDI && frames > 0 {
                            if let Some(kind) = MidiKind::from_raw(m.status, m.data1, m.data2) {
                                midi[mcount] = std::mem::MaybeUninit::new(MidiEvent { offset: (at.min(frames - 1) - start) as u32, channel: m.channel.min(15), kind });
                                mcount += 1;
                            }
                        }
                    }
                    EVENT_NOTE_ON | EVENT_NOTE_OFF => {
                        let note = unsafe { event.data.note };
                        // Unless the plugin reports its voice ends itself (`REPORTS_VOICE_END`), a
                        // note-off ends the voice now, and the host is told (CLAP `NOTE_END`).
                        if P::VOICES > 0 && !P::REPORTS_VOICE_END && event.kind == EVENT_NOTE_OFF && (0..=127).contains(&note.key) && frames > 0 {
                            // Output offsets are inside the host's block (not the piece).
                            instance.out.push(Event::note_end(at.min(frames - 1) as u32, note.note_id, note.channel, note.key));
                        }
                        if count < MAX_NOTES && (0..=127).contains(&note.key) && frames > 0 {
                            let n = NoteEvent {
                                offset: (at.min(frames - 1) - start) as u32,
                                kind: if event.kind == EVENT_NOTE_ON { NoteKind::On } else { NoteKind::Off },
                                key: note.key as u8,
                                channel: note.channel.clamp(0, 15) as u8,
                                velocity: note.velocity.clamp(0.0, 1.0),
                                note_id: note.note_id,
                            };
                            notes[count] = std::mem::MaybeUninit::new(n);
                            count += 1;
                            if mcount < MAX_MIDI {
                                let kind = match n.kind {
                                    NoteKind::On => MidiKind::NoteOn { key: n.key, velocity: n.velocity },
                                    NoteKind::Off => MidiKind::NoteOff { key: n.key, velocity: n.velocity },
                                };
                                midi[mcount] = std::mem::MaybeUninit::new(MidiEvent { offset: n.offset, channel: n.channel, kind });
                                mcount += 1;
                            }
                        }
                    }
                    EVENT_NOTE_MOD | EVENT_NOTE_EXPRESSION => {
                        let v = unsafe { event.data.voice };
                        let what = if event.kind == EVENT_NOTE_MOD {
                            // Only the plugin's own parameters; the index is the project-file position.
                            match lookup::<P>(v.target) {
                                Some((index, _)) if index < P::Params::infos().len() => Some(VoiceEventKind::Mod { index, amount: f64::from(v.value) }),
                                _ => None,
                            }
                        } else {
                            ExpressionKind::from_raw(v.target).map(|kind| VoiceEventKind::Expression { kind, value: v.value })
                        };
                        if let (Some(what), true) = (what, vcount < MAX_VOICE_EVENTS && frames > 0) {
                            voice[vcount] = std::mem::MaybeUninit::new(VoiceEvent {
                                offset: (at.min(frames - 1) - start) as u32,
                                note_id: v.note_id,
                                key: v.key.max(-1),
                                channel: v.channel.clamp(-1, 15),
                                kind: what,
                            });
                            vcount += 1;
                        }
                    }
                    _ => {}
                }
            }};
        }
        match instance.automation_mode() {
            1 => {
                // Events at or past the end of the block: notes go to the last sample, parameter
                // changes apply after the block (as in the other modes).
                let inside = events.partition_point(|e| (e.offset as usize) < frames);
                let (events, late) = events.split_at(inside);
                for event in events {
                    let at = (event.offset as usize).clamp(start, frames);
                    if event.kind == EVENT_PARAM {
                        // A value that does not change splits nothing.
                        if would_change(event) {
                            if at > start {
                                run(start, at, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), &[]);
                                (count, mcount, vcount, start) = (0, 0, 0, at);
                            }
                            apply_changed(event);
                        }
                    } else {
                        ingest!(event, at);
                    }
                }
                for event in late.iter().filter(|e| e.kind != EVENT_PARAM) {
                    ingest!(event, frames);
                }
                run(start, frames, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), &[]);
                for event in late.iter().filter(|e| e.kind == EVENT_PARAM) {
                    apply_changed(event);
                }
            }
            2 => {
                let mut pev = [const { std::mem::MaybeUninit::<ParamEvent>::uninit() }; MAX_PARAM_EVENTS]; // "events" mode only
                let mut pcount = 0;
                for event in events {
                    let at = (event.offset as usize).clamp(start, frames);
                    if event.kind == EVENT_PARAM {
                        let param = unsafe { event.data.param };
                        if let Some((index, info)) = lookup::<P>(param.id) {
                            if pcount < MAX_PARAM_EVENTS {
                                pev[pcount] = std::mem::MaybeUninit::new(ParamEvent { offset: at as u32, index: index as u32, plain: info.denormalize(param.normalized) });
                                pcount += 1;
                            }
                        }
                    } else {
                        ingest!(event, at);
                    }
                }
                // The plugin sees the values from the start of the block.
                // SAFETY: the first `pcount` entries were written above; `MaybeUninit<ParamEvent>` has the layout of `ParamEvent`.
                let pev = unsafe { std::slice::from_raw_parts(pev.as_ptr().cast::<ParamEvent>(), pcount) };
                run(0, frames, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), pev);
                for event in events.iter().filter(|e| e.kind == EVENT_PARAM) {
                    apply_changed(event);
                }
            }
            _ => {
                let period = (instance.control_period() as usize).max(1);
                let (mut i, mut seg) = (0, 0);
                while seg < frames {
                    let end = (seg + period).min(frames);
                    let from = i;
                    while i < events.len() && (events[i].offset as usize) < end {
                        i += 1;
                    }
                    let range = &events[from..i];
                    // A moving smoother keeps the pieces apart (one per control period).
                    if instance.smooth.active() && seg > start {
                        run(start, seg, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), &[]);
                        (count, mcount, vcount, start) = (0, 0, 0, seg);
                    }
                    // One pass: the pieces so far run with the old values, so the
                    // first value that really changes ends them. All of a period's values
                    // apply at its start, so only each parameter's last one matters: walk the
                    // period backwards and skip the IDs already taken (T2: dense automation
                    // looks up and converts 8 values per period instead of 256).
                    let mut flushed = false;
                    let mut taken = [0u32; 16];
                    let mut ntaken = 0;
                    let mut overflow = false;
                    let mut consider = |event: &Event, flushed: &mut bool| {
                        let param = unsafe { event.data.param };
                        if let Some((index, info)) = lookup::<P>(param.id) {
                            let v = info.denormalize(param.normalized);
                            let cell = instance.cell(index);
                            if cell.get() != v {
                                if !*flushed {
                                    *flushed = true;
                                    if seg > start {
                                        run(start, seg, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), &[]);
                                        (count, mcount, vcount, start) = (0, 0, 0, seg);
                                    }
                                }
                                cell.set(v);
                                instance.handle.0.bump_params();
                            }
                        }
                    };
                    for event in range.iter().rev().filter(|e| e.kind == EVENT_PARAM) {
                        let id = unsafe { event.data.param.id };
                        if taken[..ntaken].contains(&id) {
                            continue;
                        }
                        if ntaken == taken.len() {
                            overflow = true;
                            break;
                        }
                        taken[ntaken] = id;
                        ntaken += 1;
                        consider(event, &mut flushed);
                    }
                    if overflow {
                        // More than 16 parameters in one period: every event, in order (the
                        // last value of each still wins).
                        for event in range.iter().filter(|e| e.kind == EVENT_PARAM) {
                            consider(event, &mut flushed);
                        }
                    }
                    for event in range.iter().filter(|e| e.kind != EVENT_PARAM) {
                        ingest!(event, (event.offset as usize).clamp(start, frames));
                    }
                    seg = end;
                }
                // Events at or past the end of the block.
                for event in &events[i..] {
                    if event.kind == EVENT_PARAM {
                        apply_changed(event);
                    } else {
                        ingest!(event, frames);
                    }
                }
                run(start, frames, notes_upto!(count), midi_upto!(mcount), voice_upto!(vcount), &[]);
            }
        }
        plugin.tail()
    });
    let ran = match ran {
        Some(tail) => {
            instance.bypassfx.finish(plan, instance.bypass.get() >= 0.5, channels, frames as usize);
            let latency = instance.latency.load(Ordering::Relaxed);
            instance.tail.store(tail_samples(tail, latency), Ordering::Relaxed);
            sleep.update(tail, busy, || is_quiet(channels.iter().map(|&p| p as *const f32), frames as usize), frames as usize, sample_rate, latency);
            true
        }
        None => {
            instance.bypassfx.abort(plan);
            instance.failed.store(true, Ordering::Relaxed);
            false
        }
    };
    #[cfg(any(windows, unix))]
    if let Some(tap) = tap {
        for (c, &p) in channels.iter().enumerate() {
            tap.write(c, true, unsafe { std::slice::from_raw_parts(p, frames as usize) });
        }
        tap.advance(frames as usize);
    }
    ran
}

/// The store entry that carries `Process::STATE_VERSION`.
const STATE_VERSION_KEY: &str = "ferritron.state_version";

pub(crate) unsafe extern "C" fn save_state<P: Process + PluginMeta>(ptr: *mut c_void, buf: *mut u8, cap: u32) -> u32 {
    let instance = unsafe { instance::<P>(ptr) };
    let mut extra = vec![(BYPASS.id, instance.bypass.get())];
    if P::MCP {
        extra.push((MCP_SWITCH.id, instance.mcp.get()));
    }
    if P::QUALITY {
        extra.push((QUALITY.id, instance.quality.get()));
    }
    if P::program_param().is_some() {
        extra.push((crate::__private::PROGRAM_ID, instance.handle.0.program.get()));
    }
    let size = instance.editor_size.load(Ordering::Relaxed);
    if size != 0 {
        extra.push((EDITOR_WIDTH, (size >> 32) as f64));
        extra.push((EDITOR_HEIGHT, (size & 0xffff_ffff) as f64));
    }
    let id = instance.handle.0.id();
    extra.push((INSTANCE_ID_HIGH, (id >> 32) as f64));
    extra.push((INSTANCE_ID_LOW, (id & 0xffff_ffff) as f64));
    let mut store = instance.handle.0.store_snapshot();
    store.insert(STATE_VERSION_KEY.to_string(), P::STATE_VERSION.to_le_bytes().to_vec());
    let data = state::serialize(&*instance.params, |i| instance.handle.0.pending_value(i).unwrap_or_else(|| instance.params.cell(i).get()), &extra, &store);
    if !buf.is_null() && data.len() <= cap as usize {
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len()) };
    }
    data.len() as u32
}

pub(crate) unsafe extern "C" fn load_state<P: Process + PluginMeta>(ptr: *mut c_void, data: *const u8, len: u32) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    if data.is_null() {
        return false;
    }
    let data = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let (mut width, mut height) = (0u32, 0u32);
    let (mut id_high, mut id_low) = (None, None);
    let pixels = |v: f64| if v.is_finite() && (1.0..=65_535.0).contains(&v) { v as u32 } else { 0 };
    let word = |v: f64| (v.is_finite() && (0.0..=u32::MAX as f64).contains(&v)).then_some(v as u64);
    let loaded = state::deserialize(&*instance.params, data, |id, value| {
        if id == BYPASS.id {
            instance.bypass.set(BYPASS.sanitize(value));
        } else if P::MCP && id == MCP_SWITCH.id {
            instance.mcp.set(MCP_SWITCH.sanitize(value));
        } else if P::QUALITY && id == QUALITY.id {
            instance.quality.set(QUALITY.sanitize(value));
        } else if let Some(program) = P::program_param().filter(|_| id == crate::__private::PROGRAM_ID) {
            // The preset's values are in the state already: remember the choice, load nothing.
            instance.handle.0.set_program(program.sanitize(value));
        } else if id == EDITOR_WIDTH {
            width = pixels(value);
        } else if id == EDITOR_HEIGHT {
            height = pixels(value);
        } else if id == INSTANCE_ID_HIGH {
            id_high = word(value);
        } else if id == INSTANCE_ID_LOW {
            id_low = word(value);
        }
    });
    if loaded {
        instance.snap.store(true, Ordering::Release);
        unsafe { set_editor_size::<P>(ptr, width, height) };
        if let (Some(high), Some(low)) = (id_high, id_low) {
            instance.handle.0.restore_id((high << 32) | low);
        }
        let mut store = state::store_of(data);
        let saved = store.get(STATE_VERSION_KEY).and_then(|v| v.as_slice().try_into().ok()).map_or(0, u32::from_le_bytes);
        store.remove(STATE_VERSION_KEY);
        if saved != P::STATE_VERSION {
            // A panic in the plugin's migration: the state is kept as it was saved.
            let before = store.clone();
            if guarded(|| P::migrate_state(saved, P::STATE_VERSION, &*instance.params, &mut store)).is_none() {
                store = before;
            }
        }
        instance.handle.0.replace_store(store);
        // Whatever changed must reach `process`, even while it sleeps.
        instance.handle.0.wake.store(true, Ordering::Release);
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FloatParam, ParamCell};

    #[derive(Default)]
    struct Doubler {
        panic_next: bool,
    }

    struct Params {
        amount: FloatParam,
    }

    static INFOS: [ParamInfo; 1] =
        [ParamInfo { id: 7, key: "amount", name: "Amount", unit: "", kind: ParamKind::Float { min: 0.0, max: 4.0 }, default: 2.0, scale: ParamScale::Linear, text: None }];

    impl Default for Params {
        fn default() -> Self {
            Self { amount: FloatParam::new(2.0) }
        }
    }

    impl ParamSet for Params {
        fn infos() -> &'static [ParamInfo] {
            &INFOS
        }
        fn cell(&self, _index: usize) -> &ParamCell {
            self.amount.cell()
        }
        fn index_of(id: u32) -> Option<usize> {
            (id == 7).then_some(0)
        }
    }

    thread_local! {
        static MIGRATED: std::cell::Cell<Option<(u32, u32)>> = const { std::cell::Cell::new(None) };
        /// Only the migration test turns the upgrade on.
        static UPGRADE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Only the panic test turns this on: the migration panics.
        static PANIC_MIGRATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    impl Process for Doubler {
        type Params = Params;
        const STATE_VERSION: u32 = 3;
        fn migrate_state(from: u32, to: u32, params: &Params, store: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            MIGRATED.with(|m| m.set(Some((from, to))));
            assert!(!PANIC_MIGRATE.with(|p| p.get()), "migration bug");
            if UPGRADE.with(|u| u.get()) {
                params.amount.cell().set(1.5); // what an upgrade might do
                store.insert("migrated".into(), vec![1]);
            }
        }
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            assert!(!self.panic_next, "plugin bug");
            let amount = ctx.params().amount.value();
            ctx.io().for_each_sample(|s| *s *= amount);
        }
    }

    const INFO: PluginInfo = PluginInfo {
        id: "test.doubler",
        name: "Doubler",
        vendor: "",
        version: "1",
        url: "",
        email: "",
        kind: PluginKind::Effect,
        class_id: [0; 16],
        isolation: crate::isolation::Isolation::Guarded,
        process: crate::isolation::ProcessOptions::DEFAULT,
        vst3_subcategories: "",
        clap_features: "",
        vst2_id: 0,
        audio: crate::AudioConfig::DEFAULT,
    };

    /// X6 / H7: what the audio thread writes every block (`silent`, `tail`) sits on cache lines
    /// of its own, apart from what the host's and editors' threads write.
    #[test]
    fn audio_thread_flags_share_no_cache_line() {
        use std::mem::offset_of;
        type I = Instance<Doubler>;
        assert!(std::mem::align_of::<I>() >= 128);
        let others = [
            offset_of!(I, editor_size),
            offset_of!(I, snap),
            offset_of!(I, bypass),
            offset_of!(I, mcp),
            offset_of!(I, quality),
            offset_of!(I, offline),
            offset_of!(I, latency),
            offset_of!(I, sample_rate),
            offset_of!(I, extension),
            offset_of!(I, params),
            offset_of!(I, handle),
        ];
        for (name, at) in [("silent", offset_of!(I, silent)), ("tail", offset_of!(I, tail))] {
            for other in others {
                assert_ne!(at / 128, other / 128, "{name} at {at} shares a 128-byte line with a field at {other}");
            }
        }
        assert_ne!(offset_of!(I, silent) / 128, offset_of!(I, tail) / 128);
    }

    impl PluginMeta for Doubler {
        const INFO: &'static PluginInfo = &INFO;
        const AUTOMATION: u8 = 1;
        fn localized() -> &'static crate::i18n::Localized {
            static L: std::sync::OnceLock<crate::i18n::Localized> = std::sync::OnceLock::new();
            L.get_or_init(|| crate::i18n::Localized::new(INFO.id, Params::infos(), &[]))
        }
    }

    static INFO_C: PluginInfoC = PluginInfoC::new(&INFO);
    static API: PluginApi = PluginApi::of::<Doubler>(&INFO_C);

    /// The saved state of `instance` (`save_state` asked for its size first, as hosts do).
    unsafe fn saved_state(instance: *mut c_void) -> Vec<u8> {
        let size = unsafe { (API.save_state)(instance, std::ptr::null_mut(), 0) };
        let mut data = vec![0u8; size as usize];
        unsafe { (API.save_state)(instance, data.as_mut_ptr(), size) };
        data
    }

    /// `[vst3] controller_id`: the controller copy (`ROLE_CONTROLLER`) is left out of the instance
    /// lists; the processor's instance stays in them.
    #[test]
    fn controller_copy_is_hidden_from_the_instance_lists() {
        let processor = unsafe { (API.create)() };
        let controller = unsafe { (API.create)() };
        let role = unsafe { &*((API.get_extension)(EXT_ROLE.as_ptr()) as *const RoleApi) };
        assert!(role.struct_size as usize >= size_of::<RoleApi>());
        unsafe { (role.set_role)(controller, ROLE_CONTROLLER) };
        let id_of = |p: *mut c_void| unsafe { &*(p as *const Instance<Doubler>) }.handle.id();
        let listed: Vec<_> = crate::instance::instances::<Doubler>().iter().map(|h| h.id()).collect();
        assert!(listed.contains(&id_of(processor)));
        assert!(!listed.contains(&id_of(controller)));
        unsafe {
            (API.destroy)(controller);
            (API.destroy)(processor);
        }
    }

    fn run(instance: *mut c_void, samples: &mut [f32]) -> bool {
        run_with(instance, samples, &[])
    }

    fn run_with(instance: *mut c_void, samples: &mut [f32], events: &[Event]) -> bool {
        let ptrs = [samples.as_mut_ptr()];
        let d = ProcessData::new(ptrs.as_ptr(), 1, samples.len() as u32, events.as_ptr(), events.len() as u32, std::ptr::null());
        unsafe { (API.process)(instance, &d) }
    }

    /// `[process] automation = "events"`: one call per block, the changes as a list.
    #[derive(Default)]
    struct EventsProbe;

    thread_local! {
        static PROBE: std::cell::RefCell<(u32, f32, Vec<(u32, u32, f64)>)> = const { std::cell::RefCell::new((0, 0.0, Vec::new())) };
    }

    thread_local! {
        static SCRATCH: std::cell::Cell<(bool, bool, bool)> = const { std::cell::Cell::new((false, false, false)) };
    }

    impl Process for EventsProbe {
        type Params = Params;
        fn scratch(&self, _setup: &Setup) -> crate::ScratchSpec {
            crate::ScratchSpec::of::<f32>(100)
        }
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            // 100 floats fit, a second 100 do not; every piece starts again from the top.
            let first = ctx.scratch::<f32>(100).map(|s| {
                s.fill(1.0);
                s.len() == 100
            });
            let second = ctx.scratch::<f32>(100);
            SCRATCH.with(|c| c.set((first == Some(true), second.is_none(), ctx.scratch::<f32>(0).is_some())));
            let start = ctx.params().amount.value();
            let list: Vec<_> = ctx.param_events().iter().map(|e| (e.offset, e.index, e.plain)).collect();
            PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.0 += 1;
                p.1 = start;
                p.2 = list;
            });
        }
    }

    impl PluginMeta for EventsProbe {
        const INFO: &'static PluginInfo = &INFO;
        const AUTOMATION: u8 = 2;
        fn localized() -> &'static crate::i18n::Localized {
            static L: std::sync::OnceLock<crate::i18n::Localized> = std::sync::OnceLock::new();
            L.get_or_init(|| crate::i18n::Localized::new(INFO.id, Params::infos(), &[]))
        }
    }

    static PROBE_API: PluginApi = PluginApi::of::<EventsProbe>(&INFO_C);

    #[test]
    fn events_mode_calls_once_and_hands_over_the_changes() {
        unsafe {
            let a = (PROBE_API.create)();
            (PROBE_API.activate)(a, 48_000.0, 64);
            let mut buf = [0.0f32; 64];
            let ptrs = [buf.as_mut_ptr()];
            // amount: 2.0 -> 1.0 at 10 -> 3.0 at 50 (normalized over 0..4).
            let events = [Event::param(10, 7, 0.25), Event::param(50, 7, 0.75)];
            let d = ProcessData::new(ptrs.as_ptr(), 1, 64, events.as_ptr(), 2, std::ptr::null());
            assert!((PROBE_API.process)(a, &d));
            let (calls, start, list) = PROBE.with(|p| p.borrow().clone());
            assert_eq!(calls, 1, "the block is never split");
            assert_eq!(start, 2.0, "values are those from the start of the block");
            assert_eq!(list, vec![(10, 0, 1.0), (50, 0, 3.0)]);
            assert_eq!(SCRATCH.with(|c| c.get()), (true, true, true), "scratch lends once, then runs out");
            assert_eq!((PROBE_API.get_param)(a, 7), 0.75, "the last value stays afterwards");
            (PROBE_API.destroy)(a);
        }
    }

    #[test]
    fn sample_mode_ignores_a_resent_value() {
        unsafe {
            let a = (API.create)();
            (API.activate)(a, 48_000.0, 64);
            let mut buf = [1.0f32; 8];
            // 2.0 is the default: normalized 0.5. Sent again at 4, it changes nothing.
            assert!(run_with(a, &mut buf, &[Event::param(4, 7, 0.5)]));
            assert_eq!(buf, [2.0; 8]);
            (API.destroy)(a);
        }
    }

    #[derive(Default)]
    struct NoteCounter;

    impl Process for NoteCounter {
        type Params = Params;
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            // Write each note's key at its offset so the test can see where it landed.
            for note in ctx.notes() {
                ctx.io().channel_mut(0)[note.offset as usize] = note.key as f32;
            }
        }
    }

    impl PluginMeta for NoteCounter {
        const INFO: &'static PluginInfo = &INFO;
        fn localized() -> &'static crate::i18n::Localized {
            Doubler::localized()
        }
    }

    static NOTE_API: PluginApi = PluginApi::of::<NoteCounter>(&INFO_C);

    /// Counts `process` calls and records what it was told; with `tone`, it
    /// outputs 0.5 whatever the input.
    #[derive(Default)]
    struct Sleeper {
        calls: u32,
        tone: bool,
        tail: Option<Tail>,
        quality: Option<Quality>,
        setup_quality: Option<Quality>,
    }

    impl Process for Sleeper {
        type Params = Params;
        fn activate(&mut self, setup: &Setup) {
            self.setup_quality = Some(setup.quality);
        }
        fn tail(&self) -> Tail {
            self.tail.unwrap_or(Tail::Auto)
        }
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            self.calls += 1;
            self.quality = Some(ctx.quality());
            if self.tone {
                ctx.io().for_each_sample(|s| *s = 0.5);
            }
        }
    }

    impl PluginMeta for Sleeper {
        const INFO: &'static PluginInfo = &INFO;
        const QUALITY: bool = true;
        fn localized() -> &'static crate::i18n::Localized {
            Doubler::localized()
        }
    }

    static SLEEP_API: PluginApi = PluginApi::of::<Sleeper>(&INFO_C);

    /// 10 ms blocks at 48 kHz.
    const BLOCK: usize = 480;

    unsafe fn sleeper<'a>(instance: *mut c_void) -> &'a mut Sleeper {
        unsafe { &mut *(*(instance as *const Instance<Sleeper>)).plugin.get() }
    }

    /// Runs `blocks` blocks of `level`, returns the last block.
    fn feed(instance: *mut c_void, blocks: usize, level: f32, events: &[Event]) -> Vec<f32> {
        let mut buf = vec![level; BLOCK];
        for n in 0..blocks {
            buf.fill(level);
            let ptrs = [buf.as_mut_ptr()];
            let events: &[Event] = if n == 0 { events } else { &[] };
            let d = ProcessData::new(ptrs.as_ptr(), 1, BLOCK as u32, events.as_ptr(), events.len() as u32, std::ptr::null());
            assert!(unsafe { (SLEEP_API.process)(instance, &d) });
        }
        buf
    }

    fn silent(instance: *mut c_void) -> bool {
        let ext = unsafe { (SLEEP_API.get_extension)(EXT_SLEEP.as_ptr()) } as *const SleepApi;
        unsafe { ((*ext).silent)(instance) }
    }

    fn new_sleeper(tail: Option<Tail>) -> *mut c_void {
        unsafe {
            let a = (SLEEP_API.create)();
            sleeper(a).tail = tail;
            assert!((SLEEP_API.activate)(a, 48_000.0, BLOCK as u32));
            a
        }
    }

    #[test]
    fn the_description_carries_keys_kinds_and_builtins() {
        let ext = unsafe { (API.get_extension)(EXT_DESCRIBE.as_ptr()) } as *const DescribeApi;
        let describe = unsafe { ext.as_ref() }.expect("ferritron.describe.v1").describe;
        let len = unsafe { describe(std::ptr::null_mut(), 0) };
        let mut buf = vec![0u8; len as usize];
        assert_eq!(unsafe { describe(buf.as_mut_ptr(), len) }, len);
        let json: serde_json::Value = serde_json::from_slice(&buf).expect("valid JSON");
        assert_eq!(json["name"], "Doubler");
        let params = json["params"].as_array().unwrap();
        assert_eq!(params[0]["key"], "amount");
        assert_eq!(params[0]["kind"], "float");
        assert_eq!(params[0]["max"], 4.0);
        assert_eq!(params[1]["key"], "ferritron.bypass");
        assert_eq!(params[1]["flags"], PARAM_BYPASS);
    }

    #[test]
    fn silence_puts_the_plugin_to_sleep_and_input_wakes_it() {
        unsafe {
            let a = new_sleeper(None);
            // Auto: two seconds of silence in and out, then no more calls.
            feed(a, 199, 0.0, &[]);
            assert!(!silent(a));
            feed(a, 2, 0.0, &[]);
            let calls = sleeper(a).calls;
            assert_eq!(calls, 200, "asleep after two seconds");
            let out = feed(a, 500, 0.0, &[]);
            assert_eq!(sleeper(a).calls, calls);
            assert!(silent(a) && out.iter().all(|&s| s == 0.0));
            // Sound wakes it at once, in the same block.
            let out = feed(a, 1, 0.25, &[]);
            assert_eq!(sleeper(a).calls, calls + 1);
            assert!(!silent(a) && out[0] == 0.25);
            // Barely audible input is not silence; -130 dB is.
            feed(a, 300, 1e-5, &[]);
            assert_eq!(sleeper(a).calls, calls + 301);
            feed(a, 300, 3e-7, &[]);
            assert_eq!(sleeper(a).calls, calls + 301 + 200);
            // A parameter that changes wakes it; one resent unchanged does not.
            let before = sleeper(a).calls;
            feed(a, 1, 0.0, &[Event::param(0, 7, (SLEEP_API.get_param)(a, 7))]);
            assert_eq!(sleeper(a).calls, before);
            feed(a, 1, 0.0, &[Event::param(0, 7, 0.9)]);
            assert_eq!(sleeper(a).calls, before + 1);
            (SLEEP_API.destroy)(a);
        }
    }

    #[test]
    fn the_tail_decides_how_long_silence_is_processed() {
        unsafe {
            // A declared tail: 0.1 s after the input stops, whatever the output did.
            let a = new_sleeper(Some(Tail::Samples(4_800)));
            feed(a, 50, 0.0, &[]);
            assert_eq!(sleeper(a).calls, 10);
            let tail = (*((SLEEP_API.get_extension)(EXT_SLEEP.as_ptr()) as *const SleepApi)).tail;
            assert_eq!(tail(a), 4_800);
            (SLEEP_API.destroy)(a);

            // Never: a generator, or a plugin that says so.
            let b = new_sleeper(Some(Tail::Infinite));
            feed(b, 500, 0.0, &[]);
            assert_eq!(sleeper(b).calls, 500);
            assert_eq!(tail(b), TAIL_UNKNOWN);
            (SLEEP_API.destroy)(b);

            // Auto, but the plugin makes sound from silence: it never sleeps.
            let c = new_sleeper(None);
            sleeper(c).tone = true;
            let out = feed(c, 500, 0.0, &[]);
            assert_eq!(sleeper(c).calls, 500);
            assert_eq!(out[0], 0.5);
            (SLEEP_API.destroy)(c);

            // A declared tail never cuts off sound that is still there.
            let d = new_sleeper(Some(Tail::Samples(0)));
            sleeper(d).tone = true;
            feed(d, 50, 0.0, &[]);
            assert_eq!(sleeper(d).calls, 50);
            (SLEEP_API.destroy)(d);
        }
    }

    #[test]
    fn held_notes_keep_the_plugin_awake() {
        unsafe {
            let a = new_sleeper(None);
            feed(a, 1, 0.0, &[Event::note(0, true, 60, 0, 1.0, -1)]);
            feed(a, 400, 0.0, &[]);
            assert_eq!(sleeper(a).calls, 401, "a held note may still sound");
            feed(a, 1, 0.0, &[Event::note(0, false, 60, 0, 0.0, -1)]);
            feed(a, 400, 0.0, &[]);
            assert_eq!(sleeper(a).calls, 402 + 199, "released: asleep two seconds later");
            // A new note wakes it.
            feed(a, 1, 0.0, &[Event::note(5, true, 64, 3, 1.0, -1)]);
            assert_eq!(sleeper(a).calls, 402 + 199 + 1);
            (SLEEP_API.destroy)(a);
        }
    }

    #[test]
    fn quality_follows_export_and_the_users_switch() {
        unsafe {
            let a = new_sleeper(Some(Tail::Infinite));
            // Own parameter, bypass, quality.
            assert_eq!((SLEEP_API.param_count)(), 3);
            let mut info = std::mem::zeroed::<ParamInfoC>();
            assert!((SLEEP_API.param_info)(2, &mut info));
            assert_eq!((info.id, info.step_count, info.flags), (QUALITY.id, 2, PARAM_LIST | PARAM_NOT_AUTOMATABLE));
            assert_eq!(sleeper(a).setup_quality, Some(Quality::Normal));
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).quality, Some(Quality::Normal));

            // The host starts an export: the next block is already at its best.
            let render = &*((SLEEP_API.get_extension)(EXT_RENDER.as_ptr()) as *const RenderApi);
            (render.set_offline)(a, true);
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).quality, Some(Quality::Best));
            assert!((SLEEP_API.activate)(a, 48_000.0, BLOCK as u32));
            assert_eq!(sleeper(a).setup_quality, Some(Quality::Best));

            // The user's choice wins over Auto, also as automation-free edits from the editor.
            assert!((SLEEP_API.set_param)(a, QUALITY.id, QUALITY.normalize(1.0)));
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).quality, Some(Quality::Normal));
            (render.set_offline)(a, false);
            feed(a, 1, 0.0, &[Event::param(0, QUALITY.id, QUALITY.normalize(2.0))]);
            assert_eq!(sleeper(a).quality, Some(Quality::Best));

            // Saved with the project.
            let size = (SLEEP_API.save_state)(a, std::ptr::null_mut(), 0);
            let mut data = vec![0u8; size as usize];
            (SLEEP_API.save_state)(a, data.as_mut_ptr(), size);
            let b = (SLEEP_API.create)();
            assert!((SLEEP_API.load_state)(b, data.as_ptr(), size));
            assert_eq!((SLEEP_API.get_param)(b, QUALITY.id), 1.0);
            for i in [a, b] {
                (SLEEP_API.destroy)(i);
            }
        }
    }

    #[test]
    fn notes_arrive_at_their_offsets_inside_split_blocks() {
        unsafe {
            let a = (NOTE_API.create)();
            let mut buf = [0.0f32; 6];
            let events = [
                Event::note(1, true, 60, 0, 1.0, -1),
                Event::param(3, 7, 0.5), // splits the block at sample 3
                Event::note(4, false, 61, 0, 0.0, -1),
                Event::note(9, true, 62, 0, 1.0, -1), // past the end: lands on the last sample
            ];
            let ptrs = [buf.as_mut_ptr()];
            let d = ProcessData::new(ptrs.as_ptr(), 1, 6, events.as_ptr(), events.len() as u32, std::ptr::null());
            assert!((NOTE_API.process)(a, &d));
            assert_eq!(buf, [0.0, 60.0, 0.0, 0.0, 61.0, 62.0]);
            (NOTE_API.destroy)(a);
        }
    }

    #[test]
    fn bypass_passes_input_through_and_is_saved() {
        unsafe {
            let a = (API.create)();
            (API.activate)(a, 48_000.0, 64);
            assert_eq!((API.param_count)(), 2);
            let mut buf = [1.0; 4];
            let events = [Event::param(2, BYPASS.id, 1.0)];
            assert!(run_with(a, &mut buf, &events));
            // 2.0: bypassing crossfades over 10 ms (480 samples at 48 kHz) instead of cutting.
            assert_eq!(buf, [2.0; 4], "the fade starts with the next block");
            let mut last = [1.0f32; 64];
            for _ in 0..12 {
                last.fill(1.0);
                assert!(run(a, &mut last));
            }
            assert_eq!(last, [1.0; 64], "fully bypassed: the input, untouched");
            // Back to the plugin: fades towards its output again.
            (API.set_param)(a, BYPASS.id, 0.0);
            for _ in 0..12 {
                last.fill(1.0);
                assert!(run(a, &mut last));
            }
            assert_eq!(last, [2.0; 64]);

            (API.set_param)(a, BYPASS.id, 1.0);
            let data = saved_state(a);
            let b = (API.create)();
            assert!((API.load_state)(b, data.as_ptr(), data.len() as u32));
            assert_eq!((API.get_param)(b, BYPASS.id), 1.0);
            (API.destroy)(a);
            (API.destroy)(b);
        }
    }

    /// A panic in `migrate_state` is not sent to the host: the state loads as it was saved.
    #[test]
    fn a_panicking_migration_does_not_reach_the_host() {
        unsafe {
            let a = (API.create)();
            (API.activate)(a, 48_000.0, 64);
            let mut old = Vec::new();
            old.extend_from_slice(b"FRTN");
            old.extend_from_slice(&1u32.to_le_bytes());
            old.extend_from_slice(&1u32.to_le_bytes());
            old.extend_from_slice(&7u32.to_le_bytes());
            old.extend_from_slice(&4.0f64.to_le_bytes());
            PANIC_MIGRATE.with(|p| p.set(true));
            let loaded = (API.load_state)(a, old.as_ptr(), old.len() as u32);
            PANIC_MIGRATE.with(|p| p.set(false));
            assert!(loaded, "the load must succeed although the migration panicked");
            assert_eq!((API.get_param)(a, 7), 1.0, "the state is kept as saved (4.0 of 0..4)");
            (API.destroy)(a);
        }
    }

    #[test]
    fn state_saved_by_another_version_is_migrated() {
        unsafe {
            let a = (API.create)();
            (API.activate)(a, 48_000.0, 64);
            UPGRADE.with(|u| u.set(true));
            // State from before 2.0 has no version: 0.
            let mut old = Vec::new();
            old.extend_from_slice(b"FRTN");
            old.extend_from_slice(&1u32.to_le_bytes());
            old.extend_from_slice(&1u32.to_le_bytes());
            old.extend_from_slice(&7u32.to_le_bytes());
            old.extend_from_slice(&4.0f64.to_le_bytes());
            assert!((API.load_state)(a, old.as_ptr(), old.len() as u32));
            assert_eq!(MIGRATED.with(|m| m.take()), Some((0, 3)));
            assert_eq!(handle_of::<Doubler>(a).store().get("migrated"), Some(vec![1]));
            assert_eq!((API.get_param)(a, 7), 1.5 / 4.0, "the migration rewrote the value");
            // State saved now carries the current version: no migration on the way back.
            let data = saved_state(a);
            assert!((API.load_state)(a, data.as_ptr(), data.len() as u32));
            assert_eq!(MIGRATED.with(|m| m.take()), None);
            (API.destroy)(a);
        }
    }

    #[test]
    fn attached_values_and_the_editor_flag_follow_the_instance() {
        unsafe {
            let a = (API.create)();
            let handle = handle_of::<Doubler>(a).clone();
            assert!(!handle.is_editor_open());
            handle.editor_opened(true);
            handle.editor_opened(true);
            handle.editor_opened(false);
            assert!(handle.is_editor_open(), "one of two editors is still open");
            handle.editor_opened(false);
            handle.editor_opened(false); // too many closes do not underflow
            assert!(!handle.is_editor_open());

            let data = std::sync::Arc::new(String::from("analysis"));
            handle.attach(data.clone());
            assert!(std::sync::Arc::ptr_eq(&handle.shared::<String>().unwrap(), &data));
            assert!(handle.shared::<u32>().is_none());
            assert_eq!(std::sync::Arc::strong_count(&data), 2, "ours and the instance's");
            (API.destroy)(a);
            assert_eq!(std::sync::Arc::strong_count(&data), 1, "released with the instance");
        }
    }

    #[test]
    fn the_editor_size_is_saved_with_the_state() {
        unsafe {
            let a = (API.create)();
            let (mut w, mut h) = (0, 0);
            assert!(!get_editor_size::<Doubler>(a, &mut w, &mut h)); // not chosen yet
            set_editor_size::<Doubler>(a, 812, 455);
            let data = saved_state(a);
            let b = (API.create)();
            assert!((API.load_state)(b, data.as_ptr(), data.len() as u32));
            assert!(get_editor_size::<Doubler>(b, &mut w, &mut h));
            assert_eq!((w, h), (812, 455));
            assert_eq!((API.get_param)(b, 7), (API.get_param)(a, 7));
            // State without a size (an older project) clears it.
            let c = (API.create)();
            let data = saved_state(c);
            assert!((API.load_state)(b, data.as_ptr(), data.len() as u32));
            assert!(!get_editor_size::<Doubler>(b, &mut w, &mut h));
            for i in [a, b, c] {
                (API.destroy)(i);
            }
        }
    }

    #[test]
    fn parameter_changes_apply_at_their_sample_offset() {
        unsafe {
            let a = (API.create)();
            (API.activate)(a, 48_000.0, 64);
            let mut buf = [1.0; 6];
            // amount: 2.0 by default -> 1.0 at sample 2 -> 3.0 at sample 4 (normalized over 0..4).
            let events = [
                Event::param(2, 7, 0.25),
                Event::param(4, 7, 0.75),
                Event::param(99, 7, 0.5), // past the end: applies after the block
            ];
            assert!(run_with(a, &mut buf, &events));
            assert_eq!(buf, [2.0, 2.0, 1.0, 1.0, 3.0, 3.0]);
            assert_eq!((API.get_param)(a, 7), 0.5);
            (API.destroy)(a);
        }
    }

    #[test]
    fn processes_params_and_state_through_the_abi() {
        unsafe {
            let a = (API.create)();
            assert!((API.activate)(a, 48_000.0, 64));
            let mut buf = [1.0, -0.5];
            assert!(run(a, &mut buf));
            assert_eq!(buf, [2.0, -1.0]);

            assert!((API.set_param)(a, 7, 0.25)); // normalized 0.25 of 0..4 = 1.0
            assert_eq!((API.get_param)(a, 7), 0.25);
            assert!(!(API.set_param)(a, 999, 0.5));

            let data = saved_state(a);
            let b = (API.create)();
            assert!((API.load_state)(b, data.as_ptr(), data.len() as u32));
            assert_eq!((API.get_param)(b, 7), 0.25);

            let mut text = [0u8; 32];
            let len = (API.format_param)(7, 0.25, text.as_mut_ptr(), 32);
            assert_eq!(&text[..len as usize], b"1.00");
            (API.destroy)(a);
            (API.destroy)(b);
        }
    }

    #[test]
    fn a_panicking_plugin_is_disabled_instead_of_crashing_the_host() {
        unsafe {
            let a = (API.create)();
            (*(*(a as *const Instance<Doubler>)).plugin.get()).panic_next = true;
            let mut buf = [1.0];
            assert!(!run(a, &mut buf));
            // Stays disabled even after the cause is gone.
            (*(*(a as *const Instance<Doubler>)).plugin.get()).panic_next = false;
            assert!(!run(a, &mut buf));
            // Re-activation gives the plugin another chance.
            assert!((API.activate)(a, 48_000.0, 64));
            assert!(run(a, &mut buf));
            (API.destroy)(a);
        }
    }
    #[test]
    fn instance_ids_are_saved_and_duplicates_get_new_ones() {
        unsafe {
            let a = (API.create)();
            let id = handle_of::<Doubler>(a).id();
            let state = saved_state(a);
            // A duplicated track: the same state while the original lives.
            let b = (API.create)();
            assert!((API.load_state)(b, state.as_ptr(), state.len() as u32));
            assert_ne!(handle_of::<Doubler>(b).id(), id);
            (API.destroy)(a);
            (API.destroy)(b);
            // A reopened project: the original is gone, the ID comes back.
            let c = (API.create)();
            assert!((API.load_state)(c, state.as_ptr(), state.len() as u32));
            assert_eq!(handle_of::<Doubler>(c).id(), id);
            (API.destroy)(c);
        }
    }

    #[test]
    fn the_store_is_saved_with_the_project() {
        unsafe {
            let a = (API.create)();
            handle_of::<Doubler>(a).store().set("name", "リード");
            let state = saved_state(a);
            let b = (API.create)();
            let before = handle_of::<Doubler>(b).revision();
            assert!((API.load_state)(b, state.as_ptr(), state.len() as u32));
            assert_eq!(handle_of::<Doubler>(b).store().get_str("name").as_deref(), Some("リード"));
            assert!(handle_of::<Doubler>(b).revision() > before);
            // State without a store empties it: the project is what counts.
            let defaults = Params::default();
            let plain = state::serialize(&defaults, |i| defaults.cell(i).get(), &[], &Default::default());
            assert!((API.load_state)(b, plain.as_ptr(), plain.len() as u32));
            assert!(handle_of::<Doubler>(b).store().keys().is_empty());
            (API.destroy)(a);
            (API.destroy)(b);
        }
    }

    #[test]
    fn instances_are_listed_and_dropped_on_destroy() {
        unsafe {
            let a = (API.create)();
            let handle = handle_of::<Doubler>(a).clone();
            assert!(crate::instances::<Doubler>().contains(&handle));
            (API.destroy)(a);
            assert!(!handle.is_alive());
            assert!(!crate::instances::<Doubler>().contains(&handle));
            // A handle that outlived its instance is harmless.
            handle.edit("amount", 1.0);
            handle.wake();
        }
    }

    static EDITS: std::sync::Mutex<Vec<(u32, u32, f64)>> = std::sync::Mutex::new(Vec::new());
    static WAKES: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn record_edit(_ctx: *mut c_void, kind: u32, id: u32, normalized: f64) {
        EDITS.lock().unwrap().push((kind, id, normalized));
    }

    unsafe extern "C" fn record_wake(_ctx: *mut c_void) {
        WAKES.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn handle_edits_reach_the_host_and_wake_a_sleeping_instance() {
        unsafe {
            let a = new_sleeper(None);
            let host2 = (SLEEP_API.get_extension)(EXT_HOST2.as_ptr()) as *const HostApi2;
            let mut marker = 0u8;
            let ctx = &mut marker as *mut u8 as *mut c_void;
            let callbacks = crate::instance::HostCallbacks2 { ctx, edit: Some(record_edit), request_process: Some(record_wake) };
            ((*host2).attach)(a, &callbacks);
            let handle = handle_of::<Sleeper>(a).clone();
            assert_eq!(crate::InstanceHandle::for_shim_ctx(ctx), Some(handle.clone()));

            // Fall asleep (Tail::Auto: two seconds of silence).
            feed(a, 250, 0.0, &[]);
            assert!(silent(a));
            let calls = sleeper(a).calls;
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).calls, calls);

            // A gesture through the handle: recorded by the host, applied, and it wakes the instance.
            handle.begin("amount");
            handle.set("amount", 3.0);
            handle.end("amount");
            assert_eq!(handle.value("amount"), Some(3.0));
            let edits: Vec<_> = EDITS.lock().unwrap().iter().filter(|e| e.1 == 7).copied().collect();
            assert_eq!(edits, [(crate::instance::EDIT_BEGIN, 7, 0.5), (crate::instance::EDIT_PERFORM, 7, 0.75), (crate::instance::EDIT_END, 7, 0.75)]);
            assert!(WAKES.load(Ordering::Relaxed) >= 1);
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).calls, calls + 1);

            // A plain wake does the same.
            feed(a, 250, 0.0, &[]);
            let calls = sleeper(a).calls;
            handle.wake();
            feed(a, 1, 0.0, &[]);
            assert_eq!(sleeper(a).calls, calls + 1);

            (SLEEP_API.destroy)(a);
            assert_eq!(crate::InstanceHandle::for_shim_ctx(ctx), None);
        }
    }

    #[test]
    fn track_info_reaches_the_handle() {
        use crate::instance::{TRACK_HAS_COLOR, TRACK_HAS_NAME, TrackInfoC};
        unsafe {
            let a = (API.create)();
            let track = (API.get_extension)(EXT_TRACK.as_ptr()) as *const TrackApi;
            let info = TrackInfoC { flags: TRACK_HAS_NAME | TRACK_HAS_COLOR, name: FerritronStr::new("Vocal"), color: [255, 0, 128], index: 0, kind: 0, location: 0, channels: 0 };
            ((*track).set_track_info)(a, &info);
            let handle = handle_of::<Doubler>(a);
            assert_eq!(handle.track(), crate::TrackInfo { name: Some("Vocal".into()), color: Some([255, 0, 128]), ..Default::default() });
            // The v1 entry still sets the name.
            let host = (API.get_extension)(EXT_HOST.as_ptr()) as *const HostApi;
            ((*host).set_track_name)(a, FerritronStr::new("Lead"));
            assert_eq!(handle.track().name.as_deref(), Some("Lead"));
            (API.destroy)(a);
        }
    }

    // The same sizes are asserted in `include/ferritron_abi.h` (the FERRITRON_ABI_ASSERT lines at
    // its end). Change both together; a struct's layout is never changed once it has shipped.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn abi_sizes_match_the_c_header() {
        assert_eq!(size_of::<Event>(), 24);
        assert_eq!(size_of::<ProcessData>(), 64);
        assert_eq!(size_of::<ParamInfoC>(), 64);
        assert_eq!(size_of::<PluginInfoC>(), 176);
        assert_eq!(size_of::<PluginApi>(), 144);
        assert_eq!(size_of::<crate::transport::TransportC>(), 56);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod denormal_tests {
    use super::DenormalGuard;

    #[test]
    fn guard_flushes_denormals_and_restores_the_mode() {
        let tiny = std::hint::black_box(f32::MIN_POSITIVE / 4.0);
        assert!(tiny * std::hint::black_box(1.0f32) != 0.0);
        {
            let _guard = DenormalGuard::new();
            assert_eq!(tiny * std::hint::black_box(1.0f32), 0.0);
        }
        assert!(tiny * std::hint::black_box(1.0f32) != 0.0);
    }
}

#[cfg(test)]
mod sort_tests {
    use super::{Event, sort_events};

    #[test]
    fn sorts_by_offset_stably_like_the_c_shims() {
        // Pseudo-random offsets with many ties; `data1` keeps the arrival order.
        let mut seed = 12345u32;
        for n in [0usize, 1, 2, 3, 7, 64, 1000, 2048] {
            let mut events: Vec<Event> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    Event::midi((seed >> 16) % 37, 0x90, 0, (i % 128) as u8, (i / 128) as u8)
                })
                .collect();
            let mut expected = events.clone();
            expected.sort_by_key(|e| e.offset); // std's stable sort as the reference
            let mut scratch = vec![Event::midi(0, 0, 0, 0, 0); n];
            sort_events(&mut events, &mut scratch);
            let key = |e: &Event| (e.offset, unsafe { e.data.midi.data1 }, unsafe { e.data.midi.data2 });
            assert_eq!(events.iter().map(key).collect::<Vec<_>>(), expected.iter().map(key).collect::<Vec<_>>(), "n = {n}");
        }
    }

    #[test]
    fn sorted_input_needs_no_scratch() {
        let mut events = vec![Event::midi(0, 0x90, 0, 1, 1), Event::midi(5, 0x90, 0, 2, 1)];
        sort_events(&mut events, &mut []);
        assert_eq!(events[1].offset, 5);
    }
}
