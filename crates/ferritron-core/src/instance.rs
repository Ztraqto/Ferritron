//! What a plugin instance knows about itself and its host, and what other
//! code may do with it: [`InstanceHandle`].
//!
//! Every instance has a handle. The plugin gets it in [`Process::init`]
//! (and on the audio thread through [`Context::instance`]), editors through
//! their `Ui` (GPU) or `window.ferritron` (web), and any code in the process
//! through [`instances`]. A handle is `Send + Sync` and cheap to clone; it
//! stays valid after its instance is destroyed (then [`InstanceHandle::is_alive`]
//! is false and edits go nowhere).
//!
//! - **Identity**: [`InstanceHandle::id`], saved with the project, new for a
//!   duplicated track.
//! - **Track**: the name, color and position of the mixer channel the
//!   instance sits on, when the host says ([`TrackInfo`]).
//! - **Edits**: [`InstanceHandle::begin`] / [`set`](InstanceHandle::set) /
//!   [`end`](InstanceHandle::end) change a parameter from any thread and tell
//!   the host, so DAWs record them as automation (as edits in the editor do).
//! - **Wake**: [`InstanceHandle::wake`] makes a sleeping instance (see
//!   [`Tail`](crate::Tail)) process its next block.
//! - **Store**: [`InstanceHandle::store`], a key-value store saved with the
//!   project (names, notes, anything that is not a parameter).
//! - **Messages**: [`InstanceHandle::on_message`] and [`InstanceHandle::post`]
//!   exchange JSON text with the instance's web editor.
//! - **Notifications**: [`InstanceHandle::notify`] (any thread, the audio
//!   thread included) runs the [`InstanceHandle::on_notify`] handler on the
//!   host's main thread.
//!
//! [`Process::init`]: crate::Process::init
//! [`Context::instance`]: crate::Context::instance

use crate::{ParamCell, ParamInfo, ParamSet, Process};
use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

/// `ParamInfo::normalize` that survives a panic in a plugin's own scale (`scale = "custom"`):
/// `fallback` is used instead. Such a panic must not unwind into the host.
fn normalize_or(info: &ParamInfo, plain: f64, fallback: f64) -> f64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| info.normalize(plain))).unwrap_or(fallback)
}

/// `ParamInfo::denormalize` that survives a panic in a plugin's own scale (`scale = "custom"`).
fn denormalize_or(info: &ParamInfo, normalized: f64, fallback: f64) -> f64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| info.denormalize(normalized))).unwrap_or(fallback)
}

/// `ferritron_host_callbacks` (`ferritron.host.v1`): the shim's way to tell
/// the host about a parameter the plugin changed by itself (any thread).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostCallbacks {
    pub ctx: *mut c_void,
    pub edited: Option<unsafe extern "C" fn(*mut c_void, u32, f64)>,
}

// SAFETY: the shim promises the callbacks may be called from any thread.
unsafe impl Send for HostCallbacks {}
unsafe impl Sync for HostCallbacks {}

/// [`HostCallbacks2::edit`]: a gesture starts.
pub const EDIT_BEGIN: u32 = 0;
/// [`HostCallbacks2::edit`]: a new value during a gesture.
pub const EDIT_PERFORM: u32 = 1;
/// [`HostCallbacks2::edit`]: the gesture ends.
pub const EDIT_END: u32 = 2;

/// `ferritron_host_callbacks2` (`ferritron.host.v2`): edits as gestures and
/// waking a sleeping instance. Every entry may be called from any thread.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostCallbacks2 {
    /// The shim's object for the instance. The same pointer is the `ctx` of
    /// the `ferritron_view_host` the shim passes to the instance's editor,
    /// which is how an editor finds its instance.
    pub ctx: *mut c_void,
    /// `edit(ctx, EDIT_*, id, normalized)`: begin, perform or end an edit.
    pub edit: Option<unsafe extern "C" fn(*mut c_void, u32, u32, f64)>,
    /// The instance wants `process` called soon (the host may have stopped
    /// calling it while the output was silent).
    pub request_process: Option<unsafe extern "C" fn(*mut c_void)>,
}

unsafe impl Send for HostCallbacks2 {}
unsafe impl Sync for HostCallbacks2 {}

/// [`TrackInfoC::flags`]: `name` is set.
pub const TRACK_HAS_NAME: u32 = 1 << 0;
/// [`TrackInfoC::flags`]: `color` is set.
pub const TRACK_HAS_COLOR: u32 = 1 << 1;
/// [`TrackInfoC::flags`]: `index` is set.
pub const TRACK_HAS_INDEX: u32 = 1 << 2;
/// [`TrackInfoC::flags`]: `kind` is set (2.0).
pub const TRACK_HAS_KIND: u32 = 1 << 3;
/// [`TrackInfoC::flags`]: `location` is set (2.0).
pub const TRACK_HAS_LOCATION: u32 = 1 << 4;
/// [`TrackInfoC::flags`]: `channels` is set (2.0).
pub const TRACK_HAS_CHANNELS: u32 = 1 << 5;

/// `ferritron_track_info` (`ferritron.track.v1`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrackInfoC {
    pub flags: u32,
    /// UTF-8.
    pub name: crate::abi::FerritronStr,
    /// Red, green, blue.
    pub color: [u8; 3],
    /// Position of the track in the host's mixer, from 0.
    pub index: u32,
    /// (2.0) [`TrackKind`] as a number: 0 track, 1 return, 2 bus, 3 master.
    pub kind: u32,
    /// (2.0) [`PluginLocation`] as a number: 0 before the fader, 1 after it, 2 as the panner.
    pub location: u32,
    /// (2.0) Audio channels of the track.
    pub channels: u32,
}

/// What kind of mixer channel an instance sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrackKind {
    /// An ordinary track.
    Track,
    /// A return (FX, aux) track.
    Return,
    /// A bus (group).
    Bus,
    /// The master channel.
    Master,
}

/// Where in the channel strip an instance sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginLocation {
    /// An insert before the volume fader.
    PreFader,
    /// An insert after the volume fader.
    PostFader,
    /// In the channel's panner slot.
    Panner,
}

/// The track (mixer channel) an instance sits on, as far as the host says.
/// VST3 hosts tell through `IInfoListener`, CLAP hosts through `track-info`;
/// many hosts tell only the name, some nothing. No format tells a track's
/// mute, solo or fader position (see `docs/host-mixer-info.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub name: Option<String>,
    /// Red, green, blue.
    pub color: Option<[u8; 3]>,
    /// Position in the host's mixer, from 0 (VST3).
    pub index: Option<u32>,
    /// Track, return, bus or master (CLAP).
    pub kind: Option<TrackKind>,
    /// Before or after the fader, or the panner (VST3).
    pub location: Option<PluginLocation>,
    /// Audio channels of the track (CLAP).
    pub channels: Option<u32>,
}

/// An instance's identity: stable while the instance lives, saved with the
/// project (a reopened project gives each instance its old ID), and new when
/// the host duplicates an instance (a copied track).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(u64);

impl InstanceId {
    pub const fn get(self) -> u64 {
        self.0
    }

    /// A new, practically unique ID (never 0).
    fn random() -> Self {
        use std::hash::{BuildHasher, Hasher};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        // WebAssembly has no process ids (std panics) and may have no clock.
        #[cfg(not(target_family = "wasm"))]
        {
            if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                hasher.write_u128(now.as_nanos());
            }
            hasher.write_u32(std::process::id());
        }
        Self(hasher.finish().max(1))
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// The parameters of an instance without its type.
trait Cells: Send + Sync {
    fn infos(&self) -> &'static [ParamInfo];
    fn cell(&self, index: usize) -> &ParamCell;
    fn stream_hub(&self) -> Option<&crate::stream::StreamHub>;
}

impl<S: ParamSet> Cells for S {
    fn stream_hub(&self) -> Option<&crate::stream::StreamHub> {
        ParamSet::stream_hub(self)
    }
    fn infos(&self) -> &'static [ParamInfo] {
        S::infos()
    }
    fn cell(&self, index: usize) -> &ParamCell {
        ParamSet::cell(self, index)
    }
}

#[derive(Clone, Copy)]
enum Host {
    None,
    V1(HostCallbacks),
    V2(HostCallbacks2),
}

type Handler = Box<dyn FnMut(&str, &InstanceHandle) + Send>;
type NotifyHandler = Box<dyn FnMut(u64, &InstanceHandle) + Send>;

/// Messages kept for an editor that is not open; older ones are dropped.
const OUTBOX_LIMIT: usize = 256;

/// Per-instance data shared by the instance, its editors and handles.
pub(crate) struct Link {
    plugin_id: &'static str,
    id: AtomicU64,
    params: Arc<dyn Cells>,
    typed: Arc<dyn Any + Send + Sync>,
    alive: AtomicBool,
    /// The controller copy of a `[vst3] controller_id` plugin (`hide`): not listed anywhere.
    hidden: AtomicBool,
    host: Mutex<Host>,
    track: Mutex<TrackInfo>,
    /// Set by `wake`, taken by the next `process` call.
    pub(crate) wake: AtomicBool,
    store: Mutex<BTreeMap<String, Vec<u8>>>,
    revision: AtomicU64,
    handler: Mutex<Option<Handler>>,
    outbox: Mutex<VecDeque<String>>,
    /// Work for the host's main thread (`abi::REQUEST_*`), taken by shims.
    pub(crate) main_requests: std::sync::atomic::AtomicU32,
    /// Values set in bulk (`load_values`), applied by the next `process`.
    bulk: Mutex<Bulk>,
    /// bulk.pending, readable without the lock (the audio thread looks at it every block).
    bulk_pending: AtomicBool,
    /// How many editors of this instance are open.
    editors: Arc<std::sync::atomic::AtomicU32>,
    /// Codes for the main thread (`InstanceHandle::notify`), and their handler.
    pub(crate) notifications: crate::notify::NotifyQueue,
    notify_handler: Mutex<Option<NotifyHandler>>,
    /// Held while notifications are delivered (the shim's poll and the fallback thread).
    delivering: Mutex<()>,
    /// When the shim last polled `take_requests` (milliseconds of `clock_ms`); creation time at first.
    pub(crate) last_poll: AtomicU64,
    /// The shim asks `pending` and polls when there is work (CLAP's `request_callback`):
    /// the fallback thread leaves this instance alone.
    pub(crate) polls_on_demand: AtomicBool,
    /// The shim has polled take_requests at least once: it delivers on its main thread.
    pub(crate) polled: AtomicBool,
    /// The plugin's factory presets, and the built-in "Program" parameter: the preset chosen
    /// (by the host or `presets::load`) and the one last loaded.
    presets: &'static [crate::presets::Preset],
    pub(crate) program: ParamCell,
    /// Grows when parameter values move (any thread; relaxed), for `instances_watch`.
    pub(crate) params_revision: AtomicU64,
    program_applied: AtomicU64,
    /// Data the plugin attached for its editor, by type (`InstanceHandle::attach`).
    shared: Mutex<std::collections::HashMap<std::any::TypeId, Arc<dyn Any + Send + Sync>>>,
}

/// Parameter values waiting to be applied together at the start of a block.
struct Bulk {
    pending: bool,
    /// Plain values by parameter index; NaN = leave as is. Allocated once.
    values: Vec<f64>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every live instance in the process, in creation order.
static REGISTRY: Mutex<Vec<Weak<Link>>> = Mutex::new(Vec::new());
/// Bumped when an instance is created or destroyed.
static REGISTRY_REVISION: AtomicU64 = AtomicU64::new(0);
/// Shim objects (`HostCallbacks2::ctx`) by instance, so editors find theirs.
static SHIM_CTX: Mutex<Vec<(usize, Weak<Link>)>> = Mutex::new(Vec::new());

impl Link {
    pub(crate) fn bump_params(&self) {
        self.params_revision.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn new<P: Process>(plugin_id: &'static str, params: Arc<P::Params>, presets: &'static [crate::presets::Preset]) -> Arc<Self> {
        let link = Arc::new(Self {
            plugin_id,
            id: AtomicU64::new(InstanceId::random().0),
            params: params.clone(),
            typed: params,
            alive: AtomicBool::new(true),
            hidden: AtomicBool::new(false),
            host: Mutex::new(Host::None),
            track: Mutex::new(TrackInfo::default()),
            wake: AtomicBool::new(false),
            store: Mutex::new(BTreeMap::new()),
            revision: AtomicU64::new(0),
            handler: Mutex::new(None),
            outbox: Mutex::new(VecDeque::new()),
            main_requests: std::sync::atomic::AtomicU32::new(0),
            bulk: Mutex::new(Bulk { pending: false, values: Vec::new() }),
            bulk_pending: AtomicBool::new(false),
            editors: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            notifications: crate::notify::NotifyQueue::new(),
            notify_handler: Mutex::new(None),
            delivering: Mutex::new(()),
            last_poll: AtomicU64::new(clock_ms()),
            polls_on_demand: AtomicBool::new(false),
            polled: AtomicBool::new(false),
            presets,
            program: ParamCell::new(0.0),
            params_revision: AtomicU64::new(0),
            program_applied: AtomicU64::new(0f64.to_bits()),
            shared: Mutex::new(std::collections::HashMap::new()),
        });
        // Data streams keep their writes only while an editor is open.
        if let Some(hub) = link.params.stream_hub() {
            hub.link(&link.editors);
        }
        let mut registry = lock(&REGISTRY);
        registry.retain(|w| w.strong_count() > 0);
        registry.push(Arc::downgrade(&link));
        REGISTRY_REVISION.fetch_add(1, Ordering::AcqRel);
        drop(registry);
        let handle = InstanceHandle(link.clone());
        crate::watch::emit(move || crate::watch::InstanceEvent::Added(handle));
        link
    }

    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        // Only while a watch exists: find this link's `Arc` among the live ones.
        if crate::watch::any() {
            let me = lock(&REGISTRY).iter().filter_map(Weak::upgrade).find(|l| std::ptr::eq(&**l, self));
            if let Some(link) = me {
                crate::watch::emit(move || crate::watch::InstanceEvent::Changed(InstanceHandle(link)));
            }
        }
    }

    /// Any thread (the audio thread included): if the "Program" parameter was set to another
    /// preset, asks the main thread to load it. Lock-free.
    pub(crate) fn check_program(&self) {
        if !self.presets.is_empty() && self.program.get().to_bits() != self.program_applied.load(Ordering::Acquire) {
            self.main_requests.fetch_or(crate::abi::REQUEST_NOTIFY, Ordering::Release);
        }
    }

    /// The "Program" parameter now says `index` and that preset is (being) loaded.
    pub(crate) fn set_program(&self, index: f64) {
        self.program.set(index);
        self.program_applied.store(index.to_bits(), Ordering::Release);
    }

    /// Main thread: loads the preset the "Program" parameter was set to, if it changed.
    fn apply_program(self: &Arc<Self>) {
        if self.presets.is_empty() {
            return;
        }
        let v = self.program.get();
        if self.program_applied.swap(v.to_bits(), Ordering::AcqRel) != v.to_bits() {
            if let Some(preset) = self.presets.get(v.round().max(0.0) as usize) {
                InstanceHandle(self.clone()).load_values(preset.values);
            }
        }
    }

    /// Audio thread, at the start of a block: applies values set with
    /// `load_values` all at once. Never blocks (a handle holding the lock
    /// just delays the values to the next block) and never allocates.
    pub(crate) fn apply_bulk(&self) -> bool {
        // Without the lock: a block with nothing pending never touches it.
        if !self.bulk_pending.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut bulk) = self.bulk.try_lock() else { return false };
        if !bulk.pending {
            self.bulk_pending.store(false, Ordering::Release);
            return false;
        }
        let infos = self.params.infos();
        for (i, v) in bulk.values.iter_mut().enumerate().take(infos.len()) {
            if !v.is_nan() {
                self.params.cell(i).set(infos[i].sanitize(*v));
                *v = f64::NAN;
            }
        }
        bulk.pending = false;
        self.bulk_pending.store(false, Ordering::Release);
        true
    }

    /// The value `load_values` set for parameter `index` that the audio thread has not applied
    /// yet. Any thread; never blocks (a busy lock reads as "none").
    pub(crate) fn pending_value(&self, index: usize) -> Option<f64> {
        if !self.bulk_pending.load(Ordering::Acquire) {
            return None;
        }
        let bulk = self.bulk.try_lock().ok()?;
        let v = *bulk.values.get(index).filter(|_| bulk.pending)?;
        (!v.is_nan()).then(|| self.params.infos()[index].sanitize(v))
    }

    /// The controller copy of a `[vst3] controller_id` plugin: the instance is not one people use,
    /// so it leaves `instances()`, `instances_watch` and the MCP hub (the shim's `set_role`). Its
    /// `Removed` event goes out here; `destroy` sends none for a hidden instance.
    pub(crate) fn hide(self: &Arc<Self>) {
        if self.hidden.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::downgrade(self);
        lock(&REGISTRY).retain(|w| w.strong_count() > 0 && !w.ptr_eq(&me));
        REGISTRY_REVISION.fetch_add(1, Ordering::AcqRel);
        let (id, plugin_id) = (InstanceId(self.id()), self.plugin_id);
        crate::watch::emit(move || crate::watch::InstanceEvent::Removed { id, plugin_id });
    }

    pub(crate) fn is_hidden(&self) -> bool {
        self.hidden.load(Ordering::Acquire)
    }

    /// The instance goes away: no more host calls, no handler (which might
    /// hold handles), out of the registries.
    pub(crate) fn destroy(self: &Arc<Self>) {
        self.alive.store(false, Ordering::Release);
        *lock(&self.host) = Host::None;
        let handler = lock(&self.handler).take();
        drop(handler);
        let handler = lock(&self.notify_handler).take();
        drop(handler);
        lock(&self.outbox).clear();
        // Attached values go with the instance, here on the main thread.
        let attached: Vec<_> = lock(&self.shared).drain().collect();
        drop(attached);
        let me = Arc::downgrade(self);
        lock(&REGISTRY).retain(|w| w.strong_count() > 0 && !w.ptr_eq(&me));
        lock(&SHIM_CTX).retain(|(_, w)| w.strong_count() > 0 && !w.ptr_eq(&me));
        REGISTRY_REVISION.fetch_add(1, Ordering::AcqRel);
        self.revision.fetch_add(1, Ordering::AcqRel);
        if !self.is_hidden() {
            let (id, plugin_id) = (InstanceId(self.id()), self.plugin_id);
            crate::watch::emit(move || crate::watch::InstanceEvent::Removed { id, plugin_id });
        }
    }

    fn remember_ctx(self: &Arc<Self>, ctx: *mut c_void) {
        if ctx.is_null() {
            return;
        }
        let mut map = lock(&SHIM_CTX);
        map.retain(|(c, w)| w.strong_count() > 0 && *c != ctx as usize);
        map.push((ctx as usize, Arc::downgrade(self)));
    }

    pub(crate) fn attach_v1(self: &Arc<Self>, host: HostCallbacks) {
        let mut current = lock(&self.host);
        // A shim that speaks v2 attaches with it; v1 never replaces it.
        if !matches!(*current, Host::V2(_)) {
            *current = Host::V1(host);
        }
        drop(current);
        self.remember_ctx(host.ctx);
    }

    pub(crate) fn attach_v2(self: &Arc<Self>, host: HostCallbacks2) {
        *lock(&self.host) = Host::V2(host);
        self.remember_ctx(host.ctx);
    }

    pub(crate) fn set_track(&self, info: TrackInfo) {
        let mut track = lock(&self.track);
        if *track != info {
            *track = info;
            drop(track);
            self.changed();
        }
    }

    pub(crate) fn set_track_name(&self, name: &str) {
        let mut info = lock(&self.track).clone();
        info.name = (!name.is_empty()).then(|| name.to_string());
        self.set_track(info);
    }

    pub(crate) fn track_name(&self) -> String {
        lock(&self.track).name.clone().unwrap_or_default()
    }

    pub(crate) fn id(&self) -> u64 {
        self.id.load(Ordering::Acquire)
    }

    /// A loaded project says this instance had `id`. Another live instance
    /// of the same plugin with that ID means the host duplicated it: keep
    /// the new ID then.
    pub(crate) fn restore_id(&self, id: u64) {
        if id == 0 || id == self.id() {
            return;
        }
        let taken = lock(&REGISTRY)
            .iter()
            .filter_map(Weak::upgrade)
            .any(|other| !std::ptr::eq(&*other, self) && other.plugin_id == self.plugin_id && other.id() == id && other.alive.load(Ordering::Acquire));
        if !taken {
            self.id.store(id, Ordering::Release);
            self.changed();
        }
    }

    pub(crate) fn store_snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        lock(&self.store).clone()
    }

    pub(crate) fn replace_store(&self, store: BTreeMap<String, Vec<u8>>) {
        *lock(&self.store) = store;
        self.changed();
    }

    /// Tells the host (if it listens) about an edit of parameter `index`.
    fn notify(&self, kind: u32, index: usize) {
        let info = &self.params.infos()[index];
        let normalized = normalize_or(info, self.params.cell(index).get(), 0.0);
        // Held during the call: `destroy` cannot pull the shim away meanwhile.
        let host = lock(&self.host);
        match *host {
            Host::V2(h) => {
                if let Some(edit) = h.edit {
                    unsafe { edit(h.ctx, kind, info.id, normalized) };
                }
            }
            Host::V1(h) if kind == EDIT_PERFORM => {
                if let Some(edited) = h.edited {
                    unsafe { edited(h.ctx, info.id, normalized) };
                }
            }
            _ => {}
        }
    }

    /// A one-shot edit of any parameter (built-in switches too), already
    /// applied: tells the host as begin, perform, end.
    pub(crate) fn edited(&self, id: u32, normalized: f64) {
        let host = lock(&self.host);
        match *host {
            Host::V2(HostCallbacks2 { ctx, edit: Some(edit), .. }) => unsafe {
                edit(ctx, EDIT_BEGIN, id, normalized);
                edit(ctx, EDIT_PERFORM, id, normalized);
                edit(ctx, EDIT_END, id, normalized);
            },
            Host::V1(HostCallbacks { ctx, edited: Some(edited) }) => unsafe { edited(ctx, id, normalized) },
            _ => {}
        }
    }

    fn wake(&self) {
        self.wake.store(true, Ordering::Release);
        let host = lock(&self.host);
        if let Host::V2(HostCallbacks2 { ctx, request_process: Some(request), .. }) = *host {
            unsafe { request(ctx) };
        }
    }
}

/// A handle to a plugin instance (see the [module docs](self)).
#[derive(Clone)]
pub struct InstanceHandle(pub(crate) Arc<Link>);

impl PartialEq for InstanceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for InstanceHandle {}

impl fmt::Debug for InstanceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstanceHandle").field("plugin", &self.0.plugin_id).field("id", &self.id()).field("alive", &self.is_alive()).finish()
    }
}

impl InstanceHandle {
    /// The instance's identity (see [`InstanceId`]). It may change once,
    /// when the host loads a project into the instance.
    pub fn id(&self) -> InstanceId {
        InstanceId(self.0.id())
    }

    /// The plugin's `id` from the project file.
    pub fn plugin_id(&self) -> &'static str {
        self.0.plugin_id
    }

    /// False once the instance was destroyed.
    pub fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::Acquire)
    }

    /// The controller copy of a `[vst3] controller_id` plugin (not listed by `instances`).
    pub(crate) fn is_hidden(&self) -> bool {
        self.0.is_hidden()
    }

    /// The track the instance sits on (see [`TrackInfo`]). Not for the audio thread (locks).
    pub fn track(&self) -> TrackInfo {
        lock(&self.0.track).clone()
    }

    /// Whether an editor of this instance is open. Lock-free: the audio
    /// thread may ask (`Context::editor_open`) to stop analysis nobody sees.
    pub fn is_editor_open(&self) -> bool {
        self.0.editors.load(Ordering::Acquire) > 0
    }

    /// Editors call this when they open (`true`) and close (`false`).
    #[doc(hidden)]
    pub fn editor_opened(&self, open: bool) {
        use std::sync::atomic::Ordering::AcqRel;
        if open {
            if self.0.editors.fetch_add(1, AcqRel) == 0 {
                crate::watch::editor_open_changed(self, true);
            }
        } else if self.0.editors.fetch_update(AcqRel, Ordering::Acquire, |n| n.checked_sub(1)) == Ok(1) {
            crate::watch::editor_open_changed(self, false);
        }
    }

    /// Editors call this when their window gets the keyboard focus
    /// ([`InstanceEvent::EditorFocused`](crate::watch::InstanceEvent::EditorFocused)).
    #[doc(hidden)]
    pub fn editor_focused(&self) {
        crate::watch::editor_focused(self);
    }

    /// A number that grows when the instance's parameter values move. Lock-free.
    pub fn params_revision(&self) -> u64 {
        self.0.params_revision.load(Ordering::Relaxed)
    }

    pub(crate) fn link_key(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// The streams the plugin declared under `[streams]`: `(name, len, kind)`.
    pub fn streams(&self) -> Vec<(&'static str, usize, crate::stream::StreamKind)> {
        self.0.params.stream_hub().map(|h| h.list()).unwrap_or_default()
    }

    /// A reader of data stream `name` (the newest values the audio thread wrote while an
    /// editor was open). Every call makes an independent reader.
    pub fn stream(&self, name: &str) -> Option<crate::stream::StreamReader> {
        self.0.params.stream_hub()?.reader(name)
    }

    /// The reader of tap `name`, for a worker. There is one reader at a time: `None` while
    /// another exists (it is free again when that one is dropped).
    pub fn tap(&self, name: &str) -> Option<crate::stream::TapReader> {
        self.0.params.stream_hub()?.tap(name)
    }

    /// Hands the editor (or any other thread) a value of type `T`, kept until
    /// the instance goes away (and dropped there, never on the audio thread).
    /// A second `attach::<T>` replaces the first. Not for the audio thread.
    pub fn attach<T: Any + Send + Sync>(&self, value: Arc<T>) {
        lock(&self.0.shared).insert(std::any::TypeId::of::<T>(), value);
    }

    /// The value attached with [`attach`](Self::attach), if any.
    pub fn shared<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        lock(&self.0.shared).get(&std::any::TypeId::of::<T>()).cloned()?.downcast::<T>().ok()
    }

    /// A number that grows whenever the instance's identity, track or store
    /// changes, or it is destroyed: cheap to poll from an editor.
    pub fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }

    /// The instance's typed parameters, if it is an instance of `P`.
    pub fn params<P: Process>(&self) -> Option<Arc<P::Params>> {
        self.0.typed.clone().downcast::<P::Params>().ok()
    }

    /// The plugin's parameters, in project-file order.
    pub fn param_infos(&self) -> &'static [ParamInfo] {
        self.0.params.infos()
    }

    fn index(&self, key: &str) -> Option<usize> {
        self.0.params.infos().iter().position(|i| i.key == key)
    }

    /// The plain value (e.g. dB) of parameter `key`; lock-free, any thread.
    pub fn value(&self, key: &str) -> Option<f64> {
        self.index(key).map(|i| self.0.pending_value(i).unwrap_or_else(|| self.0.params.cell(i).get()))
    }

    /// The value of parameter `key` as 0..1; lock-free, any thread.
    pub fn normalized(&self, key: &str) -> Option<f64> {
        self.index(key).map(|i| {
            let plain = self.0.pending_value(i).unwrap_or_else(|| self.0.params.cell(i).get());
            normalize_or(&self.0.params.infos()[i], plain, 0.0)
        })
    }

    /// Sets many parameters at once, as a preset, Undo or an A/B switch does
    /// (`values` are `(parameter index, plain value)` pairs; the index is the
    /// position in the project file). The audio thread switches to all of them
    /// at the start of one block, never running with half of them. Unlike
    /// [`set`](Self::set) this is not an automation gesture: hosts record no
    /// automation and no undo steps for it; they are told the values changed
    /// (VST3 `kParamValuesChanged`, CLAP `rescan(VALUES)`) and the project
    /// is marked as changed. Until then, the host, saved state and
    /// `InstanceHandle::value` already see the new values. Any thread except the audio thread.
    pub fn load_values(&self, values: &[(usize, f64)]) {
        let n = self.0.params.infos().len();
        {
            let mut bulk = lock(&self.0.bulk);
            if bulk.values.len() < n {
                bulk.values.resize(n, f64::NAN);
            }
            for &(i, v) in values {
                if i < n {
                    bulk.values[i] = v;
                }
            }
            bulk.pending = true;
            self.0.bulk_pending.store(true, Ordering::Release);
        }
        self.0.bump_params();
        self.0.main_requests.fetch_or(crate::abi::REQUEST_VALUES, Ordering::Release);
        self.0.changed();
        self.0.wake();
    }

    /// Like [`load_values`](Self::load_values) with parameter keys.
    pub fn load_values_by_key(&self, values: &[(&str, f64)]) {
        let pairs: Vec<(usize, f64)> = values.iter().filter_map(|&(key, v)| Some((self.index(key)?, v))).collect();
        self.load_values(&pairs);
    }

    /// Starts a gesture on parameter `key` (any thread, not the audio thread).
    pub fn begin(&self, key: &str) {
        if let Some(i) = self.index(key) {
            self.0.notify(EDIT_BEGIN, i);
        }
    }

    /// Sets parameter `key` to a plain value and tells the host, which
    /// records it as automation inside a gesture. Wakes the instance.
    /// Any thread except the audio thread.
    pub fn set(&self, key: &str, plain: f64) {
        if let Some(i) = self.index(key) {
            let info = &self.0.params.infos()[i];
            self.0.params.cell(i).set(info.sanitize(plain));
            self.0.bump_params();
            self.0.notify(EDIT_PERFORM, i);
            self.0.wake();
        }
    }

    /// Like [`set`](Self::set), with a value from 0 to 1.
    pub fn set_normalized(&self, key: &str, normalized: f64) {
        if let Some(i) = self.index(key) {
            let plain = denormalize_or(&self.0.params.infos()[i], normalized, self.0.params.cell(i).get());
            self.set(key, plain);
        }
    }

    /// Ends the gesture on parameter `key`.
    pub fn end(&self, key: &str) {
        if let Some(i) = self.index(key) {
            self.0.notify(EDIT_END, i);
        }
    }

    /// A one-shot change: begin, set, end.
    pub fn edit(&self, key: &str, plain: f64) {
        self.begin(key);
        self.set(key, plain);
        self.end(key);
    }

    /// Makes the instance process its next block even if it sleeps (see
    /// [`Tail`](crate::Tail)): for changes `process` must see that are not
    /// parameter edits. Any thread except the audio thread (it takes a short lock to reach the host).
    pub fn wake(&self) {
        self.0.wake();
    }

    /// The instance's key-value store, saved with the project. Not for the
    /// audio thread (locks).
    pub fn store(&self) -> Store<'_> {
        Store(&self.0)
    }

    /// Handles messages from the instance's web editor (`ferritron.send(...)`,
    /// JSON text), on the UI thread. Replaces the previous handler. The
    /// handler gets the instance's handle; holding another clone of it inside
    /// the handler is fine (it is dropped with the instance).
    pub fn on_message(&self, handler: impl FnMut(&str, &InstanceHandle) + Send + 'static) {
        if self.is_alive() {
            *lock(&self.0.handler) = Some(Box::new(handler));
        }
    }

    /// Sends JSON text to the instance's web editor (`ferritron.onMessage`),
    /// from any thread except the audio thread. Kept until an editor shows
    /// it (at most 256 messages).
    pub fn post(&self, json: impl Into<String>) {
        if !self.is_alive() {
            return;
        }
        let mut outbox = lock(&self.0.outbox);
        if outbox.len() == OUTBOX_LIMIT {
            outbox.pop_front();
        }
        outbox.push_back(json.into());
    }

    /// Asks the host's main thread to run the [`on_notify`](Self::on_notify) handler with
    /// `code`: for work the audio thread must not do (loading a preset file after a program
    /// change, rebuilding a lookup table, telling the editor). Lock-free and allocation-free;
    /// any thread, the audio thread included (`ctx.instance().notify(code)`).
    ///
    /// A code that is already waiting is not queued twice. At most
    /// [`notify::CAPACITY`](crate::notify::CAPACITY) different codes wait at once; more are
    /// dropped and counted ([`notify_dropped`](Self::notify_dropped)). The handler runs on the
    /// host's main thread when the format's shim polls (VST3, VST2, AAX: every 30 ms; CLAP:
    /// `on_main_thread`, asked for after the block, so a code from another thread waits for the
    /// next block), and never after the instance is destroyed. Formats whose host gives no main
    /// thread callback (LV2, the standalone app, AU, WAM) run it on Ferritron's own thread
    /// `ferritron-notify` within about 0.1 s; with `[isolation]`, on a thread of the plugin's
    /// process. A format that polls never has its handler run on that thread, however long its
    /// main thread is busy. Returns false if the code was dropped.
    pub fn notify(&self, code: u64) -> bool {
        if !self.0.notifications.push(code) {
            return false;
        }
        self.0.main_requests.fetch_or(crate::abi::REQUEST_NOTIFY, Ordering::Release);
        true
    }

    /// Codes [`notify`](Self::notify) dropped because the queue was full, since the instance
    /// was created.
    pub fn notify_dropped(&self) -> u32 {
        self.0.notifications.dropped()
    }

    /// Sets the handler for [`notify`](Self::notify): called on the host's main thread with
    /// each code, oldest first. Replaces the previous handler.
    pub fn on_notify(&self, handler: impl FnMut(u64, &InstanceHandle) + Send + 'static) {
        if self.is_alive() {
            *lock(&self.0.notify_handler) = Some(Box::new(handler));
            start_notify_fallback();
        }
    }

    /// Main thread (the shim's poll): runs the handler for every waiting code.
    pub(crate) fn deliver_notifications(&self) {
        let _delivering = lock(&self.0.delivering);
        if !self.is_alive() {
            return;
        }
        self.0.apply_program();
        let mut codes = [0u64; crate::notify::CAPACITY];
        let n = self.0.notifications.take(&mut codes);
        if n == 0 {
            return;
        }
        // Taken out while it runs: the handler may call `on_notify` or `notify` itself.
        let handler = lock(&self.0.notify_handler).take();
        if let Some(mut handler) = handler {
            for &code in &codes[..n] {
                // A handler that panics is dropped (not put back), and the rest of the codes are not delivered.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(code, self))).is_err() {
                    return;
                }
            }
            let mut slot = lock(&self.0.notify_handler);
            if slot.is_none() && self.is_alive() {
                *slot = Some(handler);
            }
        }
    }

    /// For editors: delivers a message from the page to the handler.
    #[doc(hidden)]
    pub fn deliver_message(&self, message: &str) {
        // Taken out while it runs: the handler may call `on_message` itself.
        let handler = lock(&self.0.handler).take();
        if let Some(mut handler) = handler {
            // A handler that panics is dropped (not put back).
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(message, self))).is_err() {
                return;
            }
            let mut slot = lock(&self.0.handler);
            if slot.is_none() && self.is_alive() {
                *slot = Some(handler);
            }
        }
    }

    /// For editors: the messages posted since the last call.
    #[doc(hidden)]
    pub fn take_posted(&self) -> Vec<String> {
        lock(&self.0.outbox).drain(..).collect()
    }

    /// For editors: the instance whose shim passed `ctx` as the view host's
    /// context (`ferritron.host.v2` / `v1`). None in hosts and modes that do
    /// not link the editor to its instance (editor in its own process).
    #[doc(hidden)]
    pub fn for_shim_ctx(ctx: *mut c_void) -> Option<Self> {
        lock(&SHIM_CTX).iter().find(|(c, _)| *c == ctx as usize).and_then(|(_, w)| w.upgrade()).map(Self)
    }
}

/// The key-value store of an instance ([`InstanceHandle::store`]): text or
/// bytes by key, saved with the project and restored with it. Loading a
/// project replaces the whole store.
pub struct Store<'a>(&'a Link);

impl Store<'_> {
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        lock(&self.0.store).get(key).cloned()
    }

    /// The value of `key` as text (None if missing or not UTF-8).
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| String::from_utf8(v).ok())
    }

    pub fn set(&self, key: &str, value: impl Into<Vec<u8>>) {
        let value = value.into();
        let mut store = lock(&self.0.store);
        if store.get(key) != Some(&value) {
            store.insert(key.to_string(), value);
            drop(store);
            self.0.changed();
        }
    }

    pub fn remove(&self, key: &str) {
        if lock(&self.0.store).remove(key).is_some() {
            self.0.changed();
        }
    }

    pub fn keys(&self) -> Vec<String> {
        lock(&self.0.store).keys().cloned().collect()
    }
}

/// The live instances of plugin `P` in this process, in creation order:
/// for plugins whose editor controls several instances (a panner for every
/// track, linked compressors). Instances in another process (a host that
/// isolates plugins, `[isolation]`) are not seen.
pub fn instances<P: crate::__private::PluginMeta>() -> Vec<InstanceHandle> {
    instances_of(P::INFO.id)
}

/// The live instances of the plugin with project-file `id`.
pub fn instances_of(plugin_id: &str) -> Vec<InstanceHandle> {
    lock(&REGISTRY)
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|l| l.plugin_id == plugin_id && l.alive.load(Ordering::Acquire))
        .map(InstanceHandle)
        .collect()
}

/// Every live instance (any plugin), in creation order.
pub(crate) fn all_live() -> Vec<InstanceHandle> {
    lock(&REGISTRY).iter().filter_map(Weak::upgrade).filter(|l| l.alive.load(Ordering::Acquire)).map(InstanceHandle).collect()
}

/// Milliseconds since the first call, never 0.
pub(crate) fn clock_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// A shim that has not polled for this long is taken as one that never polls
/// (LV2, the standalone app, AU, WAM): Ferritron's own thread delivers its notifications.
const POLL_GRACE_MS: u64 = 100;

/// Starts (once) the thread that delivers notifications of instances whose shim does not poll
/// `take_requests` on a main thread.
fn start_notify_fallback() {
    #[cfg(not(target_family = "wasm"))]
    {
        static STARTED: std::sync::Once = std::sync::Once::new();
        clock_ms();
        STARTED.call_once(|| {
            // The thread never ends: keep this code loaded, or no thread is started.
            if !crate::module::pin_current_module() {
                return;
            }
            let _ = std::thread::Builder::new().name("ferritron-notify".into()).spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let now = clock_ms();
                    let links: Vec<Arc<Link>> = lock(&REGISTRY).iter().filter_map(Weak::upgrade).collect();
                    for link in links {
                        if !link.alive.load(Ordering::Acquire) || link.polls_on_demand.load(Ordering::Relaxed) || link.polled.load(Ordering::Acquire) || link.main_requests.load(Ordering::Acquire) & crate::abi::REQUEST_NOTIFY == 0 {
                            continue;
                        }
                        let polled = link.last_poll.load(Ordering::Acquire);
                        if now.saturating_sub(polled) > POLL_GRACE_MS {
                            link.main_requests.fetch_and(!crate::abi::REQUEST_NOTIFY, Ordering::AcqRel);
                            InstanceHandle(link).deliver_notifications();
                        }
                    }
                }
            });
        });
    }
}

/// A number that grows whenever an instance (of any plugin) is created or
/// destroyed: cheap to poll.
pub fn instances_revision() -> u64 {
    REGISTRY_REVISION.load(Ordering::Acquire)
}

/// Bytes of the store block in saved state.
pub(crate) const STORE_MAGIC: [u8; 4] = *b"FRXS";

/// Appends the store to saved state: magic, count, then per entry the key
/// and value, each as `u32` length plus bytes.
pub(crate) fn write_store(out: &mut Vec<u8>, store: &BTreeMap<String, Vec<u8>>) {
    if store.is_empty() {
        return;
    }
    out.extend_from_slice(&STORE_MAGIC);
    out.extend_from_slice(&(store.len() as u32).to_le_bytes());
    for (key, value) in store {
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
}

/// Reads a store block written by [`write_store`]; empty if there is none
/// or it is damaged.
pub(crate) fn read_store(data: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut store = BTreeMap::new();
    if data.len() < 8 || data[..4] != STORE_MAGIC {
        return store;
    }
    let mut at = 4;
    let take_u32 = |at: &mut usize| -> Option<u32> {
        let bytes = data.get(*at..*at + 4)?;
        *at += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    let Some(count) = take_u32(&mut at) else { return store };
    for _ in 0..count {
        let Some(len) = take_u32(&mut at) else { return BTreeMap::new() };
        let Some(key) = data.get(at..at + len as usize).and_then(|k| std::str::from_utf8(k).ok()) else { return BTreeMap::new() };
        at += len as usize;
        let Some(len) = take_u32(&mut at) else { return BTreeMap::new() };
        let Some(value) = data.get(at..at + len as usize) else { return BTreeMap::new() };
        at += len as usize;
        store.insert(key.to_string(), value.to_vec());
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_block_roundtrips() {
        let mut store = BTreeMap::new();
        store.insert("name".to_string(), "トラック 1".as_bytes().to_vec());
        store.insert("empty".to_string(), Vec::new());
        let mut data = Vec::new();
        write_store(&mut data, &store);
        assert_eq!(read_store(&data), store);
        // Damaged blocks read as empty rather than half.
        assert!(read_store(&data[..data.len() - 1]).is_empty());
        assert!(read_store(b"nope").is_empty());
    }

    #[test]
    fn ids_are_nonzero_and_differ() {
        let a = InstanceId::random();
        let b = InstanceId::random();
        assert_ne!(a.get(), 0);
        assert_ne!(a, b);
        assert_eq!(a.to_string().len(), 16);
    }
}
