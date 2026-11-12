use crate::abi::Executor;
use crate::{AudioIo, ParamSet};

/// What kind of plugin this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginKind {
    /// Stereo in, stereo out.
    Effect,
    /// Notes in, stereo out.
    Instrument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    On,
    Off,
}

/// A note expression a host sends per note (CLAP note expressions, VST3 Note Expression).
/// The numbers are CLAP's `CLAP_NOTE_EXPRESSION_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpressionKind {
    /// Linear gain, `0.0..=4.0` (1 = unchanged).
    Volume = 0,
    /// `0.0` left, `0.5` center, `1.0` right.
    Pan = 1,
    /// Semitones, `-120.0..=120.0`.
    Tuning = 2,
    /// `0.0..=1.0`.
    Vibrato = 3,
    /// `0.0..=1.0`.
    Expression = 4,
    /// `0.0..=1.0` (MPE "slide").
    Brightness = 5,
    /// `0.0..=1.0`.
    Pressure = 6,
}

impl ExpressionKind {
    pub fn from_raw(n: u32) -> Option<Self> {
        Some(match n {
            0 => Self::Volume,
            1 => Self::Pan,
            2 => Self::Tuning,
            3 => Self::Vibrato,
            4 => Self::Expression,
            5 => Self::Brightness,
            6 => Self::Pressure,
            _ => return None,
        })
    }
}

/// What a [`VoiceEvent`] says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceEventKind {
    /// Offset of parameter `index` (project-file position) for the note, in plain units: CLAP
    /// polyphonic modulation of a `poly = true` parameter. A new event replaces the old offset.
    Mod { index: usize, amount: f64 },
    /// A note expression.
    Expression { kind: ExpressionKind, value: f32 },
}

/// A host's per-note event ([`Context::voice_events`]). `note_id`, `key` and `channel` may each
/// be -1 (any); all three -1 means every note (CLAP's monophonic modulation).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceEvent {
    /// Sample offset inside `ctx.io`.
    pub offset: u32,
    pub note_id: i32,
    pub key: i8,
    pub channel: i8,
    pub kind: VoiceEventKind,
}

/// A note starting or ending inside the current block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEvent {
    /// Sample offset inside `ctx.io`.
    pub offset: u32,
    pub kind: NoteKind,
    /// MIDI key, 0..=127 (60 = middle C).
    pub key: u8,
    /// MIDI channel, 0..=15.
    pub channel: u8,
    /// 0.0..=1.0
    pub velocity: f32,
    /// Host note ID, or -1.
    pub note_id: i32,
}

/// Static plugin metadata, generated from the project file (`.ferritroject`).
#[derive(Debug)]
pub struct PluginInfo {
    /// Reverse-domain identifier, e.g. `com.example.gain`.
    pub id: &'static str,
    pub name: &'static str,
    pub vendor: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    pub email: &'static str,
    pub kind: PluginKind,
    /// Format-level class ID, derived from `id`.
    pub class_id: [u8; 16],
    /// `[isolation] mode` in the project file.
    pub isolation: crate::isolation::Isolation,
    /// `[isolation] share` / `pipeline`, for `Isolation::Process`.
    pub process: crate::isolation::ProcessOptions,
    /// VST3 sub-categories (`Fx|EQ|Analyzer`) from `[plugin] categories` or
    /// `[vst3] subcategories`; empty = `Fx` / `Instrument`.
    pub vst3_subcategories: &'static str,
    /// CLAP features, comma separated, from `[plugin] categories` or
    /// `[clap] features`; empty = `audio-effect,stereo` (instruments:
    /// `instrument,synthesizer,stereo`).
    pub clap_features: &'static str,
    /// VST 2 plugin ID (`[vst2] unique_id`); 0 = derived from `id`.
    pub vst2_id: u32,
    /// Channels, sidechain and MIDI input (`[audio]`, `[plugin] midi_input`).
    pub audio: AudioConfig,
}

/// The audio buses of a plugin, from the project file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioConfig {
    pub min_channels: u32,
    pub max_channels: u32,
    /// `Some(0)` = as many channels as the main bus; `Some(n)`; `None` = no sidechain.
    pub sidechain: Option<u32>,
    /// An effect that takes MIDI too.
    pub midi_input: bool,
    /// Layouts out of range pass through instead of being refused.
    pub passthrough: bool,
    /// VST3 controllers go to the host's own mapping instead of `ctx.midi`.
    pub host_cc: bool,
}

impl AudioConfig {
    /// Mono or stereo, no sidechain, no MIDI: what 1.3.0 plugins have.
    pub const DEFAULT: Self =
        Self { min_channels: 1, max_channels: 2, sidechain: None, midi_input: false, passthrough: false, host_cc: false };
}

/// Processing configuration, passed to [`Process::activate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setup {
    pub sample_rate: f64,
    pub max_block_size: usize,
    /// The host renders offline (bounce, export): `process` is called as
    /// fast as the plugin can go and may take as long as it needs, so wait
    /// for slow resources such as the GPU instead of dropping work.
    /// Under process isolation a block may take up to 30 s while offline (250 ms otherwise).
    pub offline: bool,
    /// The quality to process at, as of this activation. It can change
    /// between activations (the user's switch, a host starting an export):
    /// [`Context::quality`] is always current, so a plugin that allocates
    /// per quality prepares for [`Quality::Best`] here.
    pub quality: Quality,
    /// The speaker of each channel of the main bus, as of this activation.
    pub layout: crate::midi::SpeakerLayout,
    /// The latency the host has been told right now, in samples (what
    /// [`Process::latency`] returned at the last `activate`, or what
    /// [`Context::request_latency`] set since).
    pub latency: u32,
}

/// How much work the plugin should spend on its sound.
///
/// Ferritron picks [`Quality::Best`] when the host renders offline (export,
/// bounce, freeze) and [`Quality::Normal`] while the user plays and edits.
/// With `[quality]` in the project file the plugin also gets a "Quality"
/// switch (Auto / Normal / Best) the user can set by hand; Auto is the rule
/// above. What each level means (oversampling, filter length, voices) is the
/// plugin's choice; Ferritron itself processes the same either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum Quality {
    /// Playback and editing in real time: light enough for every track.
    #[default]
    Normal,
    /// Export: the best the plugin can do, however long it takes.
    Best,
}

impl Quality {
    pub fn is_best(self) -> bool {
        self == Self::Best
    }
}

/// How long a plugin's output goes on after its input fell silent, from
/// [`Process::tail`]. Ferritron stops calling `process` once that time has
/// passed and the input is still silent (the plugin "sleeps"): idle tracks
/// cost no CPU. Input (the sidechain too), notes or parameter changes wake it at once. The
/// length is also reported to hosts (VST3 tail samples, CLAP tail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// Ferritron decides: the plugin sleeps after its input and its output
    /// were both silent (below -120 dBFS) and no note was held for two
    /// seconds. Right for most effects and instruments; a plugin whose
    /// output may pause for longer and then continue on its own (a long
    /// delay, a looper) should say so with `Samples` or `Infinite`.
    Auto,
    /// The output dies away at most this many samples after the input
    /// stops (a reverb's decay, a delay's time times its repeats). The
    /// plugin's latency is added by Ferritron.
    Samples(u32),
    /// Never sleep: the plugin makes sound from silence (a generator, a
    /// test tone) or must see every block.
    Infinite,
}

/// Everything a plugin needs during one processing block.
#[non_exhaustive]
pub struct Context<'a, P> {
    pub(crate) params: &'a P,
    pub(crate) io: AudioIo<'a>,
    /// Notes in this block, sorted by offset. Always empty for effects.
    pub(crate) notes: &'a [NoteEvent],
    pub(crate) sample_rate: f64,
    /// The quality to process this block at (see [`Quality`]).
    pub(crate) quality: Quality,
    /// The host's tempo, time signature, play state and position at the
    /// start of this block. Every field is `None` / `false` when the host
    /// says nothing (see [`Transport`](crate::Transport)).
    pub(crate) transport: crate::Transport,
    /// MIDI of this block, sorted by offset: notes and every other message
    /// (controllers, pitch bend, pressure, program changes). Empty unless the
    /// plugin is an instrument or has `midi_input = true`. `ctx.notes` holds
    /// the notes alone.
    pub(crate) midi: &'a [crate::midi::MidiEvent],
    /// The sidechain input, if the plugin has one and the host connected it.
    pub(crate) sidechain: Option<crate::midi::Sidechain<'a>>,
    pub(crate) latency_cell: &'a std::sync::atomic::AtomicU32,
    pub(crate) main_requests: &'a std::sync::atomic::AtomicU32,
    pub(crate) executor: Option<&'a Executor>,
    pub(crate) instance: &'a crate::InstanceHandle,
    pub(crate) param_events: &'a [ParamEvent],
    pub(crate) voice_events: &'a [VoiceEvent],
    pub(crate) control_block: u32,
    /// Where this piece starts inside the host's block (see `Context::block_offset`).
    pub(crate) block_offset: u32,
    pub(crate) seen: &'a Seen,
    pub(crate) smooth: &'a crate::smooth::SmoothBank,
    pub(crate) out: &'a OutQueue,
    pub(crate) scratch: &'a Scratch,
}

impl<'a, P> Context<'a, P> {
    /// The plugin's parameters (`ctx.params().gain.value()`).
    pub fn params(&self) -> &'a P {
        self.params
    }

    /// The audio of this piece, in place.
    pub fn io(&mut self) -> &mut AudioIo<'a> {
        &mut self.io
    }

    /// Notes in this piece, sorted by offset. Always empty for effects.
    pub fn notes(&self) -> &'a [NoteEvent] {
        self.notes
    }

    /// The host's per-note modulation and note expressions in this piece, sorted by offset
    /// (CLAP `PARAM_MOD` / note expressions, VST3 Note Expression). Hand them to a
    /// [`VoiceMods`](crate::VoiceMods) with [`feed_voice`](crate::VoiceMods::feed_voice).
    pub fn voice_events(&self) -> &'a [VoiceEvent] {
        self.voice_events
    }

    /// MIDI of this piece, sorted by offset: notes and every other message.
    /// Empty unless the plugin is an instrument or has `midi_input = true`.
    pub fn midi(&self) -> &'a [crate::midi::MidiEvent] {
        self.midi
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The quality to process this piece at (see [`Quality`]).
    pub fn quality(&self) -> Quality {
        self.quality
    }

    /// The host's tempo, play state and position at the start of the block
    /// (see [`Transport`](crate::Transport)).
    pub fn transport(&self) -> &crate::Transport {
        &self.transport
    }

    /// Whether an editor is open (no analysis is worth making for an editor nobody sees).
    pub fn editor_open(&self) -> bool {
        self.instance.is_editor_open()
    }

    /// The sidechain input of this block, or `None` when the plugin has none
    /// or the host has not connected it (treat that as silence). The result
    /// does not borrow the context, so `ctx.io` stays usable.
    pub fn sidechain(&self) -> Option<crate::midi::Sidechain<'a>> {
        self.sidechain
    }
}

/// A parameter change inside the block (`[process] automation = "events"`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamEvent {
    /// Sample offset in the block.
    pub offset: u32,
    /// Index of the parameter in its `ParamSet`.
    pub index: u32,
    /// The new value in plain units.
    pub plain: f64,
}

impl<P> Context<'_, P> {
    /// Where this piece starts inside the host's block, in samples (0 unless Ferritron split the
    /// block for automation or smoothing). `transport()` is already moved to the piece's start.
    pub fn block_offset(&self) -> u32 {
        self.block_offset
    }

    /// The parameter changes of this block in offset order. Filled only with
    /// `[process] automation = "events"`; otherwise empty. The parameter
    /// values themselves are those from the start of the block.
    pub fn param_events(&self) -> &[ParamEvent] {
        self.param_events
    }

    /// The control period in samples (`[process] control_block`).
    pub fn control_block(&self) -> u32 {
        self.control_block
    }

    /// This instance (see [`InstanceHandle`](crate::InstanceHandle)). On the
    /// audio thread use only its lock-free parts: [`id`](crate::InstanceHandle::id),
    /// parameter values of other instances, [`revision`](crate::InstanceHandle::revision).
    pub fn instance(&self) -> &crate::InstanceHandle {
        self.instance
    }

    /// Asks the host's main thread to run the instance's
    /// [`on_notify`](crate::InstanceHandle::on_notify) handler with `code` (lock-free, no
    /// allocation): `ctx.instance().notify(code)`.
    pub fn notify(&self, code: u64) -> bool {
        self.instance.notify(code)
    }


    /// Tells the host that the latency is now `samples` (say, after the user
    /// switched to a linear-phase mode). Call it from `process`: it only
    /// stores a number and never allocates or locks. Ferritron then does what
    /// the format needs on the host's main thread: VST3 sends
    /// `restartComponent(kLatencyChanged)`; CLAP asks the host to restart the
    /// plugin and reports the new latency while it is deactivated; VST2 sends
    /// `ioChanged`. Until the host has taken the new value, the plugin
    /// keeps running at the old delay as far as the host compensates, so keep
    /// processing consistently. Also return the same number from
    /// [`Process::latency`], which is read at every `activate`.
    pub fn request_latency(&self, samples: u32) {
        use std::sync::atomic::Ordering;
        if self.latency_cell.swap(samples, Ordering::Relaxed) != samples {
            self.main_requests.fetch_or(crate::abi::REQUEST_LATENCY, Ordering::Release);
        }
    }

    /// Runs `f` on every item, spread over several cores when possible: the
    /// host's thread pool if the format offers one, otherwise Ferritron's
    /// workers (with `parallel = true` in the project file), otherwise one after
    /// another on this thread. Items must be independent, e.g. voices that
    /// render into their own buffers.
    ///
    /// Real-time safe: never allocates or blocks. While another instance uses
    /// the workers, the items run on this thread instead of waiting.
    pub fn parallel_for<T: Send>(&self, items: &mut [T], f: impl Fn(&mut T) + Sync) {
        struct Items<T>(*mut T);
        // SAFETY: each index is handed to exactly one task.
        unsafe impl<T: Send> Sync for Items<T> {}
        let base = Items(items.as_mut_ptr());
        let base = &base;
        crate::parallel::run(self.executor, items.len(), &|i| f(unsafe { &mut *base.0.add(i) }));
    }
}

/// The DSP part of a plugin.
///
/// `activate` and `reset` run off the audio thread and may allocate.
/// `process` runs on the audio thread and must not allocate, lock or block.
pub trait Process: Default + Send + 'static {
    /// Inserted automatically by `#[ferritron::plugin]`.
    type Params: ParamSet;

    /// Called once, right after the instance was created and before
    /// anything else, with the instance's handle: keep a clone to reach the
    /// instance's track, store, identity and editor messages later.
    fn init(&mut self, instance: &crate::InstanceHandle) {
        let _ = instance;
    }

    fn activate(&mut self, setup: &Setup) {
        let _ = setup;
    }

    /// Clears internal state such as delay lines or filter memory.
    /// Also called once after a fully bypassed span (the built-in bypass), before the plugin runs again.
    fn reset(&mut self) {}

    /// Delay the plugin adds, in samples, so the host can compensate (for
    /// example the latency of a GPU pipeline). Read after every `activate`.
    fn latency(&self) -> u32 {
        0
    }

    /// The version of what this plugin saves. Bump it when the meaning of the
    /// saved parameters changes; `migrate_state` then upgrades older projects.
    const STATE_VERSION: u32 = 1;

    /// How many voices an instrument plays at once, told to hosts that ask (CLAP
    /// `voice-info`, which hosts use to offer polyphonic modulation). 0 (the default): not
    /// told.
    const VOICES: u32 = 0;

    /// `true`: the plugin reports the end of each voice itself, with [`Context::voice_ended`]
    /// (for example when its release tail is over). `false` (the default): Ferritron reports it
    /// when the note-off arrives, as CLAP `NOTE_END`. Only meaningful with `VOICES > 0`.
    const REPORTS_VOICE_END: bool = false;

    /// Called when a project saved by an older `STATE_VERSION` (0 for state
    /// saved before 2.0 or by a plugin that never set one) is loaded, on the
    /// host's main thread, after the saved values are applied. Rewrite the
    /// parameters (`params.some.cell().set(..)`) and the `store` (the
    /// instance's saved key-value data) to what `to` expects. Parameters are
    /// matched by ID, so adding, removing or reordering them needs no migration.
    /// It is also called when the state is newer than the plugin (from > to: a project saved by a later version); leave such state alone unless you know better.
    fn migrate_state(from: u32, to: u32, params: &Self::Params, store: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        let _ = (from, to, params, store);
    }

    /// How much temporary memory `process` needs per piece, asked at every
    /// `activate`. Ferritron allocates it then; `Context::scratch` lends it
    /// out without allocating. Default: none.
    fn scratch(&self, setup: &Setup) -> ScratchSpec {
        let _ = setup;
        ScratchSpec::NONE
    }

    /// How long the output goes on after the input fell silent, so that
    /// Ferritron can stop processing silence (see [`Tail`]). Read after
    /// every block, so it may follow parameters such as a decay time.
    fn tail(&self) -> Tail {
        Tail::Auto
    }

    fn process(&mut self, ctx: &mut Context<'_, Self::Params>);

    // ---- Parameter display and mapping (1.3.1) ---------------------------
    //
    // These are associated functions (no `self`): hosts call them from any
    // thread. They are used only for parameters that ask for them in the
    // project file (`text = "custom"`, `scale = "custom"`).

    /// Writes the display text of `plain` (without the unit) for a parameter
    /// with `text = "custom"`. Return `false` to use Ferritron's display.
    /// Hosts, the editors and AI clients all show this text. Because hosts
    /// may append `unit`, leave `unit` out of the project file for such a
    /// parameter and write the unit here.
    fn format_param_text(param: crate::ParamRef, plain: f64, out: &mut dyn std::fmt::Write) -> bool {
        let _ = (param, plain, out);
        false
    }

    /// What the user typed for a parameter with `text = "custom"`: the plain
    /// value, [`crate::ParsedText::Reject`] (the value stays) or [`crate::ParsedText::Default`].
    fn parse_param_text(param: crate::ParamRef, text: &str) -> crate::ParsedText {
        let _ = (param, text);
        crate::ParsedText::Default
    }

    /// Plain value -> normalized `0..=1` for a parameter with `scale = "custom"`.
    /// Must be monotonic and the inverse of [`Process::param_denormalize`];
    /// must not allocate or lock.
    fn param_normalize(param: crate::ParamRef, plain: f64) -> f64 {
        let info = &<Self::Params as ParamSet>::infos()[param.index];
        match info.kind {
            crate::ParamKind::Float { min, max } if max > min => (plain - min) / (max - min),
            _ => 0.0,
        }
    }

    /// Normalized `0..=1` -> plain value for a parameter with `scale = "custom"`.
    fn param_denormalize(param: crate::ParamRef, normalized: f64) -> f64 {
        let info = &<Self::Params as ParamSet>::infos()[param.index];
        match info.kind {
            crate::ParamKind::Float { min, max } => min + normalized * (max - min),
            _ => normalized,
        }
    }
}

/// Which parameter versions the plugin has already looked at (audio thread
/// only). Refreshed after a `process` call, and only when the plugin asked
/// (`Context::changed` and friends), so plugins that do not ask pay nothing.
pub(crate) struct Seen(std::cell::UnsafeCell<SeenState>);

#[derive(Default)]
struct SeenState {
    versions: Vec<u32>,
    /// `ParamCell::shown` bits as of the previous call (smoothed values move without a new version).
    shown: Vec<u64>,
    /// (address of the cell, index), sorted by address.
    by_addr: Vec<(usize, u32)>,
    used: bool,
}

// SAFETY: touched by the audio thread, and by `activate` while it is not running.
unsafe impl Sync for Seen {}
unsafe impl Send for Seen {}

impl Seen {
    pub(crate) fn new() -> Self {
        Self(Default::default())
    }

    /// Heap bytes held (the tables of versions and addresses).
    pub(crate) fn heap_bytes(&self) -> usize {
        let st = self.state();
        st.versions.capacity() * size_of::<u32>() + st.by_addr.capacity() * size_of::<(usize, u32)>()
    }

    fn state(&self) -> &mut SeenState {
        // SAFETY: see the impls above.
        unsafe { &mut *self.0.get() }
    }

    /// (Re)builds the tables; every parameter then counts as changed once.
    pub(crate) fn prepare<P: ParamSet>(&self, params: &P) {
        let n = P::infos().len();
        let st = self.state();
        st.versions = (0..n).map(|i| params.cell(i).version().wrapping_sub(1)).collect();
        st.shown = (0..n).map(|i| params.cell(i).shown().to_bits()).collect();
        st.by_addr = (0..n).map(|i| (params.cell(i) as *const crate::ParamCell as usize, i as u32)).collect();
        st.by_addr.sort_unstable();
        st.used = false;
    }

    pub(crate) fn refresh<P: ParamSet>(&self, params: &P) {
        let st = self.state();
        if st.used {
            for (i, v) in st.versions.iter_mut().enumerate() {
                *v = params.cell(i).version();
                st.shown[i] = params.cell(i).shown().to_bits();
            }
        }
    }

    pub(crate) fn index(&self, cell: &crate::ParamCell) -> Option<usize> {
        let addr = cell as *const crate::ParamCell as usize;
        let st = self.state();
        st.by_addr.binary_search_by_key(&addr, |e| e.0).ok().map(|i| st.by_addr[i].1 as usize)
    }
}

impl<P: ParamSet> Context<'_, P> {
    /// Whether `p` changed since the previous `process` call (the first call
    /// reports every parameter as changed). Costs nothing for plugins that
    /// never ask. Writing the value it already has is not a change.
    /// For a smoothed parameter, a piece in which the smoothed value moved counts as changed.
    pub fn changed(&self, p: &impl crate::Param) -> bool {
        let cell = p.param_cell();
        let Some(i) = self.seen.index(cell) else { return false };
        let st = self.seen.state();
        st.used = true;
        st.versions[i] != cell.version() || st.shown[i] != cell.shown().to_bits()
    }

    /// Whether any parameter changed since the previous `process` call.
    pub fn any_changed(&self) -> bool {
        self.changed_indices().next().is_some()
    }

    /// The indices (see [`ParamSet`]) of the parameters that changed since
    /// the previous `process` call. Does not allocate.
    pub fn changed_indices(&self) -> impl Iterator<Item = usize> + '_ {
        let st = self.seen.state();
        st.used = true;
        let params = self.params;
        (0..st.versions.len()).filter(move |&i| st.versions[i] != params.cell(i).version() || st.shown[i] != params.cell(i).shown().to_bits())
    }
}

impl<P: ParamSet> Context<'_, P> {
    fn smooth_index(&self, p: &impl crate::Param) -> Option<usize> {
        self.seen.index(p.param_cell())
    }

    /// Writes the per-sample values of `p` for this piece into `out` (its
    /// length is the piece's `frames`). Returns `true` while the value is
    /// moving; otherwise `out` is filled with the constant value and the
    /// result is `false`. Never allocates.
    pub fn ramp(&self, p: &crate::FloatParam, out: &mut [f32]) -> bool {
        match self.smooth_index(p) {
            Some(i) => self.smooth.ramp(i, p.cell().shown(), out),
            None => {
                out.fill(p.value());
                false
            }
        }
    }

    /// The start value and the per-sample increment of a moving *linear*
    /// ramp of `p`, for plugins that expand it themselves (SIMD).
    pub fn ramp_linear(&self, p: &crate::FloatParam) -> Option<(f32, f32)> {
        self.smooth.ramp_linear(self.smooth_index(p)?)
    }

    /// Whether `p` is still moving towards its target.
    pub fn is_smoothing(&self, p: &crate::FloatParam) -> bool {
        self.smooth_index(p).is_some_and(|i| self.smooth.is_smoothing(i))
    }
}

/// Events the plugin produces during a block (`Context::midi_out`, `set_param`), written
/// straight into the caller's `out_events` buffer (audio thread only). The instance holds
/// no buffer of its own: without a caller buffer, or once it is full, events are dropped
/// and counted.
pub(crate) struct OutQueue {
    dst: std::cell::Cell<*mut crate::abi::Event>,
    capacity: std::cell::Cell<usize>,
    len: std::cell::Cell<usize>,
    dropped: std::sync::atomic::AtomicU64,
}

// SAFETY: touched by the audio thread only.
unsafe impl Sync for OutQueue {}
unsafe impl Send for OutQueue {}

impl OutQueue {
    pub(crate) fn new() -> Self {
        Self {
            dst: std::cell::Cell::new(std::ptr::null_mut()),
            capacity: std::cell::Cell::new(0),
            len: std::cell::Cell::new(0),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The block's destination (null: none). The caller keeps `dst` valid until `finish`.
    pub(crate) fn begin(&self, dst: *mut crate::abi::Event, capacity: usize) {
        self.dst.set(dst);
        self.capacity.set(if dst.is_null() { 0 } else { capacity });
        self.len.set(0);
    }

    /// Ends the block: how many events were written.
    pub(crate) fn finish(&self) -> usize {
        self.dst.set(std::ptr::null_mut());
        self.capacity.set(0);
        self.len.replace(0)
    }

    pub(crate) fn push(&self, event: crate::abi::Event) -> bool {
        let n = self.len.get();
        if n >= self.capacity.get() {
            self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        // SAFETY: `dst` holds `capacity` events while the block runs (`begin`).
        unsafe { self.dst.get().add(n).write(event) };
        self.len.set(n + 1);
        true
    }

    /// Events dropped since the instance was created (no caller buffer, or no room in it).
    #[allow(dead_code)]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl<P: ParamSet> Context<'_, P> {
    /// Changes `p` from the audio thread, as the user turning a knob would:
    /// the value is set now and the host is told (it records the move as
    /// automation where the format allows). Costs no allocation; if the
    /// block's queue is full the host is not told (the value still changes).
    pub fn set_param(&self, p: &impl crate::Param, plain: f64) {
        let cell = p.param_cell();
        let Some(index) = self.seen.index(cell) else { return };
        let info = &P::infos()[index];
        let plain = info.sanitize(plain);
        cell.set(plain);
        self.instance.0.bump_params();
        // Host offsets are inside the block: the piece's start is added (see `block_offset`).
        self.out.push(crate::abi::Event::param(self.block_offset, info.id, info.normalize(plain)));
    }

    /// Sends a MIDI message out of the plugin at `offset` samples into this
    /// piece (hosts that take MIDI from effects or instruments get it).
    pub fn midi_out(&self, offset: u32, channel: u8, kind: crate::MidiKind) {
        use crate::MidiKind::*;
        let b = |v: f32| (v.clamp(0.0, 1.0) * 127.0).round() as u8;
        let (status, d1, d2) = match kind {
            NoteOn { key, velocity } => (0x90, key.min(127), b(velocity).max(1)),
            NoteOff { key, velocity } => (0x80, key.min(127), b(velocity)),
            Cc { number, value } => (0xB0, number.min(127), value.min(127)),
            PitchBend(v) => {
                let n = (((v.clamp(-1.0, 1.0) + 1.0) * 8192.0).round() as i32).clamp(0, 16383);
                (0xE0, (n & 0x7F) as u8, (n >> 7) as u8)
            }
            ChannelPressure(v) => (0xD0, b(v), 0),
            PolyPressure { key, pressure } => (0xA0, key.min(127), b(pressure)),
            ProgramChange(p) => (0xC0, p.min(127), 0),
        };
        // Host offsets are inside the block: the piece's start is added (see `block_offset`).
        self.out.push(crate::abi::Event::midi(self.block_offset + offset, status, channel.min(15), d1, d2));
    }

    /// Tells the host that the voice of a note has ended, `offset` samples into this piece (CLAP
    /// `NOTE_END`). For plugins with `REPORTS_VOICE_END = true`, when the release tail is over.
    /// Hosts get the offset inside the block, so the piece's start is added.
    pub fn voice_ended(&self, offset: u32, voice: crate::VoiceId, note_id: i32) {
        self.out.push(crate::abi::Event::note_end(self.block_offset + offset, note_id, i16::from(voice.channel), i16::from(voice.key)));
    }
}

/// How much scratch memory a plugin asks for ([`Process::scratch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScratchSpec {
    pub bytes: usize,
}

impl ScratchSpec {
    pub const NONE: Self = Self { bytes: 0 };

    /// Room for `n` values of type `T`, plus alignment slack for each of up to 8 borrows.
    pub const fn of<T>(n: usize) -> Self {
        Self { bytes: n * std::mem::size_of::<T>() + 64 }
    }

    /// Both specs together.
    pub const fn plus(self, other: Self) -> Self {
        Self { bytes: self.bytes + other.bytes }
    }
}

/// Plain numbers that may live in scratch memory (any bit pattern is valid).
pub trait ScratchItem: Copy + sealed::Sealed {}
impl ScratchItem for f32 {}
impl ScratchItem for f64 {}
impl ScratchItem for i32 {}
impl ScratchItem for u32 {}
impl ScratchItem for i64 {}
impl ScratchItem for u64 {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for i32 {}
    impl Sealed for u32 {}
    impl Sealed for i64 {}
    impl Sealed for u64 {}
}

#[repr(align(64))]
#[derive(Clone, Copy)]
struct Chunk([u8; 64]);

/// A bump allocator over memory allocated in `activate`; rewound before each
/// piece. Audio thread only.
pub(crate) struct Scratch {
    chunks: std::cell::UnsafeCell<Vec<Chunk>>,
    used: std::cell::Cell<usize>,
}

// SAFETY: touched by the audio thread, and by `activate` while it is not running.
unsafe impl Sync for Scratch {}
unsafe impl Send for Scratch {}

impl Scratch {
    pub(crate) fn new() -> Self {
        Self { chunks: std::cell::UnsafeCell::new(Vec::new()), used: std::cell::Cell::new(0) }
    }

    /// Heap bytes held (the scratch memory the plugin asked for).
    pub(crate) fn heap_bytes(&self) -> usize {
        // SAFETY: read while not processing.
        unsafe { &*self.chunks.get() }.capacity() * 64
    }

    /// Allocates `bytes` (the only allocation; `activate` only).
    pub(crate) fn prepare(&self, bytes: usize) -> usize {
        // SAFETY: not processing.
        let chunks = unsafe { &mut *self.chunks.get() };
        *chunks = vec![Chunk([0; 64]); bytes.div_ceil(64)];
        self.used.set(0);
        chunks.len() * 64
    }

    pub(crate) fn rewind(&self) {
        self.used.set(0);
    }

    fn take<T: ScratchItem>(&self, n: usize) -> Option<&mut [T]> {
        // SAFETY: audio thread only; regions handed out never overlap (bump pointer).
        let chunks = unsafe { &mut *self.chunks.get() };
        let base = chunks.as_mut_ptr() as *mut u8;
        let total = chunks.len() * 64;
        let align = std::mem::align_of::<T>();
        let start = self.used.get().next_multiple_of(align);
        let end = start.checked_add(n.checked_mul(std::mem::size_of::<T>())?)?;
        if end > total {
            return None;
        }
        self.used.set(end);
        Some(unsafe { std::slice::from_raw_parts_mut(base.add(start) as *mut T, n) })
    }
}

impl<P> Context<'_, P> {
    /// Lends `n` numbers of scratch memory for this piece (contents are
    /// unspecified; they may hold the last piece's values). Returns `None`
    /// when what [`Process::scratch`] asked for is used up. Never allocates.
    /// Borrows rewind at the start of each piece.
    #[allow(clippy::mut_from_ref)]
    pub fn scratch<T: ScratchItem>(&self, n: usize) -> Option<&mut [T]> {
        self.scratch.take(n)
    }
}
