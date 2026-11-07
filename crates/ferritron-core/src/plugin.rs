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

/// Static plugin metadata, generated from the project file (`.frtx`).
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
}

/// Processing configuration, passed to [`Process::activate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setup {
    pub sample_rate: f64,
    pub max_block_size: usize,
    /// The host renders offline (bounce, export): `process` is called as
    /// fast as the plugin can go and may take as long as it needs, so wait
    /// for slow resources such as the GPU instead of dropping work.
    pub offline: bool,
    /// The quality to process at, as of this activation. It can change
    /// between activations (the user's switch, a host starting an export):
    /// [`Context::quality`] is always current, so a plugin that allocates
    /// per quality prepares for [`Quality::Best`] here.
    pub quality: Quality,
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
/// cost no CPU. Input, notes or parameter changes wake it at once. The
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
pub struct Context<'a, P> {
    pub params: &'a P,
    pub io: AudioIo<'a>,
    /// Notes in this block, sorted by offset. Always empty for effects.
    pub notes: &'a [NoteEvent],
    pub sample_rate: f64,
    /// The quality to process this block at (see [`Quality`]).
    pub quality: Quality,
    pub(crate) executor: Option<&'a Executor>,
}

impl<P> Context<'_, P> {
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

    fn activate(&mut self, setup: &Setup) {
        let _ = setup;
    }

    /// Clears internal state such as delay lines or filter memory.
    fn reset(&mut self) {}

    /// Delay the plugin adds, in samples, so the host can compensate (for
    /// example the latency of a GPU pipeline). Read after every `activate`.
    fn latency(&self) -> u32 {
        0
    }

    /// How long the output goes on after the input fell silent, so that
    /// Ferritron can stop processing silence (see [`Tail`]). Read after
    /// every block, so it may follow parameters such as a decay time.
    fn tail(&self) -> Tail {
        Tail::Auto
    }

    fn process(&mut self, ctx: &mut Context<'_, Self::Params>);
}
