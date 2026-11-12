//! Test plugins without a host.
//!
//! [`Harness`] drives a plugin through the same processing path the format
//! shims use (block splitting at parameter changes, bypass, notes, panic
//! guard), so tests see exactly what a DAW would hear.
//!
//! ```ignore
//! let mut h = ferritron::testing::Harness::<MyPlugin>::new(48_000.0, 512);
//! h.set_param("gain", -6.0);
//! let mut left = [1.0; 64];
//! let mut right = [1.0; 64];
//! assert!(h.process(&mut [&mut left, &mut right]));
//! ```

use crate::__private::PluginMeta;
use crate::abi::{self, Event};
use crate::{ParamSet, Process};
use std::ffi::c_void;
use std::marker::PhantomData;

#[cfg(feature = "alloc-check")]
pub use crate::alloc_check::AllocGuard;
pub use crate::abi::MemoryReport;

/// How parameter changes reach `process` ([`Harness::set_automation_mode`]; the
/// project file's `[process] automation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationMode {
    /// At the start of each control period (the default).
    Control,
    /// At the sample of each change (1.x).
    Sample,
    /// One call per block; the plugin reads `ctx.param_events()`.
    Events,
}

/// A parameter as hosts see it ([`Harness::host_param`]).
#[derive(Clone, Debug, PartialEq)]
pub struct HostParamView {
    pub id: u32,
    pub name: String,
    pub default_normalized: f64,
    pub flags: u32,
}

/// What [`Harness::measure_latency`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyMeasurement {
    /// Samples between the impulse going in and the loudest output sample.
    pub measured: u32,
    /// The latency the plugin tells hosts ([`Harness::latency`]).
    pub reported: u32,
}

impl LatencyMeasurement {
    /// The reported latency is the delay that was measured.
    pub fn matches(&self) -> bool {
        self.measured == self.reported
    }
}

/// The check behind `cargo ferritron check`: measures the plugin with a fresh [`Harness`]
/// (48 kHz, 512 samples) and prints one line, `FERRITRON-CHECK latency measured=<n> reported=<n>`
/// (or `silent reported=<n>`), which the command reads. Put it in a test of the plugin's crate
/// whose name contains `latency`:
///
/// ```ignore
/// #[test]
/// fn reported_latency_matches_the_delay() {
///     ferritron::testing::check_latency::<MyPlugin>().unwrap();
/// }
/// ```
///
/// `Ok(None)`: the plugin is silent for an impulse (nothing to compare). `Err`: it reports a
/// different latency than it delays, the message says how much.
pub fn check_latency<P: Process + PluginMeta>() -> Result<Option<LatencyMeasurement>, String> {
    let mut h = Harness::<P>::new(48_000.0, 512);
    match h.measure_latency() {
        None => {
            println!("FERRITRON-CHECK latency silent reported={}", h.latency());
            Ok(None)
        }
        Some(m) => {
            println!("FERRITRON-CHECK latency measured={} reported={}", m.measured, m.reported);
            if m.matches() {
                Ok(Some(m))
            } else {
                Err(format!("the plugin reports a latency of {} samples but delays by {} samples", m.reported, m.measured))
            }
        }
    }
}

pub struct Harness<P: Process + PluginMeta> {
    instance: *mut c_void,
    events: Vec<Event>,
    audio_allocs: u64,
    /// What the plugin wrote during the last `process` (`Context::midi_out`, `set_param`).
    out_events: Vec<Event>,
    /// The buffer `process` lends the plugin for its output events (allocated once).
    out_buffer: Vec<Event>,
    /// Channel pointers for `process` (allocated once).
    ptrs: Vec<*mut f32>,
    max_block: usize,
    sample_rate: f64,
    _plugin: PhantomData<P>,
}

impl<P: Process + PluginMeta> Harness<P> {
    /// Creates and activates an instance for real-time processing.
    pub fn new(sample_rate: f64, max_block_size: usize) -> Self {
        Self::with_mode(sample_rate, max_block_size, false)
    }

    /// Creates and activates an instance for offline rendering, as a host
    /// does for a bounce ([`Setup::offline`](crate::Setup::offline)).
    pub fn offline(sample_rate: f64, max_block_size: usize) -> Self {
        Self::with_mode(sample_rate, max_block_size, true)
    }

    fn with_mode(sample_rate: f64, max_block_size: usize, offline: bool) -> Self {
        let instance = unsafe { abi::create::<P>() };
        assert!(!instance.is_null(), "creating the plugin failed (it panicked in Default)");
        let render = unsafe { abi::get_extension::<P>(abi::EXT_RENDER.as_ptr()) } as *const abi::RenderApi;
        if let Some(render) = unsafe { render.as_ref() } {
            unsafe { (render.set_offline)(instance, offline) };
        }
        unsafe { abi::activate::<P>(instance, sample_rate, max_block_size as u32) };
        Self { instance, events: Vec::new(), audio_allocs: 0, out_events: Vec::new(), out_buffer: vec![Event::midi(0, 0, 0, 0, 0); 256], ptrs: Vec::with_capacity(crate::MAX_CHANNELS), max_block: max_block_size, sample_rate, _plugin: PhantomData }
    }

    /// The host starts (`true`) or ends an offline render (export) from the
    /// next block on: `ctx.quality()` follows at once (see [`Quality`](crate::Quality)).
    pub fn set_offline(&mut self, offline: bool) {
        let render = unsafe { abi::get_extension::<P>(abi::EXT_RENDER.as_ptr()) } as *const abi::RenderApi;
        if let Some(render) = unsafe { render.as_ref() } {
            unsafe { (render.set_offline)(self.instance, offline) };
        }
    }

    /// Whether the last `process` call slept (silence, see [`Tail`](crate::Tail)).
    pub fn sleeping(&self) -> bool {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_SLEEP.as_ptr()) } as *const abi::SleepApi;
        unsafe { ext.as_ref() }.is_some_and(|s| unsafe { (s.silent)(self.instance) })
    }

    /// What a host sees of parameter `index` (the order of the C ABI: the
    /// plugin's own, then Bypass, AI Access, Quality): its ID, name, default
    /// as a normalized value, and flags (`FERRITRON_PARAM_*`).
    pub fn host_param(&self, index: u32) -> Option<HostParamView> {
        let mut c = std::mem::MaybeUninit::<abi::ParamInfoC>::zeroed();
        if !unsafe { abi::param_info_of::<P>(index, c.as_mut_ptr()) } {
            return None;
        }
        let c = unsafe { c.assume_init() };
        Some(HostParamView { id: c.id, name: unsafe { c.name.to_str() }.unwrap_or("").to_string(), default_normalized: c.default_normalized, flags: c.flags })
    }

    /// The value of parameter `key` as hosts see it: normalized `0..=1`
    /// (through the parameter's scale).
    pub fn normalized(&self, key: &str) -> f64 {
        unsafe { abi::get_param::<P>(self.instance, Self::id(key)) }
    }

    /// A host sets parameter `key` from a normalized value.
    pub fn set_normalized(&mut self, key: &str, normalized: f64) {
        unsafe { abi::set_param::<P>(self.instance, Self::id(key), normalized) };
    }

    /// The text hosts show for parameter `key` at the normalized value `normalized`.
    pub fn text_of(&self, key: &str, normalized: f64) -> String {
        let mut buf = [0u8; 256];
        let len = unsafe { abi::format_param::<P>(Self::id(key), normalized, buf.as_mut_ptr(), buf.len() as u32) };
        String::from_utf8_lossy(&buf[..len.max(0) as usize]).into_owned()
    }

    /// The text hosts show for the current value of parameter `key`.
    pub fn text(&self, key: &str) -> String {
        self.text_of(key, self.normalized(key))
    }

    /// What a host makes of `text` typed into parameter `key`: the normalized
    /// value, or `None` when it is not accepted (the value then stays).
    pub fn parse_text(&self, key: &str, text: &str) -> Option<f64> {
        let mut out = 0.0f64;
        let bytes = text.as_bytes();
        let s = abi::FerritronStr { ptr: bytes.as_ptr(), len: bytes.len() as u32 };
        unsafe { abi::parse_param::<P>(Self::id(key), s, &mut out) }.then_some(out)
    }

    /// The host's transport from the next block on (see [`Transport`](crate::Transport)).
    pub fn set_transport(&mut self, transport: crate::Transport) {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_TRANSPORT.as_ptr()) } as *const abi::TransportApi;
        let c = crate::transport::TransportC::from_transport(&transport);
        if let Some(api) = unsafe { ext.as_ref() } {
            unsafe { (api.set)(self.instance, &c) };
        }
    }

    /// The work the plugin asked of the host's main thread since the last call
    /// (`abi::REQUEST_*` bits), cleared by the call: what a shim polls. Runs the
    /// `on_notify` handler for waiting `notify` codes, as a shim's poll does.
    pub fn take_main_requests(&mut self) -> u32 {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_MAIN.as_ptr()) } as *const abi::MainApi;
        unsafe { ext.as_ref() }.map_or(0, |api| unsafe { (api.take_requests)(self.instance) })
    }

    /// How many parameters hosts see.
    pub fn host_param_count(&self) -> u32 {
        unsafe { abi::param_count::<P>() }
    }

    /// The latency the plugin reports to hosts, in samples.
    pub fn latency(&self) -> u32 {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_LATENCY.as_ptr()) } as *const abi::LatencyApi;
        unsafe { ext.as_ref() }.map_or(0, |l| unsafe { (l.get)(self.instance) })
    }

    /// Measures the delay of the plugin's output: sends a single sample of 1.0 and finds where the
    /// loudest output sample comes out (the first one, over all channels). The result sets that
    /// delay beside [`latency`](Self::latency), what the plugin tells hosts, so a plugin that
    /// reports less or more than it delays shows up (`cargo ferritron check`).
    ///
    /// `None` when the plugin does not answer the impulse (silent output: an instrument,
    /// a muted plugin). It suits plugins whose main effect is a delay (look-ahead limiters,
    /// linear-phase filters, oversamplers); a resonant filter's loudest sample is not its delay.
    /// Use a fresh harness: the impulse goes through the plugin's state. Parameters set before
    /// the call are kept; a plugin that changes its latency while running (`request_latency`)
    /// is measured after one block of silence so the new value is already in place.
    pub fn measure_latency(&mut self) -> Option<LatencyMeasurement> {
        let block = self.max_block.clamp(1, 1024);
        let channels = P::INFO.audio.min_channels.clamp(1, 2) as usize;
        let mut silence = vec![vec![0.0f32; block]; channels];
        let mut refs: Vec<&mut [f32]> = silence.iter_mut().map(|c| c.as_mut_slice()).collect();
        self.process(&mut refs);
        let reported = self.latency();
        let total = (2 * reported as usize + 8192).min(1 << 20);
        let (mut best, mut best_at) = (0.0f32, 0usize);
        let mut done = 0;
        while done < total {
            let mut buffers = vec![vec![0.0f32; block]; channels];
            if done == 0 {
                for c in &mut buffers {
                    c[0] = 1.0;
                }
            }
            let mut refs: Vec<&mut [f32]> = buffers.iter_mut().map(|c| c.as_mut_slice()).collect();
            if !self.process(&mut refs) {
                return None;
            }
            for c in &buffers {
                for (i, v) in c.iter().enumerate() {
                    if v.abs() > best * 1.000_001 {
                        best = v.abs();
                        best_at = done + i;
                    }
                }
            }
            done += block;
        }
        (best > 1e-6).then_some(LatencyMeasurement { measured: best_at as u32, reported: self.latency() })
    }

    fn id(key: &str) -> u32 {
        // The built-in switches: "ferritron.bypass", "ferritron.mcp" (AI Access),
        // "ferritron.quality" (0 Auto, 1 Normal, 2 Best).
        if key.starts_with("ferritron.") {
            return crate::__private::reserved_id(key);
        }
        let infos = P::Params::infos();
        infos.iter().find(|i| i.key == key).unwrap_or_else(|| panic!("no parameter `{key}`")).id
    }

    /// Sets a parameter immediately, in plain units (e.g. dB).
    pub fn set_param(&mut self, key: &str, plain: f64) {
        let id = Self::id(key);
        unsafe { abi::set_param::<P>(self.instance, id, abi::to_normalized::<P>(id, plain)) };
    }

    /// Current value of a parameter in plain units.
    pub fn param(&self, key: &str) -> f64 {
        let id = Self::id(key);
        unsafe { abi::to_plain::<P>(id, abi::get_param::<P>(self.instance, id)) }
    }

    /// Schedules a parameter change at `offset` within the next `process` call.
    pub fn automate(&mut self, offset: u32, key: &str, plain: f64) {
        let id = Self::id(key);
        self.events.push(Event::param(offset, id, unsafe { abi::to_normalized::<P>(id, plain) }));
    }

    pub fn set_bypass(&mut self, on: bool) {
        unsafe { abi::set_param::<P>(self.instance, crate::__private::BYPASS_ID, if on { 1.0 } else { 0.0 }) };
    }

    /// Schedules a note on at `offset` within the next `process` call.
    pub fn note_on(&mut self, offset: u32, key: u8, velocity: f32) {
        self.events.push(Event::note(offset, true, key as i16, 0, velocity, -1));
    }

    pub fn note_off(&mut self, offset: u32, key: u8) {
        self.events.push(Event::note(offset, false, key as i16, 0, 0.0, -1));
    }

    /// Schedules a MIDI message for the next `process` (`ctx.midi()`).
    pub fn midi(&mut self, offset: u32, channel: u8, kind: crate::MidiKind) {
        use crate::MidiKind::*;
        let b = |v: f32| (v.clamp(0.0, 1.0) * 127.0).round() as u8;
        let (status, d1, d2) = match kind {
            NoteOn { key, velocity } => (0x90, key, b(velocity).max(1)),
            NoteOff { key, velocity } => (0x80, key, b(velocity)),
            Cc { number, value } => (0xB0, number, value),
            PitchBend(v) => {
                let n = (((v.clamp(-1.0, 1.0) + 1.0) * 8192.0).round() as i32).clamp(0, 16383);
                (0xE0, (n & 0x7F) as u8, (n >> 7) as u8)
            }
            ChannelPressure(v) => (0xD0, b(v), 0),
            PolyPressure { key, pressure } => (0xA0, key, b(pressure)),
            ProgramChange(p) => (0xC0, p, 0),
        };
        self.events.push(Event::midi(offset, status, channel, d1, d2));
    }

    /// The speaker of each channel of the main bus, as a shim reports them
    /// before `activate`. Call it, then [`Harness::reactivate`].
    pub fn set_layout(&mut self, speakers: &[crate::Speaker]) {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_LAYOUT.as_ptr()) } as *const abi::LayoutApi;
        let ids: Vec<u32> = speakers.iter().map(|s| s.id()).collect();
        if let Some(api) = unsafe { ext.as_ref() } {
            unsafe { (api.set)(self.instance, ids.as_ptr(), ids.len() as u32) };
        }
    }

    /// Activates the plugin again (a host does after an arrangement change).
    pub fn reactivate(&mut self, sample_rate: f64, max_block_size: usize) {
        unsafe { abi::activate::<P>(self.instance, sample_rate, max_block_size as u32) };
        (self.sample_rate, self.max_block) = (sample_rate, max_block_size);
    }

    /// The ID of parameter `index` in the hosts' order (see [`host_param`](Self::host_param)).
    fn id_at(&self, index: u32) -> u32 {
        self.host_param(index).unwrap_or_else(|| panic!("no parameter at index {index}")).id
    }

    /// Sets parameter `index` (the hosts' order: the plugin's own as in
    /// `Params::infos()`, then the built-in switches) in plain units.
    pub fn set_param_index(&mut self, index: u32, plain: f64) {
        self.set_param_id(self.id_at(index), plain);
    }

    /// The value of parameter `index` in plain units.
    pub fn param_index(&self, index: u32) -> f64 {
        self.param_id(self.id_at(index))
    }

    /// Sets the parameter with host ID `id` in plain units.
    pub fn set_param_id(&mut self, id: u32, plain: f64) {
        unsafe { abi::set_param::<P>(self.instance, id, abi::to_normalized::<P>(id, plain)) };
    }

    /// The value of the parameter with host ID `id` in plain units.
    pub fn param_id(&self, id: u32) -> f64 {
        unsafe { abi::to_plain::<P>(id, abi::get_param::<P>(self.instance, id)) }
    }

    /// The flags hosts see for parameter `index` (`FERRITRON_PARAM_*` of the C ABI: automatable, hidden, bypass, ...).
    pub fn flags(&self, index: u32) -> u32 {
        self.host_param(index).map_or(0, |p| p.flags)
    }

    /// The text hosts show for parameter `index` now.
    pub fn text_index(&self, index: u32) -> String {
        let id = self.id_at(index);
        let mut buf = [0u8; 256];
        let normalized = unsafe { abi::get_param::<P>(self.instance, id) };
        let len = unsafe { abi::format_param::<P>(id, normalized, buf.as_mut_ptr(), buf.len() as u32) };
        String::from_utf8_lossy(&buf[..len.max(0) as usize]).into_owned()
    }

    /// The latency a host ends up with: it handles the plugin's requests to the main thread
    /// (a new latency makes it activate the plugin again, as VST3 `restartComponent` and CLAP
    /// `request_restart` do), then asks.
    pub fn host_latency(&mut self) -> u32 {
        if self.take_main_requests() & abi::REQUEST_LATENCY != 0 {
            let (rate, block) = (self.sample_rate, self.max_block);
            self.reactivate(rate, block);
        }
        self.latency()
    }

    /// Sets several parameters at once in plain units, as a preset or a state does: the
    /// plugin switches to all of them at the start of the next block, and smoothed
    /// parameters jump instead of gliding.
    /// Clears the plugin's state (delay lines, voices, smoothing) as a host's reset does.
    pub fn reset(&mut self) {
        unsafe { abi::reset::<P>(self.instance) };
    }

    pub fn load_values(&mut self, values: &[(&str, f64)]) {
        self.instance().load_values_by_key(values);
    }

    /// Applies parameter changes as `mode` says from now on, whatever the project file says
    /// (`control_block` 0 keeps its control period). To compare modes in one test.
    pub fn set_automation_mode(&mut self, mode: AutomationMode, control_block: u32) {
        let mode = match mode {
            AutomationMode::Control => 0,
            AutomationMode::Sample => 1,
            AutomationMode::Events => 2,
        };
        unsafe { abi::set_automation::<P>(self.instance, mode, control_block) };
    }

    /// A parameter change event at `offset` in plain units, for [`process_events`](Self::process_events)
    /// (`Event::note`, `Event::midi` make the others).
    pub fn param_event(&self, offset: u32, key: &str, plain: f64) -> Event {
        let id = Self::id(key);
        Event::param(offset, id, unsafe { abi::to_normalized::<P>(id, plain) })
    }

    /// Processes with these events (any kind, any order: sorted by offset as shims do) in
    /// addition to those scheduled with `automate`, `note_on`, `midi`, ...
    pub fn process_events(&mut self, channels: &mut [&mut [f32]], events: &[Event]) -> bool {
        self.events.extend_from_slice(events);
        self.process(channels)
    }

    /// What the instance holds in memory as far as Ferritron knows (T5: the framework's share
    /// per instance). Call it after `new` (activation allocates the buffers).
    pub fn memory_report(&self) -> MemoryReport {
        unsafe { abi::memory_of::<P>(self.instance) }
    }

    /// Like [`process`](Self::process) with a sidechain input (`ctx.sidechain()`).
    /// An empty `sidechain` leaves it unconnected.
    pub fn process_with_sidechain(&mut self, channels: &mut [&mut [f32]], sidechain: &[&[f32]]) -> bool {
        let ptrs: Vec<*const f32> = sidechain.iter().map(|c| c.as_ptr()).collect();
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_SIDECHAIN.as_ptr()) } as *const abi::SidechainApi;
        let api = unsafe { ext.as_ref() };
        if let Some(api) = api {
            unsafe { (api.set)(self.instance, ptrs.as_ptr(), ptrs.len() as u32) };
        }
        let ok = self.process(channels);
        if let Some(api) = api {
            unsafe { (api.set)(self.instance, std::ptr::null(), 0) };
        }
        ok
    }

    /// Processes the channels in place with the scheduled events. Returns
    /// `false` if the plugin panicked (it then stays silent, as in a host).
    pub fn process(&mut self, channels: &mut [&mut [f32]]) -> bool {
        let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
        self.ptrs.clear();
        self.ptrs.extend(channels.iter_mut().map(|c| c.as_mut_ptr()));
        let ptrs = &self.ptrs;
        // Stable, as the shims sort: events at one offset keep their order.
        self.events.sort_by_key(|e| e.offset);
        let mut events = std::mem::take(&mut self.events);
        let instance = self.instance;
        let out = &mut self.out_buffer;
        let mut out_count = 0u32;
        let mut run = || unsafe {
            let mut data = abi::ProcessData::new(ptrs.as_ptr(), ptrs.len() as u32, frames as u32, events.as_ptr(), events.len() as u32, std::ptr::null());
            data.out_events = out.as_mut_ptr();
            data.out_capacity = out.len() as u32;
            data.out_count = &mut out_count;
            abi::process::<P>(instance, &data)
        };
        #[cfg(feature = "alloc-check")]
        let ok = {
            let (ok, n) = crate::alloc_check::audio_scope(run);
            self.audio_allocs += n;
            ok
        };
        #[cfg(not(feature = "alloc-check"))]
        let ok = run();
        self.out_events.clear();
        self.out_events.extend_from_slice(&self.out_buffer[..out_count as usize]);
        // Keep the event buffer's room for the next block.
        events.clear();
        self.events = events;
        ok
    }

    /// MIDI the plugin sent out in the last `process` (`Context::midi_out`):
    /// (offset, channel, kind).
    pub fn midi_out(&self) -> Vec<(u32, u8, crate::MidiKind)> {
        self.out_events
            .iter()
            .filter(|e| e.kind == abi::EVENT_MIDI)
            .filter_map(|e| {
                let m = unsafe { e.data.midi };
                crate::MidiKind::from_raw(m.status, m.data1, m.data2).map(|k| (e.offset, m.channel, k))
            })
            .collect()
    }

    /// Voices the plugin reported as ended in the last `process` (`EVENT_NOTE_END`, sent at a
    /// note-off or by `Context::voice_ended`): (offset, note ID, channel, key).
    pub fn voice_ends(&self) -> Vec<(u32, i32, i16, i16)> {
        self.out_events
            .iter()
            .filter(|e| e.kind == abi::EVENT_NOTE_END)
            .map(|e| {
                let n = unsafe { e.data.note };
                (e.offset, n.note_id, n.channel, n.key)
            })
            .collect()
    }

    /// Parameter changes the plugin made from the audio thread in the last
    /// `process` (`Context::set_param`): (parameter ID, normalized value).
    pub fn param_edits(&self) -> Vec<(u32, f64)> {
        self.out_events
            .iter()
            .filter(|e| e.kind == abi::EVENT_PARAM)
            .map(|e| {
                let p = unsafe { e.data.param };
                (p.id, p.normalized)
            })
            .collect()
    }

    /// Allocations and frees `process` made on the calling thread so far.
    /// Counted only with feature `alloc-check` and `AllocGuard`
    /// installed as the global allocator; otherwise 0.
    pub fn audio_allocations(&self) -> u64 {
        self.audio_allocs
    }

    /// Panics if `process` allocated or freed anything so far.
    pub fn assert_no_alloc(&self) {
        assert_eq!(self.audio_allocs, 0, "the audio thread allocated {} time(s) in process", self.audio_allocs);
    }

    /// The plugin's typed parameters (`h.params().gain.value()`).
    pub fn params(&self) -> &P::Params {
        unsafe { abi::params_of::<P>(self.instance) }
    }

    /// The instance's handle: identity, track, store, edits, messages.
    pub fn instance(&self) -> &crate::InstanceHandle {
        unsafe { abi::handle_of::<P>(self.instance) }
    }

    /// Loads factory preset `name` (its id or name, from `[presets]`), as a host's program change
    /// or an editor's preset menu does: the values take effect at the start of the next block and
    /// the smoothers jump to them. Returns `false` if there is no such preset.
    pub fn load_preset(&mut self, name: &str) -> bool {
        crate::presets::load::<P>(self.instance(), name)
    }

    /// Opens or closes a pretend editor: data streams keep what the audio thread writes only
    /// while one is open (see [`stream`](Self::stream)).
    pub fn set_editor_open(&mut self, open: bool) {
        self.instance().editor_opened(open);
    }

    /// The newest values of data stream `name` (`[streams]` in the project file); empty until
    /// something was written. Nothing is written while no editor is open: call
    /// [`set_editor_open`](Self::set_editor_open) first.
    pub fn stream(&self, name: &str) -> Vec<f32> {
        self.instance().stream(name).map(|mut r| r.latest()).unwrap_or_default()
    }

    /// Runs the instance's worker (and every other registered worker) once now, on this
    /// thread, whatever the interval: a deterministic stand-in for waiting for the worker
    /// thread.
    pub fn run_workers(&self) {
        crate::worker::poll_now();
    }

    /// Tells the instance the track it sits on, as a host does.
    pub fn set_track(&mut self, track: crate::TrackInfo) {
        use crate::instance::{PluginLocation, TRACK_HAS_CHANNELS, TRACK_HAS_COLOR, TRACK_HAS_INDEX, TRACK_HAS_KIND, TRACK_HAS_LOCATION, TRACK_HAS_NAME, TrackInfoC, TrackKind};
        let name = track.name.clone().unwrap_or_default();
        let info = TrackInfoC {
            flags: if track.name.is_some() { TRACK_HAS_NAME } else { 0 }
                | if track.color.is_some() { TRACK_HAS_COLOR } else { 0 }
                | if track.index.is_some() { TRACK_HAS_INDEX } else { 0 }
                | if track.kind.is_some() { TRACK_HAS_KIND } else { 0 }
                | if track.location.is_some() { TRACK_HAS_LOCATION } else { 0 }
                | if track.channels.is_some() { TRACK_HAS_CHANNELS } else { 0 },
            name: abi::FerritronStr { ptr: name.as_ptr(), len: name.len() as u32 },
            color: track.color.unwrap_or_default(),
            index: track.index.unwrap_or_default(),
            kind: match track.kind {
                Some(TrackKind::Return) => 1,
                Some(TrackKind::Bus) => 2,
                Some(TrackKind::Master) => 3,
                _ => 0,
            },
            location: match track.location {
                Some(PluginLocation::PostFader) => 1,
                Some(PluginLocation::Panner) => 2,
                _ => 0,
            },
            channels: track.channels.unwrap_or_default(),
        };
        let api = unsafe { abi::get_extension::<P>(abi::EXT_TRACK.as_ptr()) } as *const abi::TrackApi;
        if let Some(api) = unsafe { api.as_ref() } {
            unsafe { (api.set_track_info)(self.instance, &info) };
        }
    }

    /// Sends a message to the plugin as its web editor does
    /// (`ferritron.send`), and returns what the plugin posted back
    /// (`InstanceHandle::post`) so far.
    pub fn send_message(&mut self, json: &str) -> Vec<String> {
        let handle = self.instance().clone();
        handle.deliver_message(json);
        handle.take_posted()
    }

    /// Saves the plugin state, as a host does when storing a project.
    pub fn save_state(&self) -> Vec<u8> {
        let size = unsafe { abi::save_state::<P>(self.instance, std::ptr::null_mut(), 0) };
        let mut data = vec![0; size as usize];
        unsafe { abi::save_state::<P>(self.instance, data.as_mut_ptr(), size) };
        data
    }

    pub fn load_state(&mut self, data: &[u8]) -> bool {
        unsafe { abi::load_state::<P>(self.instance, data.as_ptr(), data.len() as u32) }
    }
}

impl<P: Process + PluginMeta> Drop for Harness<P> {
    fn drop(&mut self) {
        unsafe { abi::destroy::<P>(self.instance) };
    }
}

/// Checks a plugin against a fixture saved by an older Ferritron (`tests/fixtures/<version>/<name>.json`:
/// its parameters as hosts saw them and a saved state; G5, A15). Fails when
///
/// - a parameter of the fixture is gone, moved to another index, or has another ID (hosts would
///   lose automation and saved values),
/// - the plugin's class ID changed (hosts would not find the plugin in old projects),
/// - the old state does not load, or loads to other values than it was saved with.
///
/// New parameters after the old ones are fine. Returns how many parameters were compared.
///
/// ```ignore
/// #[test]
/// fn loads_state_saved_by_1_4_0() {
///     ferritron::testing::check_fixture::<MyPlugin>(include_str!("../../../tests/fixtures/1.4.0/my-plugin.json")).unwrap();
/// }
/// ```
pub fn check_fixture<P: Process + PluginMeta>(fixture: &str) -> Result<usize, String> {
    let json: serde_json::Value = serde_json::from_str(fixture).map_err(|e| format!("the fixture is not JSON: {e}"))?;
    let from = json["ferritron"].as_str().unwrap_or("?");
    if json["plugin"].as_str() != Some(P::INFO.id) {
        return Err(format!("the fixture is of plugin {} , not {}", json["plugin"], P::INFO.id));
    }
    if let Some(class) = json["class_id"].as_str() {
        let now: String = P::INFO.class_id.iter().map(|b| format!("{b:02x}")).collect();
        if class != now && !replaced_in_project_file(class) {
            return Err(format!(
                "the class ID changed since {from}: {class} then, {now} now (list the old one under `[vst3] compatibility.replaces` as hosts showed it)"
            ));
        }
    }
    let mut h = Harness::<P>::new(48_000.0, 512);
    // Not processed afterwards: the fixture's values are arbitrary (they may switch on a
    // plugin's test switches, such as one that crashes on purpose).
    let params = json["params"].as_array().ok_or("the fixture has no parameters")?;
    let mut problems = Vec::new();
    for p in params {
        let (index, id, name) = (p["index"].as_u64().unwrap_or(0) as u32, p["id"].as_u64().unwrap_or(0) as u32, p["name"].as_str().unwrap_or(""));
        match h.host_param(index) {
            Some(now) if now.id == id => {}
            Some(now) => problems.push(format!("parameter {index} ({name}) had ID {id} in {from}, now {} ({})", now.id, now.name)),
            None => problems.push(format!("parameter {index} ({name}, ID {id}) of {from} is gone")),
        }
    }
    let hex = json["state"].as_str().ok_or("the fixture has no state")?;
    let state: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
    if !h.load_state(&state) {
        problems.push(format!("the state saved by {from} does not load"));
    } else {
        let mcp = crate::__private::reserved_id("ferritron.mcp");
        for p in params {
            let (id, name, saved) = (p["id"].as_u64().unwrap_or(0) as u32, p["name"].as_str().unwrap_or(""), p["value"].as_f64().unwrap_or(0.0));
            if id == mcp {
                continue; // never restored from a project: the user turns it on
            }
            let now = unsafe { abi::get_param::<P>(h.instance, id) };
            if (now - saved).abs() > 1e-6 {
                problems.push(format!("{name} (ID {id}) loads as {now}, was saved as {saved} by {from}"));
            }
        }
    }
    if problems.is_empty() { Ok(params.len()) } else { Err(problems.join("\n")) }
}

/// Whether the project file of the crate under test (`CARGO_MANIFEST_DIR`) lists the old class ID
/// `hex` (the bytes an older Ferritron gave hosts) under `[vst3] compatibility.replaces`, which
/// holds IDs as hosts showed them: on Windows hosts read those bytes in COM order (BC8).
fn replaced_in_project_file(hex: &str) -> bool {
    let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
    let Ok(old) = <[u8; 16]>::try_from(bytes.as_slice()) else { return false };
    let shown = if cfg!(windows) { ferritron_manifest::windows_tuid(&old) } else { old };
    let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") else { return false };
    let Ok(path) = ferritron_manifest::locate(std::path::Path::new(&dir)) else { return false };
    let Some(manifest) = std::fs::read_to_string(path).ok().and_then(|t| ferritron_manifest::parse(&t).ok()) else { return false };
    manifest.vst3_compat.is_some_and(|c| c.replaces.contains(&shown))
}

/// Block timings from [`time_blocks`], in microseconds.
#[derive(Clone, Copy, Debug)]
pub struct BlockTimes {
    pub mean_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}

impl std::fmt::Display for BlockTimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mean {:.2} us, p99 {:.2} us, max {:.2} us", self.mean_us, self.p99_us, self.max_us)
    }
}

/// Times `blocks` calls of `step` (which processes one block, `i` its number) after 20 calls of
/// warm-up: the benchmark scenarios of `docs/performance.md` that need a whole plugin (S3, S4, S5)
/// use it from `#[ignore]` tests run with `--release`.
pub fn time_blocks(blocks: usize, mut step: impl FnMut(usize)) -> BlockTimes {
    for i in 0..20 {
        step(i);
    }
    let mut times: Vec<f64> = (0..blocks)
        .map(|i| {
            let t = std::time::Instant::now();
            step(20 + i);
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    let mean_us = times.iter().sum::<f64>() / times.len().max(1) as f64;
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times.get(((times.len().saturating_sub(1)) as f64 * q) as usize).copied().unwrap_or(0.0);
    BlockTimes { mean_us, p99_us: at(0.99), max_us: at(1.0) }
}
