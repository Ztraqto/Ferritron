//! The hot-reload proxy: an ABI function table that forwards every call to
//! the latest loaded copy of the plugin library.
//!
//! Concurrency:
//! - `[main]` calls and reloads serialize on `Global::state`.
//! - `[any]`/`[audio]` calls never lock. They register in `READERS` while
//!   they use a library, and a reload swaps pointers first and then waits
//!   until `READERS` drops to zero before freeing anything old.

use super::MARKER_SUFFIX;
use crate::module::{Library, current_module_path};
use crate::abi::{
    ABI_VERSION, EXT_LATENCY, EXT_RENDER, EXT_VIEW, Event, Executor, FerritronStr, LatencyApi, ParamInfoC, PluginApi, RenderApi, ViewApi,
    ViewHost,
};
use std::ffi::c_void;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_millis(250);

/// One loaded plugin library. `library` is `None` for the code built into the
/// bundle itself, used when the watched library cannot be loaded.
struct Loaded {
    api: &'static PluginApi,
    library: Option<Library>,
}

/// One plugin instance living in a loaded library.
struct Slot {
    loaded: Arc<Loaded>,
    core: *mut c_void,
}

impl Drop for Slot {
    fn drop(&mut self) {
        unsafe { (self.loaded.api.destroy)(self.core) };
    }
}

/// What the host sees as one instance; its slot is replaced on reload.
struct Proxy {
    slot: AtomicPtr<Slot>,
    /// Last `activate` arguments, replayed on new instances.
    setup: Mutex<Option<(f64, u32)>>,
    /// Offline rendering, as last told by the shim; new code inherits it.
    offline: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { len: meta.len(), modified: meta.modified().ok()? })
}

struct Watcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    latest: Option<Arc<Loaded>>,
    /// Kept for one more generation: strings returned by `param_info` point
    /// into it and may still be read by the shim.
    previous: Option<Arc<Loaded>>,
    stamp: Option<Stamp>,
    proxies: Vec<Weak<Proxy>>,
    watcher: Option<Watcher>,
    generation: u32,
}

struct Global {
    /// The plugin built into the bundle binary the host loaded.
    original: &'static PluginApi,
    /// The library cargo writes, e.g. `target/debug/gain.dll`.
    library: PathBuf,
    temp_dir: PathBuf,
    log: PathBuf,
    state: Mutex<State>,
}

static GLOBAL: OnceLock<Option<Global>> = OnceLock::new();
static PROXY_API: OnceLock<PluginApi> = OnceLock::new();
/// API of the latest library, for the ABI functions without an instance.
static LATEST_API: AtomicPtr<PluginApi> = AtomicPtr::new(std::ptr::null_mut());
static READERS: AtomicUsize = AtomicUsize::new(0);

struct Reading;

impl Reading {
    fn enter() -> Self {
        READERS.fetch_add(1, SeqCst);
        Reading
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        READERS.fetch_sub(1, SeqCst);
    }
}

fn wait_for_readers() {
    while READERS.load(SeqCst) != 0 {
        std::thread::yield_now();
    }
}

/// Activates the proxy if this module is part of a dev bundle.
pub fn install(original: &'static PluginApi) -> Option<&'static PluginApi> {
    GLOBAL.get_or_init(|| discover(original)).as_ref()?;
    // Own the shared Runtime here, in the library the host loaded, so that
    // temporary copies loaded for reloading never have to stay pinned.
    crate::parallel::register();
    Some(PROXY_API.get_or_init(|| table(original)))
}

fn discover(original: &'static PluginApi) -> Option<Global> {
    let module = current_module_path()?;
    let mut marker = module.into_os_string();
    marker.push(MARKER_SUFFIX);
    let library = PathBuf::from(std::fs::read_to_string(marker).ok()?.trim());
    let temp_dir = std::env::temp_dir().join("ferritron-hot-reload");
    std::fs::create_dir_all(&temp_dir).ok()?;
    let stem = library.file_stem()?.to_string_lossy().into_owned();
    let global = Global { original, log: temp_dir.join(format!("{stem}.log")), library, temp_dir, state: Mutex::default() };
    global.log(&format!("hot reload enabled, watching {}", global.library.display()));
    Some(global)
}

fn global() -> &'static Global {
    GLOBAL.get().and_then(Option::as_ref).expect("proxy used before install")
}

impl Global {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, message: &str) {
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = writeln!(file, "[{secs}] {message}");
        }
    }

    /// Loads a fresh copy of the watched library and checks it is this plugin.
    fn load(&self, generation: u32) -> Result<Arc<Loaded>, LoadError> {
        let stem = self.library.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = self.library.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let copy = self.temp_dir.join(format!("{stem}-{}-{generation}.{ext}", std::process::id()));
        let library = Library::load_copy(&self.library, copy).map_err(LoadError::Transient)?;
        let entry = library.symbol(c"ferritron_plugin_entry");
        if entry.is_null() {
            return Err(LoadError::Invalid("library does not export ferritron_plugin_entry".into()));
        }
        let entry: unsafe extern "C" fn() -> *const PluginApi = unsafe { std::mem::transmute(entry) };
        let api = unsafe { entry().as_ref() }.ok_or_else(|| LoadError::Invalid("plugin entry returned null".into()))?;
        // SAFETY: the table stays valid while `library` is loaded, and `Loaded` owns both.
        let api: &'static PluginApi = unsafe { &*(api as *const PluginApi) };
        if api.abi_version != ABI_VERSION || (api.struct_size as usize) < size_of::<PluginApi>() {
            return Err(LoadError::Invalid("library was built with an incompatible Ferritron version".into()));
        }
        let class_id = |a: &PluginApi| unsafe { (*a.info).class_id };
        if class_id(api) != class_id(self.original) {
            return Err(LoadError::Invalid("library is a different plugin (plugin.id changed?)".into()));
        }
        Ok(Arc::new(Loaded { api, library: Some(library) }))
    }
}

enum LoadError {
    /// May succeed on the next attempt (e.g. the linker still holds the file).
    Transient(String),
    Invalid(String),
}

impl LoadError {
    fn message(&self) -> &str {
        match self {
            LoadError::Transient(m) | LoadError::Invalid(m) => m,
        }
    }
}

fn latest_api() -> &'static PluginApi {
    let api = LATEST_API.load(SeqCst);
    if api.is_null() { global().original } else { unsafe { &*api } }
}

fn param_ids(api: &PluginApi) -> Vec<u32> {
    let mut info = ParamInfoC {
        id: 0,
        name: FerritronStr::new(""),
        unit: FerritronStr::new(""),
        step_count: 0,
        default_normalized: 0.0,
        flags: 0,
    };
    (0..unsafe { (api.param_count)() })
        .filter_map(|i| unsafe { (api.param_info)(i, &mut info) }.then_some(info.id))
        .collect()
}

/// Creates an instance in `to` that continues where `old` is.
fn migrate(old: &Slot, to: &Arc<Loaded>, setup: Option<(f64, u32)>, offline: bool) -> Option<*mut c_void> {
    let (from, api) = (old.loaded.api, to.api);
    unsafe {
        let core = (api.create)();
        if core.is_null() {
            return None;
        }
        let size = (from.save_state)(old.core, std::ptr::null_mut(), 0);
        let mut state = vec![0u8; size as usize];
        (from.save_state)(old.core, state.as_mut_ptr(), size);
        (api.load_state)(core, state.as_ptr(), size);
        if let Some(render) = ((api.get_extension)(EXT_RENDER.as_ptr()) as *const RenderApi).as_ref() {
            (render.set_offline)(core, offline);
        }
        if let Some((sample_rate, max_block)) = setup {
            (api.activate)(core, sample_rate, max_block);
        }
        // Automation may have moved parameters since the state was saved.
        let old_ids = param_ids(from);
        for id in param_ids(api).into_iter().filter(|id| old_ids.contains(id)) {
            (api.set_param)(core, id, (from.get_param)(old.core, id));
        }
        Some(core)
    }
}

fn reload(g: &Global, now: Stamp) {
    let mut st = g.lock();
    let loaded = match g.load(st.generation) {
        Ok(loaded) => loaded,
        Err(err) => {
            if matches!(err, LoadError::Invalid(_)) {
                st.stamp = Some(now); // wait for the next build instead of retrying
            }
            g.log(&format!("reload skipped: {}", err.message()));
            return;
        }
    };
    st.generation += 1;

    let proxies: Vec<Arc<Proxy>> = st.proxies.iter().filter_map(Weak::upgrade).collect();
    let mut retired = Vec::new();
    for proxy in &proxies {
        let old = unsafe { &*proxy.slot.load(SeqCst) };
        let setup = *proxy.setup.lock().unwrap_or_else(PoisonError::into_inner);
        match migrate(old, &loaded, setup, proxy.offline.load(SeqCst)) {
            Some(core) => {
                let slot = Box::into_raw(Box::new(Slot { loaded: loaded.clone(), core }));
                retired.push(proxy.slot.swap(slot, SeqCst));
            }
            None => g.log("an instance could not be created in the new library; it keeps running the old code"),
        }
    }
    let params_changed = param_ids(latest_api()) != param_ids(loaded.api);
    LATEST_API.store(loaded.api as *const PluginApi as *mut PluginApi, SeqCst);
    let replaced = st.latest.replace(loaded);
    let expired = std::mem::replace(&mut st.previous, replaced);
    st.stamp = Some(now);
    let generation = st.generation;
    wait_for_readers();
    drop(st);

    // Nothing can reach the old instances any more.
    for slot in retired {
        drop(unsafe { Box::from_raw(slot) });
    }
    drop(expired);
    g.log(&format!("reloaded (generation {generation}, {} instance(s))", proxies.len()));
    if params_changed {
        g.log("the parameter list changed: re-insert the plugin in the host to see the new parameters");
    }
}

fn watch(stop: Arc<AtomicBool>) {
    let g = global();
    let mut previous_poll = None;
    while !stop.load(SeqCst) {
        std::thread::sleep(POLL);
        let Some(now) = stamp(&g.library) else { continue };
        if Some(now) == g.lock().stamp {
            continue;
        }
        // Only reload once the file stopped changing: the linker may still be writing.
        if previous_poll != Some(now) {
            previous_poll = Some(now);
            continue;
        }
        reload(g, now);
    }
}

fn table(original: &'static PluginApi) -> PluginApi {
    PluginApi {
        struct_size: size_of::<PluginApi>() as u32,
        abi_version: ABI_VERSION,
        info: original.info,
        create,
        destroy,
        param_count,
        param_info,
        get_param,
        set_param,
        to_plain,
        to_normalized,
        format_param,
        parse_param,
        activate,
        process,
        save_state,
        load_state,
        get_extension,
        reset,
    }
}

/// A view opened from one loaded library, which stays loaded until the view closes.
/// Editors show the code that was current when they were opened.
struct ProxyView {
    loaded: Arc<Loaded>,
    api: &'static ViewApi,
    view: *mut c_void,
}

fn view_of(loaded: &Loaded) -> Option<&'static ViewApi> {
    unsafe { ((loaded.api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi).as_ref() }
}

static PROXY_VIEW: OnceLock<ViewApi> = OnceLock::new();

static PROXY_LATENCY: LatencyApi = LatencyApi { get: proxy_latency };
static PROXY_RENDER: RenderApi = RenderApi { set_offline: proxy_set_offline };

unsafe extern "C" fn proxy_set_offline(ptr: *mut c_void, offline: bool) {
    let proxy = unsafe { &*(ptr as *const Proxy) };
    proxy.offline.store(offline, SeqCst);
    unsafe {
        with_slot(ptr, |s| {
            if let Some(render) = ((s.loaded.api.get_extension)(EXT_RENDER.as_ptr()) as *const RenderApi).as_ref() {
                (render.set_offline)(s.core, offline);
            }
        })
    }
}

/// The latency of the code an instance currently runs.
unsafe extern "C" fn proxy_latency(ptr: *mut c_void) -> u32 {
    unsafe {
        with_slot(ptr, |s| match ((s.loaded.api.get_extension)(EXT_LATENCY.as_ptr()) as *const LatencyApi).as_ref() {
            Some(latency) => (latency.get)(s.core),
            None => 0,
        })
    }
}

unsafe extern "C" fn get_extension(id: *const std::ffi::c_char) -> *const c_void {
    if !id.is_null() && unsafe { std::ffi::CStr::from_ptr(id) } == EXT_LATENCY {
        return &PROXY_LATENCY as *const LatencyApi as *const c_void;
    }
    if !id.is_null() && unsafe { std::ffi::CStr::from_ptr(id) } == EXT_RENDER {
        return &PROXY_RENDER as *const RenderApi as *const c_void;
    }
    if id.is_null() || unsafe { std::ffi::CStr::from_ptr(id) } != EXT_VIEW {
        return std::ptr::null();
    }
    let _reading = Reading::enter();
    let Some(view) = (unsafe { ((latest_api().get_extension)(id) as *const ViewApi).as_ref() }) else {
        return std::ptr::null();
    };
    let proxy = PROXY_VIEW.get_or_init(|| ViewApi {
        width: view.width,
        height: view.height,
        min_width: view.min_width,
        min_height: view.min_height,
        max_width: view.max_width,
        max_height: view.max_height,
        open: view_open,
        close: view_close,
        set_size: view_set_size,
    });
    proxy as *const ViewApi as *const c_void
}

unsafe extern "C" fn view_open(parent: *mut c_void, host: *const ViewHost, width: u32, height: u32, scale: f64) -> *mut c_void {
    let st = global().lock();
    let Some(loaded) = st.latest.clone() else { return std::ptr::null_mut() };
    drop(st);
    let Some(api) = view_of(&loaded) else { return std::ptr::null_mut() };
    let view = unsafe { (api.open)(parent, host, width, height, scale) };
    if view.is_null() {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(ProxyView { loaded, api, view })).cast()
}

unsafe extern "C" fn view_set_size(ptr: *mut c_void, width: u32, height: u32, scale: f64) {
    if let Some(proxy) = unsafe { (ptr as *mut ProxyView).as_ref() } {
        unsafe { (proxy.api.set_size)(proxy.view, width, height, scale) };
    }
}

unsafe extern "C" fn view_close(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let proxy = unsafe { Box::from_raw(ptr as *mut ProxyView) };
    unsafe { (proxy.api.close)(proxy.view) };
    drop(proxy.loaded);
}

/// Runs `f` on the instance's current slot without locking.
unsafe fn with_slot<R>(ptr: *mut c_void, f: impl FnOnce(&Slot) -> R) -> R {
    let _reading = Reading::enter();
    let proxy = unsafe { &*(ptr as *const Proxy) };
    f(unsafe { &*proxy.slot.load(SeqCst) })
}

unsafe extern "C" fn create() -> *mut c_void {
    let g = global();
    let mut st = g.lock();
    if st.latest.is_none() {
        let generation = st.generation;
        st.generation += 1;
        st.stamp = stamp(&g.library);
        let loaded = g.load(generation).unwrap_or_else(|err| {
            g.log(&format!("cannot load {} ({}); using the code built into the bundle until the next build", g.library.display(), err.message()));
            Arc::new(Loaded { api: g.original, library: None })
        });
        if loaded.library.is_some() {
            g.log("loaded plugin library");
        }
        LATEST_API.store(loaded.api as *const PluginApi as *mut PluginApi, SeqCst);
        st.latest = Some(loaded);
    }
    let loaded = st.latest.clone().unwrap();
    let core = unsafe { (loaded.api.create)() };
    if core.is_null() {
        return std::ptr::null_mut();
    }
    let slot = Box::into_raw(Box::new(Slot { loaded, core }));
    let proxy = Arc::new(Proxy { slot: AtomicPtr::new(slot), setup: Mutex::new(None), offline: AtomicBool::new(false) });
    st.proxies.push(Arc::downgrade(&proxy));
    if st.watcher.is_none() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        match std::thread::Builder::new().name("ferritron-hot-reload".into()).spawn(move || watch(flag)) {
            Ok(thread) => st.watcher = Some(Watcher { stop, thread }),
            Err(err) => g.log(&format!("cannot start the file watcher: {err}")),
        }
    }
    Arc::into_raw(proxy) as *mut c_void
}

unsafe extern "C" fn destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let g = global();
    let proxy = unsafe { Arc::from_raw(ptr as *const Proxy) };
    let mut st = g.lock();
    let slot = proxy.slot.swap(std::ptr::null_mut(), SeqCst);
    st.proxies.retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(&proxy)));
    let last = st.proxies.is_empty();
    let (latest, previous, watcher) = if last {
        // Unload everything; the next instance loads the library again.
        LATEST_API.store(std::ptr::null_mut(), SeqCst);
        (st.latest.take(), st.previous.take(), st.watcher.take())
    } else {
        (None, None, None)
    };
    wait_for_readers();
    drop(unsafe { Box::from_raw(slot) });
    drop((latest, previous));
    drop(st);
    if let Some(watcher) = watcher {
        watcher.stop.store(true, SeqCst);
        let _ = watcher.thread.join();
    }
}

unsafe extern "C" fn param_count() -> u32 {
    let _reading = Reading::enter();
    unsafe { (latest_api().param_count)() }
}

unsafe extern "C" fn param_info(index: u32, out: *mut ParamInfoC) -> bool {
    let _reading = Reading::enter();
    unsafe { (latest_api().param_info)(index, out) }
}

unsafe extern "C" fn get_param(ptr: *mut c_void, id: u32) -> f64 {
    unsafe { with_slot(ptr, |s| (s.loaded.api.get_param)(s.core, id)) }
}

unsafe extern "C" fn set_param(ptr: *mut c_void, id: u32, normalized: f64) -> bool {
    unsafe { with_slot(ptr, |s| (s.loaded.api.set_param)(s.core, id, normalized)) }
}

unsafe extern "C" fn to_plain(id: u32, normalized: f64) -> f64 {
    let _reading = Reading::enter();
    unsafe { (latest_api().to_plain)(id, normalized) }
}

unsafe extern "C" fn to_normalized(id: u32, plain: f64) -> f64 {
    let _reading = Reading::enter();
    unsafe { (latest_api().to_normalized)(id, plain) }
}

unsafe extern "C" fn format_param(id: u32, normalized: f64, buf: *mut u8, cap: u32) -> i32 {
    let _reading = Reading::enter();
    unsafe { (latest_api().format_param)(id, normalized, buf, cap) }
}

unsafe extern "C" fn parse_param(id: u32, text: FerritronStr, out: *mut f64) -> bool {
    let _reading = Reading::enter();
    unsafe { (latest_api().parse_param)(id, text, out) }
}

unsafe extern "C" fn activate(ptr: *mut c_void, sample_rate: f64, max_block_size: u32) -> bool {
    let _st = global().lock();
    let proxy = unsafe { &*(ptr as *const Proxy) };
    *proxy.setup.lock().unwrap_or_else(PoisonError::into_inner) = Some((sample_rate, max_block_size));
    unsafe { with_slot(ptr, |s| (s.loaded.api.activate)(s.core, sample_rate, max_block_size)) }
}

/// Forwards `reset` to the current library. Libraries built before 2.0.1 have no `reset` in their
/// table (smaller `struct_size`): nothing to clear there.
unsafe extern "C" fn reset(ptr: *mut c_void) {
    unsafe {
        with_slot(ptr, |s| {
            let api = s.loaded.api;
            if api.struct_size as usize >= std::mem::offset_of!(PluginApi, reset) + size_of::<usize>() {
                (api.reset)(s.core);
            }
        })
    }
}

unsafe extern "C" fn process(ptr: *mut c_void, data: *const crate::abi::ProcessData) -> bool {
    unsafe { with_slot(ptr, |s| (s.loaded.api.process)(s.core, data)) }
}

unsafe extern "C" fn save_state(ptr: *mut c_void, buf: *mut u8, cap: u32) -> u32 {
    let _st = global().lock();
    unsafe { with_slot(ptr, |s| (s.loaded.api.save_state)(s.core, buf, cap)) }
}

unsafe extern "C" fn load_state(ptr: *mut c_void, data: *const u8, len: u32) -> bool {
    let _st = global().lock();
    unsafe { with_slot(ptr, |s| (s.loaded.api.load_state)(s.core, data, len)) }
}
