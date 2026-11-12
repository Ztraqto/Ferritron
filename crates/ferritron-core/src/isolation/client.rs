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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize};
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

/// Not forwarded to the child process yet (isolation): reset does nothing there.
unsafe extern "C" fn reset(_instance: *mut c_void) {}

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
        reset,
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
    /// `max_block` is the most samples one block brings. The FIFO holds at most `latency` samples
    /// plus one block, so it is sized for that and never grows on the audio thread.
    fn new(latency: usize, max_block: usize) -> Self {
        let fifo = (0..MAX_CHANNELS)
            .map(|_| {
                let mut q = VecDeque::with_capacity(latency + max_block + 1);
                q.extend(std::iter::repeat_n(0.0, latency));
                q
            })
            .collect();
        Self { latency, fifo, in_flight: None }
    }
}

/// Checks (in debug builds) that `count` more samples fit in the FIFO without growing it.
fn check_fifo_room(fifo: &VecDeque<f32>, count: usize) {
    debug_assert!(fifo.len() + count <= fifo.capacity(), "pipeline FIFO would grow on the audio thread");
}

/// Where a block's output events go: the host's `out_events` (null: the host gave no room), and
/// the `out_count` it reads back. Created per call, on the audio thread, without allocating.
struct Outputs {
    events: *mut Event,
    capacity: usize,
    count: usize,
    host_count: *mut u32,
}

impl Outputs {
    /// The host's output room for this block (none if the host's `ProcessData` predates it).
    fn of(d: &crate::abi::ProcessData) -> Self {
        let has_room = d.size as usize >= size_of::<crate::abi::ProcessData>() && !d.out_count.is_null();
        let events = if has_room { d.out_events } else { std::ptr::null_mut() };
        Self {
            events,
            capacity: if events.is_null() { 0 } else { d.out_capacity as usize },
            count: 0,
            host_count: if has_room { d.out_count } else { std::ptr::null_mut() },
        }
    }

    /// Adds an event the host receives; without room it is dropped.
    fn push(&mut self, event: Event) {
        if self.count < self.capacity {
            // SAFETY: `events` holds `capacity` events, the host's, for the length of this call.
            unsafe { self.events.add(self.count).write(event) };
            self.count += 1;
        }
    }

    /// Drops what this block has produced so far (a block that timed out or lost its child gives none).
    fn discard(&mut self) {
        self.count = 0;
    }

    /// Tells the host how many events it got.
    fn finish(self) {
        if !self.host_count.is_null() {
            // SAFETY: the host passed `out_count` for this call.
            unsafe { *self.host_count = self.count as u32 };
        }
    }
}

/// Whether an event at `offset` belongs to the piece `first..end` of a block of `frames` samples:
/// the events past the block's end belong to its last piece.
fn in_piece(offset: u32, first: usize, end: usize, frames: usize) -> bool {
    let offset = offset as usize;
    offset >= first && (offset < end || end >= frames)
}

/// Moves the child's output events `src` (offsets from the start of a piece of `len` samples that
/// begins at `start`) into `out`, with offsets from the block's start, below `limit`. `mirror`
/// sees each event as it is passed on (the parameter changes update the mirror).
fn relay_events(src: &[Event], start: usize, len: usize, limit: usize, out: &mut Outputs, mut mirror: impl FnMut(&Event)) {
    if len == 0 {
        return;
    }
    for event in src {
        let mut event = *event;
        let at = (start + (event.offset as usize).min(len - 1)).min(limit.saturating_sub(1));
        event.offset = at as u32;
        mirror(&event);
        out.push(event);
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
    /// The sidechain buffers the shim handed over for the next block (audio thread only).
    sidechain: UnsafeCell<([*const f32; MAX_CHANNELS], usize)>,
    /// The audio region, sized by `activate` (main thread) and read by `process`.
    audio: UnsafeCell<Option<AudioRegion>>,
    /// The most channels a block brought that the region had no room for;
    /// the next activation makes room for them.
    wanted_channels: AtomicU32,
    /// The host renders offline (the render extension, see `remote_set_offline`).
    offline: AtomicBool,
    /// Set by `activate`, cleared when a block finishes: the first block may take longer.
    first_block: AtomicBool,
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

    /// Applies a parameter event to the mirror (other kinds are ignored).
    fn mirror_event(&self, event: &Event) {
        if event.kind == EVENT_PARAM {
            let param = unsafe { event.data.param };
            if let Some(i) = self.index(param.id) {
                self.shared().params[i].store(param.normalized.to_bits(), SeqCst);
            }
        }
    }

    /// Applies the block's parameter changes to the mirror (after the block,
    /// so the child sees the old values until each change's offset).
    fn mirror_events(&self, events: &[Event]) {
        for event in events {
            self.mirror_event(event);
        }
    }

    /// Applies the parameter changes of one piece of a block (see `run_block`).
    fn mirror_piece(&self, events: &[Event], first: usize, end: usize, frames: usize) {
        for event in events.iter().filter(|e| in_piece(e.offset, first, end, frames)) {
            self.mirror_event(event);
        }
    }

    /// Passes the child's output events for the piece `start..start + len` to the host, and
    /// applies its parameter changes to the mirror (so the host's `get_param` and the next
    /// block's sync see the plugin's own edits). Offsets are moved to the block and kept below
    /// `limit`, the block's length.
    fn relay_outputs(&self, out: &mut Outputs, start: usize, len: usize, limit: usize) {
        let shared = self.shared();
        let n = (shared.out_count as usize).min(MAX_OUT);
        relay_events(&shared.out[..n], start, len, limit, out, |e| self.mirror_event(e));
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

    /// How long to wait for the child's current block: longer offline, and for the first block
    /// after an activation, than in real time (see `shm.rs`).
    fn timeout_ms(&self) -> u32 {
        if self.offline.load(SeqCst) {
            OFFLINE_TIMEOUT_MS
        } else if self.first_block.load(SeqCst) {
            FIRST_BLOCK_TIMEOUT_MS
        } else {
            PROCESS_TIMEOUT_MS
        }
    }

    fn fail(&self) {
        // Hung or died mid-block: stop the process; the watcher starts a new one.
        self.alive.store(false, SeqCst);
        self.group.kill();
    }

    /// Runs one block in the child and waits for it, in pieces when the
    /// host sends more samples than it said it would (`max_block_size`).
    ///
    /// Each piece's input parameter changes are mirrored after the piece, and the plugin's own
    /// output changes after those, so the mirror ends with the plugin's final values.
    unsafe fn run_block(&self, audio: &AudioRegion, channels: &[*mut f32], frames: usize, events: &[Event], out: &mut Outputs) -> bool {
        if frames == 0 {
            self.mirror_events(events);
            return true;
        }
        let mut ok = true;
        let mut start = 0;
        while start < frames {
            let len = (frames - start).min(audio.frames);
            unsafe {
                self.send_block(audio, channels, start, len, frames, events);
                if !self.done.wait(Some(self.timeout_ms()), None) {
                    self.mirror_events(events);
                    out.discard();
                    self.fail();
                    return false;
                }
                self.first_block.store(false, SeqCst);
                for (ch, &dst) in channels.iter().enumerate() {
                    std::ptr::copy_nonoverlapping(audio.main(ch), dst.add(start), len);
                }
                ok &= (*self.shared).ok != 0;
            }
            self.mirror_piece(events, start, start + len, frames);
            self.relay_outputs(out, start, len, frames);
            start += len;
        }
        ok
    }

    /// Sends samples `start..start + len` of a block of `frames` samples, and
    /// the events in that range (with offsets from `start`).
    unsafe fn send_block(&self, audio: &AudioRegion, channels: &[*mut f32], start: usize, len: usize, frames: usize, events: &[Event]) {
        let shared = self.shared;
        unsafe {
            for (ch, &src) in channels.iter().enumerate() {
                std::ptr::copy_nonoverlapping(src.add(start), audio.main(ch), len);
            }
            let (sc, sc_len) = &*self.sidechain.get();
            let sc_len = (*sc_len).min(audio.channels);
            for (ch, &src) in sc.iter().enumerate().take(sc_len) {
                std::ptr::copy_nonoverlapping(src.add(start), audio.sidechain(ch), len);
            }
            (*shared).sc_channels = sc_len as u32;
            let mut count = 0;
            for e in events {
                if in_piece(e.offset, start, start + len, frames) && count < MAX_EVENTS {
                    let mut e = *e;
                    e.offset = (e.offset - start as u32).min(len.saturating_sub(1) as u32);
                    (*shared).events[count] = e;
                    count += 1;
                }
            }
            (*shared).frames = len as u32;
            (*shared).channels = channels.len() as u32;
            (*shared).num_events = count as u32;
            (*shared).ok = 0;
            (*shared).out_count = 0;
        }
        self.request.set();
    }

    /// Pipeline mode: collects the previous block, starts this one, and
    /// outputs the samples that are `latency` old.
    unsafe fn run_pipelined(&self, audio: &AudioRegion, pipeline: &mut Pipeline, channels: &[*mut f32], frames: usize, events: &[Event], out: &mut Outputs) -> bool {
        if let Some((done_frames, done_channels)) = pipeline.in_flight.take() {
            let finished = self.done.wait(Some(self.timeout_ms()), None);
            let ok = finished && unsafe { (*self.shared).ok } != 0;
            if !finished {
                self.fail();
            } else {
                self.first_block.store(false, SeqCst);
                // The previous block's outputs come out at the start of this one (their offsets
                // are one block old, so they are placed at the start, within this block).
                self.relay_outputs(out, 0, done_frames, frames);
            }
            for (ch, fifo) in pipeline.fifo.iter_mut().enumerate() {
                check_fifo_room(fifo, done_frames);
                if ok && ch < done_channels {
                    fifo.extend(unsafe { std::slice::from_raw_parts(audio.main(ch), done_frames) });
                } else {
                    fifo.extend(std::iter::repeat_n(0.0, done_frames));
                }
            }
        }
        // After a failure, fill in silence so the delay stays the same.
        for fifo in &mut pipeline.fifo {
            let missing = frames.saturating_sub(fifo.len());
            check_fifo_room(fifo, missing);
            fifo.extend(std::iter::repeat_n(0.0, missing));
        }
        if self.alive.load(SeqCst) && frames <= pipeline.latency.min(audio.frames) {
            unsafe { self.send_block(audio, channels, 0, frames, frames, events) };
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
            sidechain: UnsafeCell::new(([std::ptr::null(); MAX_CHANNELS], 0)),
            audio: UnsafeCell::new(None),
            wanted_channels: AtomicU32::new(0),
            offline: AtomicBool::new(false),
            first_block: AtomicBool::new(false),
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
        client.done.wait(Some(client.timeout_ms()), None);
    }
    // Room for the main bus's channels (and as many sidechain channels) and
    // the largest block: a new region only when the old one is too small.
    let frames = (max_block_size as usize).clamp(1, MAX_FRAMES);
    let channels = (shared.layout_len as usize).max(client.wanted_channels.load(SeqCst) as usize).clamp(2, MAX_CHANNELS);
    let audio = unsafe { &mut *client.audio.get() };
    if audio.as_ref().is_none_or(|a| a.frames < frames || a.channels < channels) {
        let generation = shared.audio_generation.load(SeqCst).wrapping_add(1);
        *audio = Memory::create(&audio_name(&client.id, generation), audio_len(frames, channels))
            .map(|memory| AudioRegion { memory, frames, channels });
        if let Some(a) = audio.as_ref() {
            shared.audio_frames.store(a.frames as u32, SeqCst);
            shared.audio_channels.store(a.channels as u32, SeqCst);
            shared.audio_generation.store(generation, SeqCst);
        } else {
            log(&format!("{}: cannot make room for audio", client.id));
        }
    }
    shared.sample_rate.store(sample_rate.to_bits(), SeqCst);
    shared.max_block.store(max_block_size, SeqCst);
    shared.setup_generation.fetch_add(1, SeqCst);
    let pipelined = options() & super::PIPELINE != 0;
    let block = (max_block_size as usize).min(MAX_FRAMES);
    unsafe { *client.pipeline.get() = pipelined.then(|| Pipeline::new(block, block)) };
    client.first_block.store(true, SeqCst);
    // Activate the child now, so its latency is known when the host asks.
    client.reactivate();
    true
}

unsafe extern "C" fn process(ptr: *mut c_void, data: *const crate::abi::ProcessData) -> bool {
    let Some(d) = (unsafe { data.as_ref() }).filter(|d| d.size as usize >= std::mem::offset_of!(crate::abi::ProcessData, out_events)) else { return false };
    let mut out = Outputs::of(d);
    let ok = unsafe { process_into(ptr, d, &mut out) };
    out.finish();
    ok
}

/// `process`, with the block's output events collected into `out`.
unsafe fn process_into(ptr: *mut c_void, d: &crate::abi::ProcessData, out: &mut Outputs) -> bool {
    let (channels, num_channels, frames, events, num_events) = (d.channels, d.num_channels, d.frames, d.events, d.num_events);
    let client = unsafe { client(ptr) };
    let events: &[Event] = if events.is_null() || num_events == 0 { &[] } else { unsafe { std::slice::from_raw_parts(events, num_events as usize) } };
    let (n, frames) = (num_channels as usize, frames as usize);
    let audio = unsafe { &*client.audio.get() };
    let Some(audio) = audio.as_ref().filter(|a| n <= a.channels && !(n > 0 && channels.is_null())) else {
        // No room for this many channels until the next activation: the shim outputs silence.
        client.wanted_channels.fetch_max(n.min(MAX_CHANNELS) as u32, SeqCst);
        client.mirror_events(events);
        return false;
    };
    let channels = if n == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(channels, n) } };
    if let Some(pipeline) = unsafe { &mut *client.pipeline.get() } {
        return unsafe { client.run_pipelined(audio, pipeline, channels, frames, events, out) };
    }
    if !client.alive.load(SeqCst) {
        client.mirror_events(events);
        return false;
    }
    unsafe { client.run_block(audio, channels, frames, events, out) }
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

unsafe extern "C" fn remote_set_transport(ptr: *mut c_void, transport: *const crate::transport::TransportC) {
    let client = unsafe { client(ptr) };
    unsafe { (*client.shared).transport = transport.as_ref().copied().unwrap_or_default() };
}

unsafe extern "C" fn remote_set_sidechain(ptr: *mut c_void, channels: *const *const f32, n: u32) {
    let client = unsafe { client(ptr) };
    let slot = unsafe { &mut *client.sidechain.get() };
    slot.1 = 0;
    if channels.is_null() {
        return;
    }
    for i in 0..(n as usize).min(MAX_CHANNELS) {
        let p = unsafe { *channels.add(i) };
        if p.is_null() {
            slot.1 = 0;
            return;
        }
        slot.0[i] = p;
        slot.1 = i + 1;
    }
}

unsafe extern "C" fn remote_set_layout(ptr: *mut c_void, speakers: *const u32, n: u32) {
    let client = unsafe { client(ptr) };
    let n = if speakers.is_null() { 0 } else { (n as usize).min(MAX_CHANNELS) };
    unsafe {
        (*client.shared).layout_len = n as u32;
        for i in 0..n {
            (*client.shared).layout[i] = *speakers.add(i);
        }
    }
}

unsafe extern "C" fn remote_take_requests(ptr: *mut c_void) -> u32 {
    unsafe { client(ptr) }.shared().requests.swap(0, SeqCst)
}

unsafe extern "C" fn remote_pending(ptr: *mut c_void) -> bool {
    unsafe { client(ptr) }.shared().requests.load(SeqCst) != 0
}

static REMOTE_TRANSPORT: crate::abi::TransportApi = crate::abi::TransportApi { set: remote_set_transport };
static REMOTE_SIDECHAIN: crate::abi::SidechainApi = crate::abi::SidechainApi { set: remote_set_sidechain };
static REMOTE_LAYOUT: crate::abi::LayoutApi = crate::abi::LayoutApi { set: remote_set_layout };
static REMOTE_MAIN: crate::abi::MainApi = crate::abi::MainApi { take_requests: remote_take_requests, pending: remote_pending };

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
    // Transport, sidechain, speaker layout and main-thread requests cross to the child through shared memory.
    if name == crate::abi::EXT_TRANSPORT {
        return &REMOTE_TRANSPORT as *const crate::abi::TransportApi as *const c_void;
    }
    if name == crate::abi::EXT_SIDECHAIN {
        return &REMOTE_SIDECHAIN as *const crate::abi::SidechainApi as *const c_void;
    }
    if name == crate::abi::EXT_LAYOUT {
        return &REMOTE_LAYOUT as *const crate::abi::LayoutApi as *const c_void;
    }
    if name == crate::abi::EXT_MAIN {
        return &REMOTE_MAIN as *const crate::abi::MainApi as *const c_void;
    }
    if name == crate::abi::EXT_VIEW {
        return super::editor(unsafe { &*(inner as *const crate::abi::ViewApi) }, true);
    }
    if name == crate::abi::EXT_EDITOR_SIZE {
        return &SHADOW_EDITOR_SIZE as *const crate::abi::EditorSizeApi as *const c_void;
    }
    if name == crate::abi::EXT_RENDER {
        return &REMOTE_RENDER as *const crate::abi::RenderApi as *const c_void;
    }
    std::ptr::null()
}

/// The host renders offline: the client keeps the flag (for the longer block timeout), tells the
/// child through shared memory (the child passes it on before its next block, see `server.rs`)
/// and tells the shadow instance.
unsafe extern "C" fn remote_set_offline(ptr: *mut c_void, offline: bool) {
    let client = unsafe { client(ptr) };
    client.offline.store(offline, SeqCst);
    client.shared().offline.store(offline as u32, SeqCst);
    let inner = unsafe { (original().get_extension)(crate::abi::EXT_RENDER.as_ptr()) } as *const crate::abi::RenderApi;
    if let Some(inner) = unsafe { inner.as_ref() } {
        unsafe { (inner.set_offline)(client.shadow, offline) };
    }
}

static REMOTE_RENDER: crate::abi::RenderApi = crate::abi::RenderApi { set_offline: remote_set_offline };

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A host buffer of `capacity` events and the `Outputs` that writes into it.
    fn room(buf: &mut [Event], count: &mut u32) -> Outputs {
        Outputs { events: buf.as_mut_ptr(), capacity: buf.len(), count: 0, host_count: count }
    }

    /// The child's output events of a piece are moved to the block: offsets from the piece's
    /// start, every parameter change reaches the mirror, and the host gets the count.
    #[test]
    fn a_pieces_outputs_reach_the_host_with_block_offsets() {
        let src = [Event::param(3, 7, 0.5), Event::midi(10, 0x90, 1, 60, 100), Event::param(900, 8, 0.25)];
        let mut buf = vec![Event::param(0, 0, 0.0); 8];
        let mut count = 0;
        let mut out = room(&mut buf, &mut count);
        let mut mirrored = Vec::new();
        // The piece is 64 samples long and starts at sample 128 of a 256-sample block.
        relay_events(&src, 128, 64, 256, &mut out, |e| mirrored.push((e.kind, e.offset)));
        out.finish();
        assert_eq!(count, 3);
        assert_eq!(buf[0].offset, 131);
        assert_eq!(buf[1].offset, 138);
        // An offset past the piece is kept inside it.
        assert_eq!(buf[2].offset, 191);
        let p = unsafe { buf[0].data.param };
        assert_eq!((p.id, p.normalized), (7, 0.5));
        assert_eq!(mirrored, vec![(EVENT_PARAM, 131), (crate::abi::EVENT_MIDI, 138), (EVENT_PARAM, 191)]);
    }

    /// The host's room is respected: events beyond the capacity are dropped.
    #[test]
    fn outputs_stop_at_the_hosts_capacity() {
        let src = [Event::param(0, 1, 0.0), Event::param(1, 2, 0.0), Event::param(2, 3, 0.0)];
        let mut buf = vec![Event::param(0, 0, 0.0); 2];
        let mut count = 0;
        let mut out = room(&mut buf, &mut count);
        relay_events(&src, 0, 16, 16, &mut out, |_| {});
        out.finish();
        assert_eq!(count, 2);
    }

    /// A host with no output buffer gets a count of 0; the mirror still follows the plugin.
    #[test]
    fn a_host_without_room_gets_no_events() {
        let mut count = 99;
        let mut out = Outputs { events: std::ptr::null_mut(), capacity: 0, count: 0, host_count: &mut count };
        let mut mirrored = 0;
        relay_events(&[Event::param(0, 1, 0.0)], 0, 8, 8, &mut out, |_| mirrored += 1);
        out.finish();
        assert_eq!((count, mirrored), (0, 1));
    }

    /// A block that timed out gives no outputs, even when earlier pieces produced some.
    #[test]
    fn discarding_drops_the_block_so_far() {
        let mut buf = vec![Event::param(0, 0, 0.0); 4];
        let mut count = 0;
        let mut out = room(&mut buf, &mut count);
        relay_events(&[Event::param(0, 1, 0.0)], 0, 8, 16, &mut out, |_| {});
        out.discard();
        out.finish();
        assert_eq!(count, 0);
    }

    /// The last piece takes the events past the block's end; the others do not.
    #[test]
    fn events_past_the_end_belong_to_the_last_piece() {
        assert!(in_piece(10, 0, 16, 64));
        assert!(!in_piece(20, 0, 16, 64), "the first piece ends at 16");
        assert!(in_piece(20, 16, 32, 64));
        assert!(in_piece(70, 48, 64, 64), "past the block: the last piece");
        assert!(!in_piece(70, 0, 16, 64));
    }

    /// A host that passes `out_count` but no `out_events` gets a count of 0 and no events.
    #[test]
    fn a_host_with_no_event_buffer_gets_count_zero() {
        let mut d = crate::abi::ProcessData::new(std::ptr::null(), 0, 0, std::ptr::null(), 0, std::ptr::null());
        let mut count = 5u32;
        d.out_count = &mut count;
        d.out_events = std::ptr::null_mut();
        let out = Outputs::of(&d);
        assert!(out.events.is_null());
        assert_eq!(out.capacity, 0);
        out.finish();
        assert_eq!(count, 0);
    }
}
