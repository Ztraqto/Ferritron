//! Rust side of `include/ferritron_abi.h`: exposes a [`Process`] plugin as a
//! C function table that format shims call.

use crate::__private::PluginMeta;
use crate::{
    AudioIo, Context, NoteEvent, NoteKind, ParamCell, ParamInfo, ParamKind, ParamSet, PluginInfo, PluginKind, Process, Quality, Setup,
    Tail, state,
};
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub const ABI_VERSION: u32 = 1;

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
}

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
        }
    }
}

const PARAM_LIST: u32 = 1 << 0;
const PARAM_BYPASS: u32 = 1 << 1;
const PARAM_NOT_AUTOMATABLE: u32 = 1 << 2;

/// The "AI Access" switch of plugins with `[mcp]`: while on, AI clients may
/// reach the instance through the Ferritron MCP hub. Off for every new
/// instance and not automatable; saved with the project like any parameter
/// (formats require state to restore every parameter).
static MCP_SWITCH: ParamInfo = ParamInfo {
    id: crate::__private::param_id("ferritron.mcp"),
    key: "ferritron.mcp",
    name: "AI Access",
    unit: "",
    kind: ParamKind::Bool,
    default: 0.0,
};

/// The "Quality" switch of plugins with `[quality]`: Auto (best when the host
/// renders offline, normal otherwise), Normal or Best. Not automatable;
/// saved with the project.
static QUALITY: ParamInfo = ParamInfo {
    id: crate::__private::param_id(crate::i18n::QUALITY_KEY),
    key: crate::i18n::QUALITY_KEY,
    name: "Quality",
    unit: "",
    kind: ParamKind::Enum { choices: &["Auto", "Normal", "Best"] },
    default: 0.0,
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

/// A slot an extension may use for per-instance data (ARA's plug-in extension).
///
/// # Safety
/// `ptr` must be an instance of `P` created by this table.
#[doc(hidden)]
pub unsafe fn extension_slot<'a, P: Process>(ptr: *mut c_void) -> &'a std::sync::atomic::AtomicPtr<c_void> {
    unsafe { &instance::<P>(ptr).extension }
}

/// The editor size the user chose, saved with the project (logical pixels).
const EDITOR_WIDTH: u32 = crate::__private::param_id("ferritron.editor.width");
const EDITOR_HEIGHT: u32 = crate::__private::param_id("ferritron.editor.height");

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

/// `ferritron_host_api`.
#[cfg(any(windows, unix))]
#[repr(C)]
pub struct HostApi {
    pub attach: unsafe extern "C" fn(*mut c_void, *const crate::mcp::HostCallbacks),
    pub set_track_name: unsafe extern "C" fn(*mut c_void, FerritronStr),
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
    #[cfg(any(windows, unix))]
    const HOST: HostApi = HostApi { attach: attach_host::<P>, set_track_name: set_track_name::<P> };
}

#[cfg(any(windows, unix))]
unsafe extern "C" fn attach_host<P: Process>(ptr: *mut c_void, host: *const crate::mcp::HostCallbacks) {
    let instance = unsafe { instance::<P>(ptr) };
    if let (Some(shared), Some(host)) = (&instance.mcp_shared, unsafe { host.as_ref() }) {
        *shared.host.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*host);
    }
}

#[cfg(any(windows, unix))]
unsafe extern "C" fn set_track_name<P: Process>(ptr: *mut c_void, name: FerritronStr) {
    let instance = unsafe { instance::<P>(ptr) };
    if let (Some(shared), Some(name)) = (&instance.mcp_shared, unsafe { name.to_str() }) {
        *shared.track.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = name.to_string();
    }
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
    pub process: unsafe extern "C" fn(*mut c_void, *const *mut f32, u32, u32, *const Event, u32, *const Executor) -> bool,
    pub save_state: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> u32,
    pub load_state: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> bool,
    pub get_extension: unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void,
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
        }
    }
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
    #[cfg(any(windows, unix))]
    if id == EXT_HOST && P::MCP {
        return &Latency::<P>::HOST as *const HostApi as *const c_void;
    }
    match P::view() {
        Some(view) if id == EXT_VIEW => crate::isolation::editor(view, P::VIEW_PROCESS),
        Some(_) if id == EXT_EDITOR_SIZE => &Latency::<P>::EDITOR_SIZE as *const EditorSizeApi as *const c_void,
        _ => std::ptr::null(),
    }
}

struct Instance<P: Process> {
    params: P::Params,
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
    /// The tail as hosts are told ([`tail_samples`]), as of the last block.
    tail: AtomicU32,
    /// The last block slept.
    silent: AtomicBool,
    /// Editor size as `width << 32 | height`; 0 until the user resizes.
    editor_size: AtomicU64,
    /// Per-instance data of an extension (ARA).
    extension: std::sync::atomic::AtomicPtr<c_void>,
    #[cfg(any(windows, unix))]
    mcp_shared: Option<std::sync::Arc<crate::mcp::Shared>>,
}

/// Sets flush-to-zero / denormals-are-zero for the current thread while alive,
/// so denormal numbers cannot slow processing down.
pub(crate) struct DenormalGuard {
    #[cfg(target_arch = "x86_64")]
    saved: u32,
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
        #[cfg(not(target_arch = "x86_64"))]
        Self {}
    }
}

impl Drop for DenormalGuard {
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &self.saved, options(nostack, readonly));
        }
    }
}

/// Runs plugin code, turning a panic into `None` instead of unwinding into the host.
fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    catch_unwind(AssertUnwindSafe(f)).ok()
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

/// Parameter at `index`: the plugin's own, then bypass, then (with `[mcp]`)
/// the AI Access switch, then (with `[quality]`) the Quality switch.
fn info_at<P: Process + PluginMeta>(index: usize) -> Option<&'static ParamInfo> {
    let infos = P::Params::infos();
    match index {
        i if i == infos.len() => Some(&BYPASS),
        i if Some(i) == mcp_index::<P>() => Some(&MCP_SWITCH),
        i if Some(i) == quality_index::<P>() => Some(&QUALITY),
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

/// Whether every sample of the channels is silence.
fn is_quiet(channels: &[*mut f32], frames: usize) -> bool {
    channels.iter().all(|&p| {
        // SAFETY: the shim passes `frames` valid samples per channel.
        let samples = unsafe { std::slice::from_raw_parts(p, frames) };
        // A plain maximum vectorizes; NaN counts as quiet (it is not sound).
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs())) <= QUIET
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
                    "min": info.denormalize(0.0),
                    "max": info.denormalize(1.0),
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
        let host = *self.shared.host.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(edited) = host.and_then(|h| h.edited.map(|f| (h.ctx, f))) {
            unsafe { (edited.1)(edited.0, info.id, info.normalize(plain)) };
        }
        Ok(serde_json::json!({ "key": info.key, "value": plain, "text": P::localized().format(index, info, plain) }))
    }
}

pub(crate) unsafe extern "C" fn create<P: Process + PluginMeta>() -> *mut c_void {
    let Some(plugin) = guarded(P::default) else { return std::ptr::null_mut() };
    if P::PARALLEL {
        crate::parallel::acquire();
    }
    let instance = Instance::<P> {
        params: P::Params::default(),
        plugin: UnsafeCell::new(plugin),
        bypass: ParamCell::new(BYPASS.default),
        sample_rate: AtomicU64::new(44_100f64.to_bits()),
        failed: AtomicBool::new(false),
        latency: AtomicU32::new(0),
        offline: AtomicBool::new(false),
        mcp: ParamCell::new(MCP_SWITCH.default),
        quality: ParamCell::new(QUALITY.default),
        sleep: UnsafeCell::new(Sleep::default()),
        tail: AtomicU32::new(TAIL_UNKNOWN),
        silent: AtomicBool::new(false),
        editor_size: AtomicU64::new(0),
        extension: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        #[cfg(any(windows, unix))]
        mcp_shared: P::MCP.then(|| std::sync::Arc::new(crate::mcp::Shared::new())),
    };
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
        // Plugin destructors are plugin code too.
        let _ = guarded(move || drop(instance));
        let _ = guarded(|| P::preload_view(false));
        if P::PARALLEL {
            crate::parallel::release();
        }
    }
}

unsafe extern "C" fn param_count<P: Process + PluginMeta>() -> u32 {
    P::Params::infos().len() as u32 + 1 + P::MCP as u32 + P::QUALITY as u32
}

unsafe extern "C" fn param_info<P: Process + PluginMeta>(index: u32, out: *mut ParamInfoC) -> bool {
    let Some(info) = info_at::<P>(index as usize) else { return false };
    if out.is_null() {
        return false;
    }
    let flags = match info.kind {
        _ if info.id == BYPASS.id => PARAM_BYPASS,
        _ if info.id == MCP_SWITCH.id => PARAM_NOT_AUTOMATABLE,
        _ if info.id == QUALITY.id => PARAM_LIST | PARAM_NOT_AUTOMATABLE,
        ParamKind::Enum { .. } => PARAM_LIST,
        _ => 0,
    };
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
            default_normalized: info.normalize(info.default),
            flags,
        })
    };
    true
}

pub(crate) unsafe extern "C" fn get_param<P: Process + PluginMeta>(ptr: *mut c_void, id: u32) -> f64 {
    let instance = unsafe { instance::<P>(ptr) };
    lookup::<P>(id).map_or(0.0, |(index, info)| info.normalize(instance.cell(index).get()))
}

pub(crate) unsafe extern "C" fn set_param<P: Process + PluginMeta>(ptr: *mut c_void, id: u32, normalized: f64) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    let Some((index, info)) = lookup::<P>(id) else { return false };
    instance.cell(index).set(info.denormalize(normalized));
    true
}

pub(crate) unsafe extern "C" fn to_plain<P: Process + PluginMeta>(id: u32, normalized: f64) -> f64 {
    lookup::<P>(id).map_or(0.0, |(_, info)| info.denormalize(normalized))
}

pub(crate) unsafe extern "C" fn to_normalized<P: Process + PluginMeta>(id: u32, plain: f64) -> f64 {
    lookup::<P>(id).map_or(0.0, |(_, info)| info.normalize(plain))
}

unsafe extern "C" fn format_param<P: Process + PluginMeta>(id: u32, normalized: f64, buf: *mut u8, cap: u32) -> i32 {
    let Some((index, info)) = lookup::<P>(id) else { return -1 };
    if buf.is_null() {
        return -1;
    }
    let text = P::localized().format(index, info, info.denormalize(normalized));
    // Truncate at a character boundary.
    let mut len = text.len().min(cap as usize);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), buf, len) };
    len as i32
}

unsafe extern "C" fn parse_param<P: Process + PluginMeta>(id: u32, text: FerritronStr, out: *mut f64) -> bool {
    let Some((index, info)) = lookup::<P>(id) else { return false };
    let Some(text) = (unsafe { text.to_str() }) else { return false };
    match P::localized().parse(index, info, text) {
        Some(plain) if !out.is_null() => {
            unsafe { *out = info.normalize(plain) };
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
    let setup = Setup { sample_rate, max_block_size: max_block_size as usize, offline, quality: instance.quality() };
    instance.sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
    unsafe { *instance.sleep.get() = Sleep::default() };
    instance.silent.store(false, Ordering::Relaxed);
    let ok = guarded(|| {
        plugin.activate(&setup);
        plugin.reset();
        let latency = plugin.latency();
        instance.latency.store(latency, Ordering::Relaxed);
        instance.tail.store(tail_samples(plugin.tail(), latency), Ordering::Relaxed);
    })
    .is_some();
    instance.failed.store(!ok, Ordering::Relaxed);
    ok
}

/// Most channels `process` accepts; shims currently pass at most two.
const MAX_CHANNELS: usize = 8;

pub const EVENT_PARAM: u32 = 0;
pub const EVENT_NOTE_ON: u32 = 1;
pub const EVENT_NOTE_OFF: u32 = 2;

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

    pub fn note(offset: u32, on: bool, key: i16, channel: i16, velocity: f32, note_id: i32) -> Self {
        let kind = if on { EVENT_NOTE_ON } else { EVENT_NOTE_OFF };
        Self { offset, kind, data: EventData { note: NoteData { note_id, channel, key, velocity } } }
    }
}

/// Notes delivered to one processing call.
const MAX_NOTES: usize = 512;

pub(crate) unsafe extern "C" fn process<P: Process + PluginMeta>(
    ptr: *mut c_void,
    channels: *const *mut f32,
    num_channels: u32,
    frames: u32,
    events: *const Event,
    num_events: u32,
    executor: *const Executor,
) -> bool {
    let instance = unsafe { instance::<P>(ptr) };
    let executor = unsafe { executor.as_ref() };
    let events: &[Event] =
        if num_events == 0 || events.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(events, num_events as usize) } };
    let apply = |event: &Event| {
        if event.kind != EVENT_PARAM {
            return;
        }
        let param = unsafe { event.data.param };
        if let Some((index, info)) = lookup::<P>(param.id) {
            instance.cell(index).set(info.denormalize(param.normalized));
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
    let mut woken = false;
    for event in events {
        match event.kind {
            EVENT_PARAM => {
                let param = unsafe { event.data.param };
                // Hosts may resend unchanged values every block.
                woken |= lookup::<P>(param.id).is_some_and(|(index, info)| info.normalize(instance.cell(index).get()) != param.normalized);
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
    let input_quiet = is_quiet(channels, frames as usize);
    let busy = woken || !input_quiet || sleep.holding();
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

    // Split the block at every parameter change so automation is sample
    // accurate; each piece gets the notes that fall inside it.
    let ran = guarded(|| {
        let mut run = |start: usize, end: usize, notes: &[NoteEvent]| {
            // Bypassed: the shim already copied the input to the output.
            if end <= start || instance.bypass.get() >= 0.5 {
                return;
            }
            let mut ptrs = [std::ptr::null_mut::<f32>(); MAX_CHANNELS];
            for (slot, &ch) in ptrs.iter_mut().zip(channels) {
                *slot = unsafe { ch.add(start) };
            }
            let io = unsafe { AudioIo::from_raw(&ptrs[..channels.len()], end - start) };
            // Read per piece: the user's switch may change inside the block.
            let quality = instance.quality();
            let mut ctx = Context { params: &instance.params, io, notes, sample_rate, quality, executor };
            plugin.process(&mut ctx);
        };
        let frames = frames as usize;
        let empty = NoteEvent { offset: 0, kind: NoteKind::Off, key: 0, channel: 0, velocity: 0.0, note_id: -1 };
        let mut notes = [empty; MAX_NOTES];
        let (mut count, mut start) = (0, 0);
        for event in events {
            let at = (event.offset as usize).clamp(start, frames);
            match event.kind {
                EVENT_PARAM => {
                    if at > start {
                        run(start, at, &notes[..count]);
                        (count, start) = (0, at);
                    }
                    apply(event);
                }
                EVENT_NOTE_ON | EVENT_NOTE_OFF => {
                    let note = unsafe { event.data.note };
                    if count == MAX_NOTES || !(0..=127).contains(&note.key) || frames == 0 {
                        continue;
                    }
                    notes[count] = NoteEvent {
                        offset: (at.min(frames - 1) - start) as u32,
                        kind: if event.kind == EVENT_NOTE_ON { NoteKind::On } else { NoteKind::Off },
                        key: note.key as u8,
                        channel: note.channel.clamp(0, 15) as u8,
                        velocity: note.velocity.clamp(0.0, 1.0),
                        note_id: note.note_id,
                    };
                    count += 1;
                }
                _ => {}
            }
        }
        run(start, frames, &notes[..count]);
        plugin.tail()
    });
    let ran = match ran {
        Some(tail) => {
            let latency = instance.latency.load(Ordering::Relaxed);
            instance.tail.store(tail_samples(tail, latency), Ordering::Relaxed);
            sleep.update(tail, busy, || is_quiet(channels, frames as usize), frames as usize, sample_rate, latency);
            true
        }
        None => {
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

pub(crate) unsafe extern "C" fn save_state<P: Process + PluginMeta>(ptr: *mut c_void, buf: *mut u8, cap: u32) -> u32 {
    let instance = unsafe { instance::<P>(ptr) };
    let mut extra = vec![(BYPASS.id, instance.bypass.get())];
    if P::MCP {
        extra.push((MCP_SWITCH.id, instance.mcp.get()));
    }
    if P::QUALITY {
        extra.push((QUALITY.id, instance.quality.get()));
    }
    let size = instance.editor_size.load(Ordering::Relaxed);
    if size != 0 {
        extra.push((EDITOR_WIDTH, (size >> 32) as f64));
        extra.push((EDITOR_HEIGHT, (size & 0xffff_ffff) as f64));
    }
    let data = state::serialize(&instance.params, &extra);
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
    let pixels = |v: f64| if v.is_finite() && (1.0..=65_535.0).contains(&v) { v as u32 } else { 0 };
    let loaded = state::deserialize(&instance.params, data, |id, value| {
        if id == BYPASS.id {
            instance.bypass.set(BYPASS.sanitize(value));
        } else if P::MCP && id == MCP_SWITCH.id {
            instance.mcp.set(MCP_SWITCH.sanitize(value));
        } else if P::QUALITY && id == QUALITY.id {
            instance.quality.set(QUALITY.sanitize(value));
        } else if id == EDITOR_WIDTH {
            width = pixels(value);
        } else if id == EDITOR_HEIGHT {
            height = pixels(value);
        }
    });
    if loaded {
        unsafe { set_editor_size::<P>(ptr, width, height) };
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
        [ParamInfo { id: 7, key: "amount", name: "Amount", unit: "", kind: ParamKind::Float { min: 0.0, max: 4.0 }, default: 2.0 }];

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

    impl Process for Doubler {
        type Params = Params;
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            assert!(!self.panic_next, "plugin bug");
            let amount = ctx.params.amount.value();
            ctx.io.for_each_sample(|s| *s *= amount);
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
    };

    impl PluginMeta for Doubler {
        const INFO: &'static PluginInfo = &INFO;
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

    fn run(instance: *mut c_void, samples: &mut [f32]) -> bool {
        run_with(instance, samples, &[])
    }

    fn run_with(instance: *mut c_void, samples: &mut [f32], events: &[Event]) -> bool {
        let ptrs = [samples.as_mut_ptr()];
        unsafe { (API.process)(instance, ptrs.as_ptr(), 1, samples.len() as u32, events.as_ptr(), events.len() as u32, std::ptr::null()) }
    }

    #[derive(Default)]
    struct NoteCounter;

    impl Process for NoteCounter {
        type Params = Params;
        fn process(&mut self, ctx: &mut Context<'_, Params>) {
            // Write each note's key at its offset so the test can see where it landed.
            for note in ctx.notes {
                ctx.io.channel_mut(0)[note.offset as usize] = note.key as f32;
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
            self.quality = Some(ctx.quality);
            if self.tone {
                ctx.io.for_each_sample(|s| *s = 0.5);
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
            assert!(unsafe { (SLEEP_API.process)(instance, ptrs.as_ptr(), 1, BLOCK as u32, events.as_ptr(), events.len() as u32, std::ptr::null()) });
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
            assert!((NOTE_API.process)(a, ptrs.as_ptr(), 1, 6, events.as_ptr(), events.len() as u32, std::ptr::null()));
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
            assert_eq!(buf, [2.0, 2.0, 1.0, 1.0]); // bypassed from sample 2 on

            let data = saved_state(a);
            let b = (API.create)();
            assert!((API.load_state)(b, data.as_ptr(), data.len() as u32));
            assert_eq!((API.get_param)(b, BYPASS.id), 1.0);
            (API.destroy)(a);
            (API.destroy)(b);
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
