//! Isolation Level 2 (design doc 8): a Ferritron plugin compiled to
//! WebAssembly, run in a wasmtime sandbox inside the host process.
//!
//! The guest is an ordinary Ferritron plugin built for `wasm32-wasip1`: its
//! `ferritron_plugin_entry` table lives in the module's memory and its
//! functions in the module's function table. This crate implements the C
//! ABI for the format adapters on top of it: it calls the guest's functions
//! through the table and copies audio, events and strings in and out of the
//! guest's memory. The guest gets no files, no environment and no network;
//! a crash inside it (a trap) or a runaway loop (stopped after
//! [`TIME_LIMIT_MS`]) only silences that plugin.
//!
//! The plugin binary is a small crate with the format adapters:
//!
//! ```ignore
//! ferritron_wasm::wasm_plugin!(include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm")));
//! ```

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use wasmtime::{Config, Engine, Instance as WasmInstance, Linker, Memory, Module, Store, TypedFunc, Val};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

/// A guest call that runs longer than this is stopped (it traps).
pub const TIME_LIMIT_MS: u64 = 500;
const TICK_MS: u64 = 10;
/// Most channels and events per block passed to the guest.
const MAX_CHANNELS: usize = 8;
const MAX_EVENTS: usize = 1024;
/// Most events the guest may write in one block (the same as the VST3 and CLAP shims).
const MAX_OUT: usize = 256;
const EVENT_SIZE: usize = 24;
// The guest's `ferritron_event` (wasm32) has the host's layout, so its bytes are copied as they are.
const _: () = assert!(size_of::<ferritron_core::abi::Event>() == EVENT_SIZE);

mod c {
    //! The host side of ferritron_abi.h (64-bit layouts).
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Str {
        pub ptr: *const u8,
        pub len: u32,
    }

    #[repr(C)]
    pub struct Info {
        pub strings: [Str; 6],
        pub kind: u32,
        pub class_id: [u8; 16],
        pub isolation: u32,
        /// Added in 1.3.1 (`ferritron_plugin_info`): the shims read them.
        pub vst3_subcategories: Str,
        pub clap_features: Str,
        pub audio_flags: u32,
        pub min_channels: u32,
        pub max_channels: u32,
        pub sidechain_channels: u32,
        pub vst2_id: u32,
    }

    #[repr(C)]
    pub struct ParamInfo {
        pub id: u32,
        pub name: Str,
        pub unit: Str,
        pub step_count: u32,
        pub default_normalized: f64,
        pub flags: u32,
    }

    #[repr(C)]
    pub struct Api {
        pub struct_size: u32,
        pub abi_version: u32,
        pub info: *const Info,
        pub create: unsafe extern "C" fn() -> *mut c_void,
        pub destroy: unsafe extern "C" fn(*mut c_void),
        pub param_count: unsafe extern "C" fn() -> u32,
        pub param_info: unsafe extern "C" fn(u32, *mut ParamInfo) -> bool,
        pub get_param: unsafe extern "C" fn(*mut c_void, u32) -> f64,
        pub set_param: unsafe extern "C" fn(*mut c_void, u32, f64) -> bool,
        pub to_plain: unsafe extern "C" fn(u32, f64) -> f64,
        pub to_normalized: unsafe extern "C" fn(u32, f64) -> f64,
        pub format_param: unsafe extern "C" fn(u32, f64, *mut u8, u32) -> i32,
        pub parse_param: unsafe extern "C" fn(u32, Str, *mut f64) -> bool,
        pub activate: unsafe extern "C" fn(*mut c_void, f64, u32) -> bool,
        /// C ABI v2: one `ferritron_process_data` per call.
        pub process: unsafe extern "C" fn(*mut c_void, *const ferritron_core::abi::ProcessData) -> bool,
        pub save_state: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> u32,
        pub load_state: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> bool,
        pub get_extension: unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void,
        /// The last field of `ferritron_plugin_api`: the VST3 shim rejects a table that is shorter.
        pub reset: unsafe extern "C" fn(*mut c_void),
    }

    #[repr(C)]
    pub struct LatencyApi {
        pub get: unsafe extern "C" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    pub struct RenderApi {
        pub set_offline: unsafe extern "C" fn(*mut c_void, bool),
    }
}

/// instance, address of a `ferritron_process_data` in the guest's memory (C ABI v2).
type ProcessArgs = (u32, u32);

/// The C ABI version guests are built for (`FERRITRON_ABI_VERSION`).
const GUEST_ABI_VERSION: u32 = 2;
/// `ferritron_process_data` with wasm32 pointers: size, frames, num_channels, num_events,
/// channels, events, executor, out_events, out_capacity, out_count (4 bytes each).
const GUEST_PROCESS_DATA_SIZE: usize = 40;

/// The guest's functions, typed (wasm32 C ABI: pointers are u32).
struct Guest {
    /// Address of the guest's `ferritron_plugin_api` table.
    api: u32,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    create: TypedFunc<(), u32>,
    destroy: TypedFunc<u32, ()>,
    reset: TypedFunc<u32, ()>,
    get_param: TypedFunc<(u32, u32), f64>,
    set_param: TypedFunc<(u32, u32, f64), u32>,
    to_plain: TypedFunc<(u32, f64), f64>,
    to_normalized: TypedFunc<(u32, f64), f64>,
    format_param: TypedFunc<(u32, f64, u32, u32), i32>,
    parse_param: wasmtime::Func,
    activate: TypedFunc<(u32, f64, u32), u32>,
    process: TypedFunc<ProcessArgs, u32>,
    save_state: TypedFunc<(u32, u32, u32), u32>,
    load_state: TypedFunc<(u32, u32, u32), u32>,
    latency: Option<TypedFunc<u32, u32>>,
    set_offline: Option<TypedFunc<(u32, u32), ()>>,
    param_count: TypedFunc<(), u32>,
    param_info: TypedFunc<(u32, u32), u32>,
}

/// One sandbox: a wasmtime store with one instance of the module.
struct Sandbox {
    store: Store<WasiP1Ctx>,
    guest: Guest,
    /// Guest memory for blocks and strings, allocated once.
    scratch: u32,
    /// Guest memory for the events the guest writes (`MAX_OUT` of `EVENT_SIZE` bytes, 8-aligned),
    /// followed by the `u32` count it sets. Allocated once, at load time.
    out: u32,
}

const SCRATCH: usize = 64 * 1024;

impl Sandbox {
    fn new(module: &Module, linker: &Linker<WasiP1Ctx>) -> Result<Self, String> {
        let wasi = WasiCtxBuilder::new().build_p1(); // no files, no environment, no network
        let mut store = Store::new(module.engine(), wasi);
        store.set_epoch_deadline(TIME_LIMIT_MS / TICK_MS);
        let instance = linker.instantiate(&mut store, module).map_err(|e| e.to_string())?;
        if let Ok(init) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            init.call(&mut store, ()).map_err(|e| e.to_string())?;
        }
        let guest = Guest::bind(&mut store, &instance)?;
        let scratch = guest.alloc.call(&mut store, SCRATCH as u32).map_err(|e| e.to_string())?;
        // Room for the 8-byte alignment as well.
        let base = guest.alloc.call(&mut store, (MAX_OUT * EVENT_SIZE + 4 + 8) as u32).map_err(|e| e.to_string())?;
        let out = base.next_multiple_of(8);
        Ok(Self { store, guest, scratch, out })
    }

    /// Runs a guest call with a fresh time limit.
    fn call<R>(&mut self, f: impl FnOnce(&mut Store<WasiP1Ctx>, &Guest) -> wasmtime::Result<R>) -> Option<R> {
        self.store.set_epoch_deadline(TIME_LIMIT_MS / TICK_MS);
        f(&mut self.store, &self.guest).ok()
    }

    fn read(&self, at: u32, len: usize) -> Vec<u8> {
        let data = self.guest.memory.data(&self.store);
        data.get(at as usize..at as usize + len).map(<[u8]>::to_vec).unwrap_or_default()
    }

    fn write(&mut self, at: u32, bytes: &[u8]) -> bool {
        let data = self.guest.memory.data_mut(&mut self.store);
        match data.get_mut(at as usize..at as usize + bytes.len()) {
            Some(dst) => {
                dst.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn u32_at(&self, at: u32) -> u32 {
        let b = self.read(at, 4);
        if b.len() == 4 { u32::from_le_bytes(b.try_into().unwrap()) } else { 0 }
    }

    /// A guest `ferritron_str` (8 bytes in wasm32) as a host string.
    fn string_at(&self, at: u32) -> String {
        let (ptr, len) = (self.u32_at(at), self.u32_at(at + 4));
        String::from_utf8_lossy(&self.read(ptr, len as usize)).into_owned()
    }
}

impl Guest {
    fn bind(store: &mut Store<WasiP1Ctx>, instance: &WasmInstance) -> Result<Self, String> {
        let memory = instance.get_memory(&mut *store, "memory").ok_or("the guest exports no memory")?;
        let table = instance.get_table(&mut *store, "__indirect_function_table").ok_or("the guest exports no function table (link with --export-table)")?;
        let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "ferritron_wasm_alloc").map_err(|e| e.to_string())?;
        let entry = instance.get_typed_func::<(), u32>(&mut *store, "ferritron_plugin_entry").map_err(|e| e.to_string())?;
        let api = entry.call(&mut *store, ()).map_err(|e| e.to_string())?;
        let read_u32 = |store: &Store<WasiP1Ctx>, at: u32| {
            let d = memory.data(store);
            d.get(at as usize..at as usize + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        if read_u32(store, api + 4) != GUEST_ABI_VERSION {
            return Err("the guest was built for another Ferritron ABI version".into());
        }
        // Function pointers in wasm32 are indices into the function table.
        let func = |store: &mut Store<WasiP1Ctx>, offset: u32| -> Result<wasmtime::Func, String> {
            let index = read_u32(store, api + offset);
            match table.get(&mut *store, index as u64) {
                Some(wasmtime::Ref::Func(Some(f))) => Ok(f),
                _ => Err(format!("the guest's function table has no function {index}")),
            }
        };
        macro_rules! typed {
            ($offset:expr) => {
                func(store, $offset)?.typed(&*store).map_err(|e| e.to_string())?
            };
        }
        let get_extension: TypedFunc<u32, u32> = typed!(68);
        // Extensions: ids written into the guest's memory, tables read back.
        let extension = |store: &mut Store<WasiP1Ctx>, id: &[u8]| -> Option<wasmtime::Func> {
            let at = alloc.call(&mut *store, id.len() as u32 + 1).ok()?;
            let data = memory.data_mut(&mut *store);
            data.get_mut(at as usize..at as usize + id.len() + 1)?.copy_from_slice(&[id, &[0]].concat());
            let table_at = get_extension.call(&mut *store, at).ok()?;
            if table_at == 0 {
                return None;
            }
            let index = read_u32(store, table_at);
            match table.get(&mut *store, index as u64) {
                Some(wasmtime::Ref::Func(Some(f))) => Some(f),
                _ => None,
            }
        };
        let latency = extension(store, b"ferritron.latency.v1").and_then(|f| f.typed(&*store).ok());
        let set_offline = extension(store, b"ferritron.render.v1").and_then(|f| f.typed(&*store).ok());
        Ok(Self {
            api,
            memory,
            alloc,
            create: typed!(12),
            destroy: typed!(16),
            reset: typed!(72),
            param_count: typed!(20),
            param_info: typed!(24),
            get_param: typed!(28),
            set_param: typed!(32),
            to_plain: typed!(36),
            to_normalized: typed!(40),
            format_param: typed!(44),
            parse_param: func(store, 48)?,
            activate: typed!(52),
            process: typed!(56),
            save_state: typed!(60),
            load_state: typed!(64),
            latency,
            set_offline,
        })
    }
}

/// The module, compiled once, and what the host keeps of its metadata.
struct Plugin {
    module: Module,
    linker: Linker<WasiP1Ctx>,
    /// For calls without an instance (to_plain, format_param, ...).
    meta: Mutex<Sandbox>,
    /// Boxed, and kept only for `api.info`, which points to it (the plugin
    /// moves into a static after loading).
    _info: Box<c::Info>,
    params: Vec<c::ParamInfo>,
    /// (parameter ID, position in `params`), sorted by ID: found by binary search on the audio thread.
    index: Vec<(u32, usize)>,
    api: c::Api,
}

// SAFETY: the raw pointers point to leaked, immutable strings.
unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

static PLUGIN: OnceLock<Option<Plugin>> = OnceLock::new();
static ENGINE: OnceLock<Engine> = OnceLock::new();

fn leak(text: String) -> c::Str {
    let text: &'static str = Box::leak(text.into_boxed_str());
    c::Str { ptr: text.as_ptr(), len: text.len() as u32 }
}

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        // Interruptible guests. (Built without wasmtime's parallel-compilation
        // feature: no compiler threads that could outlive the plugin library.)
        // Guest code runs on the host's threads, whose stacks may be small.
        config.epoch_interruption(true).max_wasm_stack(128 * 1024);
        Engine::new(&config).expect("valid wasmtime configuration")
    })
}

/// The plugin table for the guest module in `wasm`, compiled on first use.
/// None if the module cannot be loaded (the reason goes to
/// `%TEMP%/ferritron-wasm.log`).
pub fn plugin(wasm: &[u8]) -> Option<&'static PluginApi> {
    let plugin = PLUGIN.get_or_init(|| match load(wasm) {
        Ok(p) => Some(p),
        Err(message) => {
            log(&message);
            None
        }
    });
    // SAFETY: `c::Api` has the layout of `ferritron_plugin_api`.
    plugin.as_ref().map(|p| unsafe { &*(&p.api as *const c::Api as *const PluginApi) })
}

fn log(message: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("ferritron-wasm.log")) {
        let _ = writeln!(file, "{message}");
    }
}

/// Where the compiled form of `wasm` is kept: compiling takes about 200 ms
/// each time a host loads the plugin, reading it back a few. The name covers
/// the module and everything the compiled code depends on (wasmtime's
/// version and settings, the CPU's features), so a stale file is never used.
/// `FERRITRON_WASM_NO_CACHE` turns the cache off.
fn cache_path(wasm: &[u8]) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    if std::env::var_os("FERRITRON_WASM_NO_CACHE").is_some() {
        return None;
    }
    let base = if cfg!(windows) {
        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from).or_else(|| Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cache")))?
    };
    let mut hasher = std::hash::DefaultHasher::new();
    wasm.hash(&mut hasher);
    engine().precompile_compatibility_hash().hash(&mut hasher);
    Some(base.join("Ferritron").join("wasm").join(format!("{:016x}.cwasm", hasher.finish())))
}

/// The guest module, from the cache when possible.
fn compile(wasm: &[u8]) -> Result<Module, String> {
    let cached = cache_path(wasm);
    // SAFETY: the file was written by `Module::serialize` below for this
    // engine; wasmtime checks that it fits before using it. The folder is
    // the user's own, like the plugin binaries themselves.
    if let Some(module) = cached.as_ref().filter(|p| p.exists()).and_then(|p| unsafe { Module::deserialize_file(engine(), p) }.ok()) {
        return Ok(module);
    }
    // Compiling needs more stack than hosts give their threads.
    let bytes = wasm.to_vec();
    let module = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || Module::new(engine(), &bytes).map_err(|e| format!("invalid guest module: {e:?}")))
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "compiling the guest module failed".to_string())??;
    if let (Some(path), Ok(data)) = (cached, module.serialize()) {
        // Written aside and renamed: another host loading the plugin at the
        // same moment never reads half a file.
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let written = path.parent().is_some_and(|dir| std::fs::create_dir_all(dir).is_ok()) && std::fs::write(&temp, data).is_ok();
        if !written || std::fs::rename(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
    Ok(module)
}

fn load(wasm: &[u8]) -> Result<Plugin, String> {
    let module = compile(wasm)?;
    let mut linker = Linker::new(engine());
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx| ctx).map_err(|e| e.to_string())?;
    let mut meta = Sandbox::new(&module, &linker)?;
    let api = meta.guest.api;
    // Plugin info (wasm32 layout: 6 strings of 8 bytes, kind, class id, isolation).
    let info_at = meta.u32_at(api + 8);
    let mut info = Box::new(c::Info {
        strings: [c::Str { ptr: std::ptr::null(), len: 0 }; 6],
        kind: meta.u32_at(info_at + 48),
        class_id: meta.read(info_at + 52, 16).try_into().map_err(|_| "cannot read the class id")?,
        isolation: 0,
        // wasm32 layout after `isolation` (68): two strings at 72 and 80, then five u32 from 88.
        vst3_subcategories: leak(meta.string_at(info_at + 72)),
        clap_features: leak(meta.string_at(info_at + 80)),
        // The sandbox offers no sidechain extension; MIDI input and the other flags pass on.
        audio_flags: meta.u32_at(info_at + 88) & !ferritron_core::abi::AUDIO_SIDECHAIN,
        min_channels: meta.u32_at(info_at + 92).clamp(1, MAX_CHANNELS as u32),
        max_channels: meta.u32_at(info_at + 96).clamp(1, MAX_CHANNELS as u32),
        sidechain_channels: 0,
        vst2_id: meta.u32_at(info_at + 104),
    });
    for (i, s) in info.strings.iter_mut().enumerate() {
        *s = leak(meta.string_at(info_at + 8 * i as u32));
    }
    // Parameters (wasm32 layout: id, name, unit, steps, default at 24, flags at 32).
    let count = meta.call(|s, g| g.param_count.call(s, ())).ok_or("param_count trapped")?;
    let out = meta.scratch;
    let mut params = Vec::new();
    for index in 0..count {
        meta.call(|s, g| g.param_info.call(s, (index, out))).ok_or("param_info trapped")?;
        let default = f64::from_le_bytes(meta.read(out + 24, 8).try_into().map_err(|_| "cannot read a parameter")?);
        params.push(c::ParamInfo {
            id: meta.u32_at(out),
            name: leak(meta.string_at(out + 4)),
            unit: leak(meta.string_at(out + 12)),
            step_count: meta.u32_at(out + 20),
            default_normalized: default,
            flags: meta.u32_at(out + 32),
        });
    }
    let info_ptr: *const c::Info = &*info;
    let mut index: Vec<(u32, usize)> = params.iter().enumerate().map(|(k, p)| (p.id, k)).collect();
    index.sort_unstable();
    Ok(Plugin {
        module,
        linker,
        meta: Mutex::new(meta),
        _info: info,
        params,
        index,
        api: c::Api {
            struct_size: size_of::<c::Api>() as u32,
            abi_version: ferritron_core::abi::ABI_VERSION,
            info: info_ptr,
            create: host::create,
            destroy: host::destroy,
            param_count: host::param_count,
            param_info: host::param_info,
            get_param: host::get_param,
            set_param: host::set_param,
            to_plain: host::to_plain,
            to_normalized: host::to_normalized,
            format_param: host::format_param,
            parse_param: host::parse_param,
            activate: host::activate,
            process: host::process,
            save_state: host::save_state,
            load_state: host::load_state,
            get_extension: host::get_extension,
            reset: host::reset,
        },
    })
}

/// Sets flush-to-zero and denormals-are-zero while it lives (x86).
struct DenormalGuard {
    #[cfg(target_arch = "x86_64")]
    saved: u32,
}

impl DenormalGuard {
    #[allow(deprecated)] // _mm_getcsr/_mm_setcsr: no replacement on stable yet
    fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
            let saved = unsafe { _mm_getcsr() };
            unsafe { _mm_setcsr(saved | 0x8040) };
            Self { saved }
        }
        #[cfg(not(target_arch = "x86_64"))]
        Self {}
    }
}

impl Drop for DenormalGuard {
    #[allow(deprecated)]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_setcsr(self.saved)
        };
    }
}

/// Ticks the engine's epoch while any instance exists, so guest calls can
/// be timed out; stopped with the last instance (no thread outlives the
/// plugin library).
static TICKER: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>> = Mutex::new(None);
static INSTANCES: AtomicUsize = AtomicUsize::new(0);

fn ticker_start() {
    if INSTANCES.fetch_add(1, Ordering::SeqCst) > 0 {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(TICK_MS));
            engine().increment_epoch();
        }
    });
    *TICKER.lock().unwrap_or_else(PoisonError::into_inner) = Some((stop, thread));
}

fn ticker_stop() {
    if INSTANCES.fetch_sub(1, Ordering::SeqCst) != 1 {
        return;
    }
    if let Some((stop, thread)) = TICKER.lock().unwrap_or_else(PoisonError::into_inner).take() {
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }
}

/// What calls into the guest need: its sandbox and the guest's state. Behind the instance's
/// mutex, so the guest runs on one thread at a time.
struct Live {
    sandbox: Sandbox,
    core: u32,
    failed: bool,
    latency: u32,
}

/// A plugin instance: its own sandbox, so instances cannot touch each other.
///
/// The guest is called under `live`. Parameter values are kept beside it: `set_param` and
/// `get_param` (any thread, e.g. the editor) only touch `values`, and an edit reaches the guest
/// at the start of the next block (`dirty`). So a host thread never calls into the guest while
/// the audio thread does (D1).
struct Instance {
    live: std::sync::Mutex<Live>,
    /// Normalized values in the order of `Plugin::params`, as `f64` bits.
    values: Box<[AtomicU64]>,
    /// Set when `values[i]` was changed and has not reached the guest yet.
    dirty: Box<[AtomicBool]>,
}

mod host {
    //! The C ABI functions the format adapters call.
    use super::*;

    fn plugin() -> &'static Plugin {
        PLUGIN.get().and_then(Option::as_ref).expect("table handed out only after loading")
    }

    unsafe fn instance<'a>(p: *mut c_void) -> &'a Instance {
        unsafe { &*(p as *const Instance) }
    }

    /// The guest, locked (host threads other than the audio thread wait for it).
    fn lock_live(i: &Instance) -> std::sync::MutexGuard<'_, Live> {
        i.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The position of parameter `id` in `Plugin::params` (and in `Instance::values`).
    fn index_of(id: u32) -> Option<usize> {
        let index = &plugin().index;
        index.binary_search_by_key(&id, |&(id, _)| id).ok().map(|at| index[at].1)
    }

    /// Copies every parameter's value from the guest into `values`. A value the guest does not
    /// give back is left as it is. The caller holds the guest.
    fn refresh(i: &Instance, live: &mut Live) {
        let core = live.core;
        for (k, param) in plugin().params.iter().enumerate() {
            if let Some(value) = live.sandbox.call(|s, g| g.get_param.call(s, (core, param.id))) {
                i.values[k].store(value.to_bits(), Ordering::Release);
            }
        }
    }

    /// Sends the edits still waiting in `values` (`dirty`) to the guest. The caller holds the guest.
    fn flush_dirty(i: &Instance, live: &mut Live) {
        let core = live.core;
        for (k, param) in plugin().params.iter().enumerate() {
            if i.dirty[k].swap(false, Ordering::AcqRel) {
                let value = f64::from_bits(i.values[k].load(Ordering::Acquire));
                live.sandbox.call(|s, g| g.set_param.call(s, (core, param.id, value)));
            }
        }
    }

    /// Writes the parameter changes among the block's events (`raw`, `EVENT_SIZE` bytes each)
    /// into `values`. With `pending`, they are also marked dirty: they reach the guest at the
    /// next block (used when this block is silent and the guest does not get them now).
    fn record_params(i: &Instance, raw: &[u8], pending: bool) {
        for event in raw.chunks_exact(EVENT_SIZE) {
            let kind = u32::from_ne_bytes(event[4..8].try_into().unwrap());
            if kind != ferritron_core::abi::EVENT_PARAM {
                continue;
            }
            let id = u32::from_ne_bytes(event[8..12].try_into().unwrap());
            let value = f64::from_ne_bytes(event[16..24].try_into().unwrap());
            if let Some(k) = index_of(id) {
                i.values[k].store(value.to_bits(), Ordering::Release);
                if pending {
                    i.dirty[k].store(true, Ordering::Release);
                }
            }
        }
    }

    /// Hands the events the guest wrote in this block to the host. `memory` is the guest's memory,
    /// `out` the start of the output area (`Sandbox::out`). Up to the host's room, the events go
    /// into `d.out_events` and `*d.out_count` gets their number. Parameter changes also reach
    /// `values`, whether or not the host has room, except where an edit is still waiting for the
    /// guest (`dirty`): that edit is the newer one. Reads and copies only: no allocation.
    fn pass_out_events(i: &Instance, memory: &[u8], out: u32, d: &ferritron_core::abi::ProcessData) {
        // Fields beyond the caller's `size` are not there: read `out_capacity` and `out_count` only when they are.
        let full = d.size as usize >= size_of::<ferritron_core::abi::ProcessData>();
        let host_count = if full { d.out_count } else { std::ptr::null_mut() };
        let room = if full && !host_count.is_null() && !d.out_events.is_null() { d.out_capacity as usize } else { 0 };
        let word = |at: usize| memory.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        let count = (word(out as usize + MAX_OUT * EVENT_SIZE) as usize).min(MAX_OUT);
        let mut copied = 0usize;
        for k in 0..count {
            let at = out as usize + k * EVENT_SIZE;
            let Some(event) = memory.get(at..at + EVENT_SIZE) else { break };
            if u32::from_le_bytes(event[4..8].try_into().unwrap()) == ferritron_core::abi::EVENT_PARAM {
                let id = u32::from_le_bytes(event[8..12].try_into().unwrap());
                let normalized = f64::from_le_bytes(event[16..24].try_into().unwrap());
                if let Some(p) = index_of(id).filter(|&p| !i.dirty[p].load(Ordering::Acquire)) {
                    i.values[p].store(normalized.to_bits(), Ordering::Release);
                }
            }
            if copied < room {
                // SAFETY: `copied < room` is within the host's capacity; the layouts match (see the assert above).
                unsafe { std::ptr::copy_nonoverlapping(event.as_ptr(), (d.out_events as *mut u8).add(copied * EVENT_SIZE), EVENT_SIZE) };
                copied += 1;
            }
        }
        if !host_count.is_null() {
            // SAFETY: non-null, and within the caller's struct (`full`).
            unsafe { *host_count = copied as u32 };
        }
    }

    pub unsafe extern "C" fn create() -> *mut c_void {
        let p = plugin();
        let Ok(mut sandbox) = Sandbox::new(&p.module, &p.linker) else { return std::ptr::null_mut() };
        let Some(core) = sandbox.call(|s, g| g.create.call(s, ())).filter(|&c| c != 0) else { return std::ptr::null_mut() };
        let count = p.params.len();
        let instance = Instance {
            live: std::sync::Mutex::new(Live { sandbox, core, failed: false, latency: 0 }),
            values: (0..count).map(|_| AtomicU64::new(0)).collect(),
            dirty: (0..count).map(|_| AtomicBool::new(false)).collect(),
        };
        refresh(&instance, &mut lock_live(&instance));
        ticker_start();
        Box::into_raw(Box::new(instance)) as *mut c_void
    }

    pub unsafe extern "C" fn destroy(p: *mut c_void) {
        let instance = unsafe { Box::from_raw(p as *mut Instance) };
        {
            let mut live = lock_live(&instance);
            let core = live.core;
            live.sandbox.call(|s, g| g.destroy.call(s, core));
        }
        drop(instance);
        ticker_stop();
    }

    pub unsafe extern "C" fn param_count() -> u32 {
        plugin().params.len() as u32
    }

    pub unsafe extern "C" fn param_info(index: u32, out: *mut c::ParamInfo) -> bool {
        let Some(info) = plugin().params.get(index as usize) else { return false };
        if out.is_null() {
            return false;
        }
        unsafe {
            *out = c::ParamInfo { id: info.id, name: info.name, unit: info.unit, step_count: info.step_count, default_normalized: info.default_normalized, flags: info.flags }
        };
        true
    }

    /// Reads the host's copy: the guest is not called (it may be busy in the audio thread).
    pub unsafe extern "C" fn get_param(p: *mut c_void, id: u32) -> f64 {
        let i = unsafe { instance(p) };
        index_of(id).map_or(0.0, |k| f64::from_bits(i.values[k].load(Ordering::Acquire)))
    }

    /// Stores the value and marks it; the guest gets it before the next block (its answer is not
    /// checked). Returns false for a parameter the plugin does not have.
    pub unsafe extern "C" fn set_param(p: *mut c_void, id: u32, value: f64) -> bool {
        let i = unsafe { instance(p) };
        let Some(k) = index_of(id) else { return false };
        i.values[k].store(value.to_bits(), Ordering::Release);
        i.dirty[k].store(true, Ordering::Release);
        true
    }

    fn meta() -> std::sync::MutexGuard<'static, Sandbox> {
        plugin().meta.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub unsafe extern "C" fn to_plain(id: u32, value: f64) -> f64 {
        meta().call(|s, g| g.to_plain.call(s, (id, value))).unwrap_or(0.0)
    }

    pub unsafe extern "C" fn to_normalized(id: u32, value: f64) -> f64 {
        meta().call(|s, g| g.to_normalized.call(s, (id, value))).unwrap_or(0.0)
    }

    pub unsafe extern "C" fn format_param(id: u32, value: f64, buf: *mut u8, cap: u32) -> i32 {
        if buf.is_null() {
            return -1;
        }
        let mut m = meta();
        let (at, cap) = (m.scratch, cap.min(SCRATCH as u32));
        let Some(len) = m.call(|s, g| g.format_param.call(s, (id, value, at, cap))).filter(|&n| n >= 0) else { return -1 };
        let text = m.read(at, len as usize);
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len()) };
        text.len() as i32
    }

    pub unsafe extern "C" fn parse_param(id: u32, text: c::Str, out: *mut f64) -> bool {
        if text.ptr.is_null() || out.is_null() || text.len as usize > SCRATCH / 2 {
            return false;
        }
        let bytes = unsafe { std::slice::from_raw_parts(text.ptr, text.len as usize) }.to_vec();
        let mut m = meta();
        let base = m.scratch;
        let (string, header, result) = (base + 64, base, base + 16);
        if !m.write(string, &bytes) || !m.write(header, &[string.to_le_bytes(), (bytes.len() as u32).to_le_bytes()].concat()) {
            return false;
        }
        // wasm32's C ABI passes the string either by pointer or as two values.
        let ok = m.call(|s, g| {
            let params: Vec<Val> = match g.parse_param.ty(&*s).params().len() {
                3 => vec![Val::I32(id as i32), Val::I32(header as i32), Val::I32(result as i32)],
                _ => vec![Val::I32(id as i32), Val::I32(string as i32), Val::I32(bytes.len() as i32), Val::I32(result as i32)],
            };
            let mut ret = [Val::I32(0)];
            g.parse_param.call(&mut *s, &params, &mut ret)?;
            Ok(ret[0].unwrap_i32() != 0)
        });
        if ok != Some(true) {
            return false;
        }
        let value = m.read(result, 8);
        unsafe { *out = f64::from_le_bytes(value.try_into().unwrap_or([0; 8])) };
        true
    }

    pub unsafe extern "C" fn reset(p: *mut c_void) {
        let i = unsafe { instance(p) };
        let mut live = lock_live(i);
        let core = live.core;
        if live.sandbox.call(|s, g| g.reset.call(s, core)).is_none() {
            live.failed = true;
        }
    }

    pub unsafe extern "C" fn activate(p: *mut c_void, rate: f64, max_block: u32) -> bool {
        let i = unsafe { instance(p) };
        let mut live = lock_live(i);
        flush_dirty(i, &mut live);
        let core = live.core;
        let ok = live.sandbox.call(|s, g| g.activate.call(s, (core, rate, max_block))).is_some_and(|ok| ok != 0);
        let latency = live.sandbox.guest.latency.clone();
        let latency = match latency {
            Some(f) => live.sandbox.call(|s, _| f.call(s, core)).unwrap_or(0),
            None => 0,
        };
        live.latency = latency;
        live.failed = !ok;
        refresh(i, &mut live);
        ok
    }

    pub unsafe extern "C" fn process(p: *mut c_void, data: *const ferritron_core::abi::ProcessData) -> bool {
        let Some(d) = (unsafe { data.as_ref() }).filter(|d| d.size as usize >= std::mem::offset_of!(ferritron_core::abi::ProcessData, out_events))
        else {
            return false;
        };
        // The count stays 0 unless the guest's events are handed over below.
        if d.size as usize >= size_of::<ferritron_core::abi::ProcessData>() && !d.out_count.is_null() {
            unsafe { *d.out_count = 0 };
        }
        let (channels, num_channels, frames) = (d.channels, d.num_channels, d.frames);
        let (events, num_events) = (d.events as *const u8, d.num_events);
        if num_channels > 0 && channels.is_null() || num_events > 0 && events.is_null() {
            return false;
        }
        let i = unsafe { instance(p) };
        let (n_ch, n_ev) = (num_channels as usize, (num_events as usize).min(MAX_EVENTS));
        // The block's events as the guest gets them (`n_ev` of `EVENT_SIZE` bytes each).
        let raw: &[u8] = if n_ev > 0 { unsafe { std::slice::from_raw_parts(events, n_ev * EVENT_SIZE) } } else { &[] };
        let zero_channels = || {
            for ch in 0..n_ch {
                unsafe { std::slice::from_raw_parts_mut(*channels.add(ch), frames as usize) }.fill(0.0);
            }
        };
        // Another thread (an edit, a saved state) holds the guest: this block is silent, and the
        // plugin is not failed for it. Its parameter changes are kept for the next block.
        let Ok(mut guard) = i.live.try_lock() else {
            zero_channels();
            record_params(i, raw, true);
            return false;
        };
        let live: &mut Live = &mut guard;
        let core = live.core;
        // Edits made since the last block reach the guest first.
        flush_dirty(i, live);
        // Layout in the guest's scratch memory: the process data, channel pointers, events, audio.
        let data_at = live.sandbox.scratch;
        let pointers = data_at + GUEST_PROCESS_DATA_SIZE as u32;
        let events_at = (pointers + (MAX_CHANNELS * 4) as u32).next_multiple_of(8);
        let audio = events_at + (MAX_EVENTS * EVENT_SIZE) as u32;
        let needed = (audio - data_at) as usize + n_ch * frames as usize * 4;
        if live.failed || n_ch > MAX_CHANNELS || needed > SCRATCH {
            zero_channels();
            live.failed = true;
            return false;
        }
        let out_at = live.sandbox.out;
        let out_count_at = out_at + (MAX_OUT * EVENT_SIZE) as u32;
        {
            let memory = live.sandbox.guest.memory;
            let data = memory.data_mut(&mut live.sandbox.store);
            let mut put = |at: usize, bytes: &[u8]| data[at..at + bytes.len()].copy_from_slice(bytes);
            // Nothing written yet: the guest sets the count when it returns.
            put(out_count_at as usize, &0u32.to_le_bytes());
            for ch in 0..n_ch {
                let at = audio as usize + ch * frames as usize * 4;
                put(pointers as usize + ch * 4, &(at as u32).to_le_bytes());
                let samples = unsafe { std::slice::from_raw_parts(*channels.add(ch), frames as usize) };
                #[cfg(target_endian = "little")]
                {
                    // SAFETY: f32 has no padding; the bytes of `samples` are `frames * 4` initialized bytes.
                    let bytes = unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 4) };
                    put(at, bytes);
                }
                #[cfg(not(target_endian = "little"))]
                for (k, s) in samples.iter().enumerate() {
                    put(at + k * 4, &s.to_le_bytes());
                }
            }
            if n_ev > 0 {
                put(events_at as usize, raw);
            }
            // The guest's room for its own events: `out_events`, `out_capacity`, `out_count`.
            let fields = [GUEST_PROCESS_DATA_SIZE as u32, frames, num_channels, n_ev as u32, pointers, events_at, 0, out_at, MAX_OUT as u32, out_count_at];
            for (k, v) in fields.iter().enumerate() {
                put(data_at as usize + 4 * k, &v.to_le_bytes());
            }
        }
        // The guest gets these changes in this block: the host's copy takes them now.
        record_params(i, raw, false);
        let ok = {
            // WebAssembly cannot set the FPU flags itself: flush denormals here.
            let _denormals = DenormalGuard::new();
            live.sandbox.call(|s, g| g.process.call(s, (core, data_at)))
        };
        match ok {
            Some(ok) => {
                let memory = live.sandbox.guest.memory;
                let data = memory.data(&live.sandbox.store);
                for ch in 0..n_ch {
                    let at = audio as usize + ch * frames as usize * 4;
                    let out = unsafe { std::slice::from_raw_parts_mut(*channels.add(ch), frames as usize) };
                    #[cfg(target_endian = "little")]
                    {
                        let bytes = &data[at..at + out.len() * 4]; // panics like the old loop when out of range
                        // SAFETY: `bytes` holds `out.len() * 4` bytes; `out` is `out.len()` f32s; they do not overlap
                        // (guest memory and the host's buffer).
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr() as *mut u8, bytes.len()) };
                    }
                    #[cfg(not(target_endian = "little"))]
                    for (k, s) in out.iter_mut().enumerate() {
                        *s = f32::from_le_bytes(data[at + k * 4..at + k * 4 + 4].try_into().unwrap());
                    }
                }
                // The events are handed over whatever the guest returned: they were written before it returned.
                pass_out_events(i, data, out_at, d);
                ok != 0
            }
            // A trap or the time limit: this plugin goes silent, the host lives on.
            None => {
                zero_channels();
                live.failed = true;
                false
            }
        }
    }

    /// The state is at most the scratch area (`SCRATCH`). A larger one is not saved: this returns
    /// 0, which the shims save as an empty state, and the reason goes to stderr.
    pub unsafe extern "C" fn save_state(p: *mut c_void, buf: *mut u8, cap: u32) -> u32 {
        let i = unsafe { instance(p) };
        let mut live = lock_live(i);
        // An edit that has not reached the guest yet belongs in the state.
        flush_dirty(i, &mut live);
        let (core, at) = (live.core, live.sandbox.scratch);
        let size = live.sandbox.call(|s, g| g.save_state.call(s, (core, at, SCRATCH as u32))).unwrap_or(0);
        if size as usize > SCRATCH {
            eprintln!("ferritron-wasm: the state is {size} bytes, more than the {SCRATCH} the guest can hand over; it is not saved");
            return 0;
        }
        if !buf.is_null() && size <= cap {
            let bytes = live.sandbox.read(at, size as usize);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
        }
        size
    }

    pub unsafe extern "C" fn load_state(p: *mut c_void, data: *const u8, len: u32) -> bool {
        let i = unsafe { instance(p) };
        if data.is_null() || len as usize > SCRATCH {
            return false;
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
        let mut live = lock_live(i);
        // Edits waiting for the next block are older than the state: they must not overwrite it.
        for dirty in i.dirty.iter() {
            dirty.store(false, Ordering::Release);
        }
        let (core, at) = (live.core, live.sandbox.scratch);
        let ok = live.sandbox.write(at, bytes) && live.sandbox.call(|s, g| g.load_state.call(s, (core, at, len))).is_some_and(|ok| ok != 0);
        refresh(i, &mut live);
        ok
    }

    unsafe extern "C" fn latency(p: *mut c_void) -> u32 {
        lock_live(unsafe { instance(p) }).latency
    }

    unsafe extern "C" fn set_offline(p: *mut c_void, offline: bool) {
        let i = unsafe { instance(p) };
        let mut live = lock_live(i);
        let core = live.core;
        if let Some(f) = live.sandbox.guest.set_offline.clone() {
            live.sandbox.call(|s, _| f.call(s, (core, offline as u32)));
        }
    }

    static LATENCY: c::LatencyApi = c::LatencyApi { get: latency };
    static RENDER: c::RenderApi = c::RenderApi { set_offline };

    pub unsafe extern "C" fn get_extension(id: *const c_char) -> *const c_void {
        if id.is_null() {
            return std::ptr::null();
        }
        match unsafe { std::ffi::CStr::from_ptr(id) }.to_bytes() {
            b"ferritron.latency.v1" => &LATENCY as *const c::LatencyApi as *const c_void,
            b"ferritron.render.v1" => &RENDER as *const c::RenderApi as *const c_void,
            _ => std::ptr::null(),
        }
    }
}

/// Defines `ferritron_plugin_entry` as the WebAssembly guest in `$wasm`
/// (bytes, e.g. `include_bytes!`).
#[macro_export]
macro_rules! wasm_plugin {
    ($wasm:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn ferritron_plugin_entry() -> *const $crate::PluginApi {
            static WASM: &[u8] = $wasm;
            $crate::plugin(WASM).map_or(::core::ptr::null(), |api| api as *const _)
        }
    };
}

pub use ferritron_core::abi::PluginApi;
