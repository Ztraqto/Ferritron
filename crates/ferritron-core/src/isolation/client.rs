//! The host side: an ABI table whose instances forward processing to a
//! child process, which is restarted when it dies.
//!
//! Instances of one plugin type share one child process (a "group") unless
//! the plugin asks for a process per instance. The child serves each
//! instance on its own thread, through that instance's shared memory.

use super::os::{Memory, Process, Signal};
use super::shm::*;
use crate::abi::{EVENT_PARAM, Event, Executor, FerritronStr, LatencyApi, ParamInfoC, PluginApi};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::path::Path;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static ORIGINAL: OnceLock<&'static PluginApi> = OnceLock::new();
static TABLE: OnceLock<PluginApi> = OnceLock::new();
static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
/// The group shared by instances of this plugin type, while it has any.
static SHARED_GROUP: Mutex<Option<Arc<Group>>> = Mutex::new(None);
/// Creating and ending groups happens one at a time.
static MEMBERSHIP: Mutex<()> = Mutex::new(());

fn original() -> &'static PluginApi {
    ORIGINAL.get().expect("isolation client used before install")
}

/// `[isolation]` options; `FERRITRON_ISOLATION_SHARE=plugin|instance` and
/// `FERRITRON_ISOLATION_PIPELINE=0|1` override them.
fn options() -> u32 {
    let mut options = unsafe { (*original().info).isolation };
    let mut set = |flag: u32, on: bool| options = if on { options | flag } else { options & !flag };
    match std::env::var("FERRITRON_ISOLATION_SHARE").as_deref() {
        Ok("instance") => set(super::PER_INSTANCE, true),
        Ok("plugin") => set(super::PER_INSTANCE, false),
        _ => {}
    }
    match std::env::var("FERRITRON_ISOLATION_PIPELINE").as_deref() {
        Ok("1") => set(super::PIPELINE, true),
        Ok("0") => set(super::PIPELINE, false),
        _ => {}
    }
    options
}

pub fn install(api: &'static PluginApi) -> Option<&'static PluginApi> {
    let library = current_module_path()?;
    let _ = LIBRARY.set(library);
    let _ = ORIGINAL.set(api);
    Some(TABLE.get_or_init(|| PluginApi {
        struct_size: size_of::<PluginApi>() as u32,
        abi_version: api.abi_version,
        info: api.info,
        create,
        destroy,
        // Parameter metadata needs no instance: answer it in this process.
        param_count: api.param_count,
        param_info: api.param_info,
        get_param,
        set_param,
        to_plain: api.to_plain,
        to_normalized: api.to_normalized,
        format_param: api.format_param,
        parse_param: api.parse_param,
        activate,
        process,
        save_state,
        load_state,
        get_extension,
    }))
}

pub(super) fn log(message: &str) {
    let path = std::env::temp_dir().join("ferritron-isolation.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = writeln!(file, "[{secs}] {message}");
    }
}

pub(super) fn current_module_path() -> Option<PathBuf> {
    crate::module::current_module_path()
}

// --- Groups: one child process serving several instances ------------------

struct Group {
    id: String,
    memory: Memory,
    command: Signal,
    ready: Signal,
    child: Mutex<Option<Arc<Process>>>,
    members: Mutex<Vec<Weak<Client>>>,
    stop: AtomicBool,
    wake: Signal,
    watcher: Mutex<Option<JoinHandle<()>>>,
    /// Starting the child and adding instances to it happen one at a time.
    starting: Mutex<()>,
}

// SAFETY: the control region and handles are used under the protocol in shm.rs.
unsafe impl Send for Group {}
unsafe impl Sync for Group {}

impl Group {
    fn new(id: String) -> Option<Arc<Self>> {
        let (memory, command, ready) = group_names(&id);
        let (Some(memory), Some(command), Some(ready), Some(wake)) =
            (Memory::create(&memory, size_of::<Control>()), Signal::create(&command), Signal::create(&ready), Signal::anonymous())
        else {
            log(&format!("group {id}: cannot set up isolation"));
            return None;
        };
        let group = Arc::new(Self {
            id,
            memory,
            command,
            ready,
            child: Mutex::new(None),
            members: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            wake,
            watcher: Mutex::new(None),
            starting: Mutex::new(()),
        });
        if !group.spawn() {
            log(&format!("group {}: starting without a plugin process; retrying", group.id));
        }
        let watcher = group.clone();
        *group.watcher.lock().unwrap_or_else(PoisonError::into_inner) =
            std::thread::Builder::new().name("ferritron-isolation".into()).spawn(move || watcher.watch()).ok();
        Some(group)
    }

    fn child(&self) -> Option<Arc<Process>> {
        self.child.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn control(&self) -> &Control {
        unsafe { &*(self.memory.ptr() as *const Control) }
    }

    fn live_members(&self) -> Vec<Arc<Client>> {
        let mut members = self.members.lock().unwrap_or_else(PoisonError::into_inner);
        members.retain(|m| m.strong_count() > 0);
        members.iter().filter_map(Weak::upgrade).collect()
    }

    /// Starts the child process and waits until it takes commands.
    fn spawn(&self) -> bool {
        let library = LIBRARY.get().map_or(Path::new(""), PathBuf::as_path);
        let control = self.control();
        control.ready.store(0, SeqCst);
        control.quit.store(0, SeqCst);
        control.head.store(0, SeqCst);
        control.tail.store(0, SeqCst);
        let child = match Process::spawn(library, &format!("group {} {}", self.id, Process::current_id())) {
            Ok(child) => child,
            Err(e) => {
                log(&format!("group {}: cannot start the plugin process: {e}", self.id));
                return false;
            }
        };
        if !self.ready.wait(Some(10_000), Some(&child)) || control.ready.load(SeqCst) == 0 {
            log(&format!("group {}: the plugin process did not start", self.id));
            child.kill();
            return false;
        }
        *self.child.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(child));
        for member in self.live_members() {
            self.serve(&member);
        }
        true
    }

    /// Asks the child to serve `client` and waits until it does.
    fn serve(&self, client: &Client) -> bool {
        let Some(child) = self.child() else {
            return false; // the watcher adds it once the child runs
        };
        let control = self.control();
        let head = control.head.load(SeqCst);
        if head.wrapping_sub(control.tail.load(SeqCst)) as usize >= MAX_COMMANDS {
            return false;
        }
        let slot = &control.commands[head as usize % MAX_COMMANDS];
        let mut command = [0u8; COMMAND_LEN];
        let bytes = &client.id.as_bytes()[..client.id.len().min(COMMAND_LEN - 1)];
        command[..bytes.len()].copy_from_slice(bytes);
        // SAFETY: the child reads this slot only after `head` moves past it.
        unsafe { *slot.get() = command };
        client.shared().ready.store(0, SeqCst);
        client.shared().quit.store(0, SeqCst);
        control.head.store(head.wrapping_add(1), SeqCst);
        self.command.set();
        let ready = client.ready.wait(Some(10_000), Some(&child)) && client.shared().ready.load(SeqCst) != 0;
        if ready {
            client.alive.store(true, SeqCst);
            client.reactivate();
        } else {
            log(&format!("{}: the plugin process did not take the instance", client.id));
        }
        ready
    }

    fn add(&self, client: &Arc<Client>) {
        let _starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
        self.members.lock().unwrap_or_else(PoisonError::into_inner).push(Arc::downgrade(client));
        self.serve(client);
    }

    /// Restarts the child whenever it exits, until the group ends.
    fn watch(self: Arc<Self>) {
        let mut backoff = Duration::from_millis(200);
        while !self.stop.load(SeqCst) {
            match self.child() {
                None => std::thread::sleep(backoff),
                Some(child) => {
                    if !child.wait_exit(None, Some(&self.wake)) {
                        continue; // woken to stop
                    }
                    // The child died: its instances are silent until a new one serves them.
                    for member in self.live_members() {
                        member.alive.store(false, SeqCst);
                    }
                    *self.child.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    log(&format!("group {}: the plugin process ended unexpectedly; restarting", self.id));
                    std::thread::sleep(backoff);
                }
            }
            if self.stop.load(SeqCst) {
                break;
            }
            let started_at = Instant::now();
            let _starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
            if self.spawn() {
                log(&format!("group {}: plugin process restarted", self.id));
                // A child that dies right away again backs off further.
                backoff = if started_at.elapsed() < Duration::from_secs(2) { (backoff * 2).min(Duration::from_secs(5)) } else { Duration::from_millis(200) };
            } else {
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }

    /// Stops the watcher and the child; the group has no instances left.
    fn end(&self) {
        self.stop.store(true, SeqCst);
        self.wake.set();
        if let Some(watcher) = self.watcher.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = watcher.join();
        }
        let child = self.child.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(child) = child {
            self.control().quit.store(1, SeqCst);
            self.command.set();
            if !child.wait_exit(Some(2000), None) {
                child.kill();
            }
        }
    }

    /// The child process, to stop it when a block hangs.
    fn kill(&self) {
        if let Some(child) = self.child() {
            child.kill();
        }
    }
}

// --- Instances ------------------------------------------------------------

/// Pipeline mode: blocks are processed one block late, in parallel with the
/// host. Outputs go through a FIFO with a fixed delay of `latency` samples,
/// so the delay holds for any block size.
struct Pipeline {
    latency: usize,
    fifo: Vec<VecDeque<f32>>,
    /// The block the child is processing: (frames, channels).
    in_flight: Option<(usize, usize)>,
}

impl Pipeline {
    fn new(latency: usize) -> Self {
        let fifo = (0..MAX_CHANNELS)
            .map(|_| {
                let mut q = VecDeque::with_capacity(2 * latency + 1);
                q.extend(std::iter::repeat_n(0.0, latency));
                q
            })
            .collect();
        Self { latency, fifo, in_flight: None }
    }
}

struct Client {
    id: String,
    shared: *mut Shared,
    _memory: Memory,
    request: Signal,
    done: Signal,
    ready: Signal,
    alive: AtomicBool,
    group: Arc<Group>,
    /// Never processes audio: serializes state and holds parameter defaults.
    shadow: *mut c_void,
    ids: Vec<u32>,
    /// Touched by `activate` (main thread) and `process` (audio thread),
    /// which hosts never run at the same time.
    pipeline: UnsafeCell<Option<Pipeline>>,
}

// SAFETY: the shared region and handles are used under the protocol in shm.rs.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

impl Client {
    fn shared(&self) -> &Shared {
        unsafe { &*self.shared }
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.ids.iter().position(|&i| i == id)
    }

    /// Applies the block's parameter changes to the mirror (after the block,
    /// so the child sees the old values until each change's offset).
    fn mirror_events(&self, events: &[Event]) {
        for event in events.iter().filter(|e| e.kind == EVENT_PARAM) {
            let param = unsafe { event.data.param };
            if let Some(i) = self.index(param.id) {
                self.shared().params[i].store(param.normalized.to_bits(), SeqCst);
            }
        }
    }

    /// Activates the child's instance now (it reports its latency), if this
    /// instance was activated before.
    fn reactivate(&self) -> bool {
        let shared = self.shared();
        if shared.max_block.load(SeqCst) == 0 || !self.alive.load(SeqCst) {
            return false;
        }
        unsafe {
            (*self.shared).frames = 0;
            (*self.shared).num_events = 0;
        }
        self.request.set();
        self.done.wait(Some(2000), None)
    }

    fn fail(&self) {
        // Hung or died mid-block: stop the process; the watcher starts a new one.
        self.alive.store(false, SeqCst);
        self.group.kill();
    }

    /// Runs one block in the child and waits for it.
    unsafe fn run_block(&self, channels: &[*mut f32], frames: usize, events: &[Event]) -> bool {
        unsafe {
            self.send_block(channels, frames, events);
            let finished = self.done.wait(Some(PROCESS_TIMEOUT_MS), None);
            self.mirror_events(events);
            if !finished {
                self.fail();
                return false;
            }
            for (ch, &dst) in channels.iter().enumerate() {
                std::ptr::copy_nonoverlapping((*self.shared).audio[ch].as_ptr(), dst, frames);
            }
            (*self.shared).ok != 0
        }
    }

    unsafe fn send_block(&self, channels: &[*mut f32], frames: usize, events: &[Event]) {
        let shared = self.shared;
        unsafe {
            for (ch, &src) in channels.iter().enumerate() {
                std::ptr::copy_nonoverlapping(src, (*shared).audio[ch].as_mut_ptr(), frames);
            }
            let count = events.len().min(MAX_EVENTS);
            std::ptr::copy_nonoverlapping(events.as_ptr(), (*shared).events.as_mut_ptr(), count);
            (*shared).frames = frames as u32;
            (*shared).channels = channels.len() as u32;
            (*shared).num_events = count as u32;
            (*shared).ok = 0;
        }
        self.request.set();
    }

    /// Pipeline mode: collects the previous block, starts this one, and
    /// outputs the samples that are `latency` old.
    unsafe fn run_pipelined(&self, pipeline: &mut Pipeline, channels: &[*mut f32], frames: usize, events: &[Event]) -> bool {
        if let Some((done_frames, done_channels)) = pipeline.in_flight.take() {
            let finished = self.done.wait(Some(PROCESS_TIMEOUT_MS), None);
            let ok = finished && unsafe { (*self.shared).ok } != 0;
            if !finished {
                self.fail();
            }
            for (ch, fifo) in pipeline.fifo.iter_mut().enumerate() {
                if ok && ch < done_channels {
                    fifo.extend(&self.shared().audio[ch][..done_frames]);
                } else {
                    fifo.extend(std::iter::repeat_n(0.0, done_frames));
                }
            }
        }
        // After a failure, fill in silence so the delay stays the same.
        for fifo in &mut pipeline.fifo {
            let missing = frames.saturating_sub(fifo.len());
            fifo.extend(std::iter::repeat_n(0.0, missing));
        }
        if self.alive.load(SeqCst) && frames <= pipeline.latency {
            unsafe { self.send_block(channels, frames, events) };
            pipeline.in_flight = Some((frames, channels.len()));
        }
        self.mirror_events(events);
        for (ch, &dst) in channels.iter().enumerate() {
            let fifo = &mut pipeline.fifo[ch];
            for i in 0..frames {
                unsafe { *dst.add(i) = fifo.pop_front().unwrap_or(0.0) };
            }
        }
        true
    }
}

impl ParamInfoC {
    pub(crate) fn empty() -> Self {
        Self { id: 0, name: FerritronStr::new(""), unit: FerritronStr::new(""), step_count: 0, default_normalized: 0.0, flags: 0 }
    }
}

unsafe fn client<'a>(ptr: *mut c_void) -> &'a Client {
    unsafe { &*(ptr as *const Client) }
}

/// The group a new instance joins: the plugin type's shared one, or a new
/// one of its own.
fn group_for(id: &str) -> Option<Arc<Group>> {
    if options() & super::PER_INSTANCE != 0 {
        return Group::new(id.to_string());
    }
    let mut shared = SHARED_GROUP.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(group) = shared.as_ref() {
        return Some(group.clone());
    }
    let group = Group::new(format!("{}-type", Process::current_id()))?;
    *shared = Some(group.clone());
    Some(group)
}

unsafe extern "C" fn create() -> *mut c_void {
    let _membership = MEMBERSHIP.lock().unwrap_or_else(PoisonError::into_inner);
    let api = original();
    let id = format!("{}-{}", Process::current_id(), NEXT_ID.fetch_add(1, SeqCst));
    let (memory, request, done, ready) = names(&id);
    let (Some(memory), Some(request), Some(done), Some(ready)) =
        (Memory::create(&memory, size_of::<Shared>()), Signal::create(&request), Signal::create(&done), Signal::create(&ready))
    else {
        log(&format!("{id}: cannot set up isolation"));
        return std::ptr::null_mut();
    };
    unsafe {
        let shadow = (api.create)();
        if shadow.is_null() {
            log(&format!("{id}: cannot set up isolation"));
            return std::ptr::null_mut();
        }
        let Some(group) = group_for(&id) else {
            (api.destroy)(shadow);
            return std::ptr::null_mut();
        };
        let count = ((api.param_count)() as usize).min(MAX_PARAMS);
        let shared = memory.ptr() as *mut Shared;
        let ids: Vec<u32> = (0..count as u32)
            .map(|i| {
                let mut info = ParamInfoC::empty();
                (api.param_info)(i, &mut info);
                (*shared).params[i as usize].store((api.get_param)(shadow, info.id).to_bits(), SeqCst);
                info.id
            })
            .collect();
        let client = Arc::new(Client {
            id,
            shared,
            _memory: memory,
            request,
            done,
            ready,
            alive: AtomicBool::new(false),
            group: group.clone(),
            shadow,
            ids,
            pipeline: UnsafeCell::new(None),
        });
        group.add(&client);
        Arc::into_raw(client) as *mut c_void
    }
}

unsafe extern "C" fn destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let _membership = MEMBERSHIP.lock().unwrap_or_else(PoisonError::into_inner);
    let client = unsafe { Arc::from_raw(ptr as *const Client) };
    let group = client.group.clone();
    // The child ends this instance's thread and destroys its plugin.
    if client.alive.swap(false, SeqCst) {
        client.shared().quit.store(1, SeqCst);
        client.request.set();
        let child = group.child();
        client.done.wait(Some(2000), child.as_deref());
    }
    let last = {
        let _starting = group.starting.lock().unwrap_or_else(PoisonError::into_inner);
        let mut members = group.members.lock().unwrap_or_else(PoisonError::into_inner);
        members.retain(|m| m.strong_count() > 0 && !std::ptr::eq(m.as_ptr(), Arc::as_ptr(&client)));
        members.is_empty()
    };
    unsafe { (original().destroy)(client.shadow) };
    drop(client);
    if last {
        let mut shared = SHARED_GROUP.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.as_ref().is_some_and(|g| Arc::ptr_eq(g, &group)) {
            *shared = None;
        }
        drop(shared);
        group.end();
    }
}

unsafe extern "C" fn get_param(ptr: *mut c_void, id: u32) -> f64 {
    let client = unsafe { client(ptr) };
    client.index(id).map_or(0.0, |i| f64::from_bits(client.shared().params[i].load(SeqCst)))
}

unsafe extern "C" fn set_param(ptr: *mut c_void, id: u32, normalized: f64) -> bool {
    let client = unsafe { client(ptr) };
    let Some(i) = client.index(id) else { return false };
    // Store what the plugin would store (rounded for discrete parameters).
    let plain = unsafe { (original().to_plain)(id, normalized) };
    let value = unsafe { (original().to_normalized)(id, plain) };
    client.shared().params[i].store(value.to_bits(), SeqCst);
    true
}

unsafe extern "C" fn activate(ptr: *mut c_void, sample_rate: f64, max_block_size: u32) -> bool {
    let client = unsafe { client(ptr) };
    let shared = client.shared();
    // A block still in flight finishes before the new setup.
    if let Some(pipeline) = unsafe { &mut *client.pipeline.get() }
        && pipeline.in_flight.take().is_some()
    {
        client.done.wait(Some(PROCESS_TIMEOUT_MS), None);
    }
    shared.sample_rate.store(sample_rate.to_bits(), SeqCst);
    shared.max_block.store(max_block_size, SeqCst);
    shared.setup_generation.fetch_add(1, SeqCst);
    let pipelined = options() & super::PIPELINE != 0;
    unsafe { *client.pipeline.get() = pipelined.then(|| Pipeline::new((max_block_size as usize).min(MAX_FRAMES))) };
    // Activate the child now, so its latency is known when the host asks.
    client.reactivate();
    true
}

unsafe extern "C" fn process(
    ptr: *mut c_void,
    channels: *const *mut f32,
    num_channels: u32,
    frames: u32,
    events: *const Event,
    num_events: u32,
    _executor: *const Executor,
) -> bool {
    let client = unsafe { client(ptr) };
    let events: &[Event] = if events.is_null() || num_events == 0 { &[] } else { unsafe { std::slice::from_raw_parts(events, num_events as usize) } };
    let (n, frames) = (num_channels as usize, frames as usize);
    if n > MAX_CHANNELS || frames > MAX_FRAMES || (n > 0 && channels.is_null()) {
        client.mirror_events(events);
        return false; // the shim outputs silence
    }
    let channels = if n == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(channels, n) } };
    if let Some(pipeline) = unsafe { &mut *client.pipeline.get() } {
        return unsafe { client.run_pipelined(pipeline, channels, frames, events) };
    }
    if !client.alive.load(SeqCst) {
        client.mirror_events(events);
        return false;
    }
    unsafe { client.run_block(channels, frames, events) }
}

/// Copies the mirrored parameters into the shadow instance.
fn sync_shadow(client: &Client) {
    for (i, &id) in client.ids.iter().enumerate() {
        unsafe { (original().set_param)(client.shadow, id, f64::from_bits(client.shared().params[i].load(SeqCst))) };
    }
}

unsafe extern "C" fn save_state(ptr: *mut c_void, buf: *mut u8, cap: u32) -> u32 {
    let client = unsafe { client(ptr) };
    sync_shadow(client);
    unsafe { (original().save_state)(client.shadow, buf, cap) }
}

unsafe extern "C" fn load_state(ptr: *mut c_void, data: *const u8, len: u32) -> bool {
    let client = unsafe { client(ptr) };
    sync_shadow(client);
    let ok = unsafe { (original().load_state)(client.shadow, data, len) };
    for (i, &id) in client.ids.iter().enumerate() {
        let value = unsafe { (original().get_param)(client.shadow, id) };
        client.shared().params[i].store(value.to_bits(), SeqCst);
    }
    ok
}

/// The plugin's latency as the child reports it, plus one block in
/// pipeline mode.
unsafe extern "C" fn latency(ptr: *mut c_void) -> u32 {
    let client = unsafe { client(ptr) };
    let pipeline = unsafe { &*client.pipeline.get() }.as_ref().map_or(0, |p| p.latency as u32);
    client.shared().latency.load(SeqCst) + pipeline
}

static LATENCY: LatencyApi = LatencyApi { get: latency };

/// The plugin's editor opens in a separate process too (not the audio
/// process: an editor crash does not interrupt the sound). Its size is kept
/// by the shadow instance, which saves the state.
unsafe extern "C" fn get_extension(id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let name = unsafe { std::ffi::CStr::from_ptr(id) };
    if name == crate::abi::EXT_LATENCY {
        return &LATENCY as *const LatencyApi as *const c_void;
    }
    let inner = unsafe { (original().get_extension)(id) };
    if inner.is_null() {
        return inner;
    }
    if name == crate::abi::EXT_VIEW {
        return super::editor(unsafe { &*(inner as *const crate::abi::ViewApi) }, true);
    }
    if name == crate::abi::EXT_EDITOR_SIZE {
        return &SHADOW_EDITOR_SIZE as *const crate::abi::EditorSizeApi as *const c_void;
    }
    std::ptr::null()
}

fn editor_size() -> Option<&'static crate::abi::EditorSizeApi> {
    let ext = unsafe { (original().get_extension)(crate::abi::EXT_EDITOR_SIZE.as_ptr()) };
    unsafe { (ext as *const crate::abi::EditorSizeApi).as_ref() }
}

unsafe extern "C" fn shadow_get_editor_size(ptr: *mut c_void, width: *mut u32, height: *mut u32) -> bool {
    editor_size().is_some_and(|e| unsafe { (e.get)(client(ptr).shadow, width, height) })
}

unsafe extern "C" fn shadow_set_editor_size(ptr: *mut c_void, width: u32, height: u32) {
    if let Some(e) = editor_size() {
        unsafe { (e.set)(client(ptr).shadow, width, height) };
    }
}

static SHADOW_EDITOR_SIZE: crate::abi::EditorSizeApi =
    crate::abi::EditorSizeApi { get: shadow_get_editor_size, set: shadow_set_editor_size };
