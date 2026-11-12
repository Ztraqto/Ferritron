//! The plugin running on its own: one instance, fed by the audio device's
//! callback, with notes from MIDI devices (or an on-screen keyboard) and
//! input from the device's microphone or line in (effects).
//!
//! The parts every platform shares; each platform's module opens the audio
//! device and calls [`Renderer::render`] from its callback.

use ferritron_core::abi::{EVENT_MIDI, EXT_HOST2, Event, PluginApi};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The longest block passed to the plugin; device callbacks are split.
pub const MAX_BLOCK: usize = 1024;
const MAX_NOTES: usize = 256;
/// Events the plugin may write per block (the `out_events` the renderer passes).
const MAX_OUT: usize = 256;
/// Input buffered between the input and output callbacks, in frames.
const RING_FRAMES: usize = 16384;
/// MIDI messages the plugin queued and the MIDI thread has not sent yet.
const MIDI_OUT_MESSAGES: usize = 1024;

/// One MIDI message the plugin asked to send: `len` bytes of `bytes` (2 or 3).
#[derive(Clone, Copy, Default)]
pub struct MidiMessage {
    pub len: u8,
    pub bytes: [u8; 3],
}

/// What the UI thread, MIDI threads and the audio callback share.
pub struct Shared {
    pub api: &'static PluginApi,
    pub instance: *mut c_void,
    pub instrument: bool,
    /// Notes from MIDI or the UI, taken by the next block.
    notes: Mutex<Vec<Event>>,
    /// Stereo input, interleaved (effects).
    input: Ring<[f32; 2]>,
    /// MIDI the plugin sends (`ctx.midi_out`), taken by the MIDI output thread.
    pub(crate) midi_out: Ring<MidiMessage>,
}

// SAFETY: the instance's functions may be called from the threads the ABI
// allows; the queue and ring synchronize themselves.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// A new instance of the plugin, with the state the app saved last time.
    pub fn new(api: &'static PluginApi) -> Option<Box<Self>> {
        let instance = unsafe { (api.create)() };
        if instance.is_null() {
            return None;
        }
        let shared = Box::new(Self {
            api,
            instance,
            instrument: unsafe { (*api.info).kind == 1 },
            notes: Mutex::new(Vec::with_capacity(MAX_NOTES)),
            input: Ring::new(RING_FRAMES * 2),
            midi_out: Ring::new(MIDI_OUT_MESSAGES),
        });
        // Editors find their instance through this pointer (the view host's ctx).
        let host2 = unsafe { (api.get_extension)(EXT_HOST2.as_ptr()) } as *const ferritron_core::abi::HostApi2;
        if let Some(host2) = unsafe { host2.as_ref() } {
            let callbacks = ferritron_core::instance::HostCallbacks2 { ctx: shared.ctx(), edit: None, request_process: None };
            unsafe { (host2.attach)(instance, &callbacks) };
        }
        if let Some(state) = state_path(api).and_then(|p| std::fs::read(p).ok()) {
            unsafe { (api.load_state)(instance, state.as_ptr(), state.len() as u32) };
        }
        Some(shared)
    }

    /// The pointer editors get as their host's context.
    pub fn ctx(&self) -> *mut c_void {
        self as *const Self as *mut c_void
    }

    pub fn activate(&self, sample_rate: f64) -> bool {
        unsafe { (self.api.activate)(self.instance, sample_rate, MAX_BLOCK as u32) }
    }

    /// A MIDI message from any thread; notes on and off are kept.
    pub fn midi(&self, bytes: &[u8]) {
        let [status, key, velocity, ..] = *bytes else { return };
        let (kind, channel) = (status & 0xf0, (status & 0x0f) as i16);
        let event = match kind {
            0x90 if velocity > 0 => Event::note(0, true, key as i16, channel, velocity as f32 / 127.0, -1),
            0x80 | 0x90 => Event::note(0, false, key as i16, channel, velocity as f32 / 127.0, -1),
            _ => return,
        };
        let mut notes = self.notes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if notes.len() < MAX_NOTES {
            notes.push(event);
        }
    }

    /// Input from the device (interleaved, `channels` per frame), from its callback.
    pub fn push_input(&self, data: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        for frame in data.chunks(channels) {
            let left = frame[0];
            let right = *frame.get(1).unwrap_or(&left);
            self.input.push([left, right]);
        }
    }

    /// Saves the plugin's state for the next start.
    pub fn save(&self) {
        let Some(path) = state_path(self.api) else { return };
        let size = unsafe { (self.api.save_state)(self.instance, std::ptr::null_mut(), 0) };
        let mut data = vec![0u8; size as usize];
        if unsafe { (self.api.save_state)(self.instance, data.as_mut_ptr(), size) } == size {
            let _ = path.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(path, data);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe { (self.api.destroy)(self.instance) };
    }
}

/// Where the app keeps the plugin's state between runs.
fn state_path(api: &PluginApi) -> Option<PathBuf> {
    let id = unsafe { (*api.info).id.to_str() }?.to_string();
    Some(data_dir()?.join("standalone").join(format!("{id}.state")))
}

/// Ferritron's data folder (`ferritron.data_dir` of the platform); on phones,
/// the folder the app was given (`set_data_dir`).
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = DATA_DIR.get() {
        return Some(dir.clone());
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|p| p.join("Ferritron"))
    } else if cfg!(any(target_os = "macos", target_os = "ios")) {
        env("HOME").map(|p| p.join("Library/Application Support/Ferritron"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|p| p.join(".local/share"))).map(|p| p.join("ferritron"))
    }
}

static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The app's own folder (Android, iOS sandboxes).
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

/// The audio thread's part: block buffers and the events of a block.
pub struct Renderer {
    shared: &'static Shared,
    buffers: [Vec<f32>; 2],
    events: Vec<Event>,
    /// The events the plugin writes for the host side (`out_events`), filled by `process`.
    out: [Event; MAX_OUT],
}

impl Renderer {
    /// `shared` must outlive the renderer (the app keeps it until it exits).
    pub fn new(shared: &'static Shared) -> Self {
        Self {
            shared,
            buffers: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            events: Vec::with_capacity(MAX_NOTES),
            out: [Event::param(0, 0, 0.0); MAX_OUT],
        }
    }

    /// Fills `out` (interleaved, `channels` per frame) with the plugin's output.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.render_block(n);
            let [left, right] = &self.buffers;
            for i in 0..n {
                let frame = &mut out[(done + i) * channels..(done + i + 1) * channels];
                for (c, sample) in frame.iter_mut().enumerate() {
                    *sample = match c {
                        0 => left[i],
                        1 => right[i],
                        _ => 0.0,
                    };
                }
            }
            done += n;
        }
    }

    /// Renders `n` frames into separate left and right buffers (non-interleaved callbacks).
    pub fn render_split(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.render_block(n);
            left[done..done + n].copy_from_slice(&self.buffers[0][..n]);
            right[done..done + n].copy_from_slice(&self.buffers[1][..n]);
            done += n;
        }
    }

    fn render_block(&mut self, n: usize) {
        let shared = self.shared;
        let [left, right] = &mut self.buffers;
        if shared.instrument {
            left[..n].fill(0.0);
            right[..n].fill(0.0);
        } else {
            for i in 0..n {
                let [l, r] = shared.input.pop().unwrap_or([0.0, 0.0]);
                left[i] = l;
                right[i] = r;
            }
        }
        self.events.clear();
        // Never wait on the audio thread: notes wait for the next block if the queue is busy.
        if let Ok(mut notes) = shared.notes.try_lock() {
            self.events.extend(notes.drain(..));
        }
        let channels = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut out_count = 0u32;
        let ok = unsafe {
            let mut d = ferritron_core::abi::ProcessData::new(channels.as_ptr(), 2, n as u32, self.events.as_ptr(), self.events.len() as u32, std::ptr::null());
            d.out_events = self.out.as_mut_ptr();
            d.out_capacity = MAX_OUT as u32;
            d.out_count = &mut out_count;
            (shared.api.process)(shared.instance, &d)
        };
        if !ok {
            left[..n].fill(0.0);
            right[..n].fill(0.0);
            return;
        }
        // Queue the MIDI the plugin wrote; the MIDI thread sends it. Never waits: a full queue drops the message.
        let written = (out_count as usize).min(MAX_OUT);
        for event in &self.out[..written] {
            if event.kind != EVENT_MIDI {
                continue;
            }
            // SAFETY: `kind` is EVENT_MIDI, so the `midi` member is the one the plugin wrote.
            let midi = unsafe { event.data.midi };
            let len = match midi.status & 0xf0 {
                0x80 | 0x90 | 0xa0 | 0xb0 | 0xe0 => 3,
                0xc0 | 0xd0 => 2,
                _ => continue,
            };
            // Data bytes are 7 bits; the status byte carries the channel.
            let message = MidiMessage { len, bytes: [(midi.status & 0xf0) | (midi.channel & 0x0f), midi.data1 & 0x7f, midi.data2 & 0x7f] };
            shared.midi_out.push(message);
        }
    }
}

/// A single-producer, single-consumer ring: the audio thread pushes, another thread pops.
pub(crate) struct Ring<T> {
    data: Box<[std::cell::UnsafeCell<T>]>,
    read: AtomicUsize,
    write: AtomicUsize,
}

// SAFETY: one producer and one consumer; each slot is written before `write` publishes it.
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T: Copy + Default> Ring<T> {
    fn new(len: usize) -> Self {
        Self { data: (0..len).map(|_| std::cell::UnsafeCell::new(T::default())).collect(), read: AtomicUsize::new(0), write: AtomicUsize::new(0) }
    }

    fn push(&self, value: T) {
        let (w, r) = (self.write.load(Ordering::Relaxed), self.read.load(Ordering::Acquire));
        let next = (w + 1) % self.data.len();
        if next == r {
            return; // full: the consumer stopped reading
        }
        unsafe { *self.data[w].get() = value };
        self.write.store(next, Ordering::Release);
    }

    pub(crate) fn pop(&self) -> Option<T> {
        let (r, w) = (self.read.load(Ordering::Relaxed), self.write.load(Ordering::Acquire));
        if r == w {
            return None;
        }
        let frame = unsafe { *self.data[r].get() };
        self.read.store((r + 1) % self.data.len(), Ordering::Release);
        Some(frame)
    }
}

/// The view host editors get: values straight from the instance; edits set
/// the instance's parameters (the app is the host, nothing to record).
pub mod view_host {
    use super::Shared;
    use std::ffi::c_void;

    fn shared(ctx: *mut c_void) -> &'static Shared {
        unsafe { &*(ctx as *const Shared) }
    }
    pub unsafe extern "C" fn get(ctx: *mut c_void, id: u32) -> f64 {
        let s = shared(ctx);
        unsafe { (s.api.get_param)(s.instance, id) }
    }
    pub unsafe extern "C" fn begin(_ctx: *mut c_void, _id: u32) {}
    pub unsafe extern "C" fn perform(ctx: *mut c_void, id: u32, normalized: f64) {
        let s = shared(ctx);
        unsafe { (s.api.set_param)(s.instance, id, normalized) };
    }
    pub unsafe extern "C" fn end(_ctx: *mut c_void, _id: u32) {}

    pub fn for_shared(s: &Shared) -> ferritron_core::abi::ViewHost {
        ferritron_core::abi::ViewHost { ctx: s.ctx(), get_param: get, begin_edit: begin, perform_edit: perform, end_edit: end, request_resize: None }
    }
}
