//! ARA 2 (Audio Random Access) for Ferritron plugins: `[ara]` in the project
//! file.
//!
//! ARA hosts (DAWs such as Studio One, Cubase, Logic, REAPER) hand the plugin
//! whole audio files ("audio sources") and tell it where their parts
//! ("playback regions") play on the timeline. The plugin can read any part of
//! a source at any time, ahead of playback, which is what pitch and time
//! editors, restoration and analysis tools need.
//!
//! Ferritron keeps every source the host enables in memory and, for each
//! instance that renders playback, calls [`AraRender::render_region`] for the
//! parts of its regions inside each block. The result then goes through the
//! plugin's own `process` like any input:
//!
//! ```ignore
//! impl ferritron::ara::AraRender for Reverser {
//!     fn render_region(&mut self, params: &Params, source: &Source, start: i64, out: &mut [&mut [f32]]) {
//!         // Plays the source backwards: random access makes it trivial.
//!         for (c, out) in out.iter_mut().enumerate() {
//!             let input = source.channel(c.min(source.channels() - 1));
//!             for (i, s) in out.iter_mut().enumerate() {
//!                 *s = input[source.len() - 1 - (start as usize + i)];
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! The ARA SDK is the developer's own copy (`ARA_SDK_DIR`); Ferritron does
//! not ship it.

mod analysis;
mod stretch;

pub use analysis::{Note, detect_notes};
pub use stretch::granular;

use ferritron_core::__private::PluginMeta;
use ferritron_core::abi::{AraApi, extension_slot, params_of, plugin_mut};
use ferritron_core::Process;
use std::ffi::{CString, c_char, c_void};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::SeqCst;

/// Most channels a region is rendered with.
pub const MAX_CHANNELS: usize = 8;

/// An audio source the host gave the plugin: the whole file, in memory.
pub struct Source<'a> {
    channels: &'a [*const f32],
    len: usize,
    sample_rate: f64,
}

impl Source<'_> {
    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// Samples per channel.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// All samples of channel `c`.
    pub fn channel(&self, c: usize) -> &[f32] {
        // SAFETY: the bridge passes `channels()` pointers to `len` samples each,
        // valid for this call.
        unsafe { std::slice::from_raw_parts(self.channels[c], self.len) }
    }

    /// Copies the source from `start` into `out`, mixing down to mono or
    /// spreading mono when the channel counts differ.
    pub fn copy_to(&self, start: i64, out: &mut [&mut [f32]]) {
        let Ok(start) = usize::try_from(start) else { return };
        if self.channels.is_empty() || start >= self.len {
            return;
        }
        let n = out.first().map_or(0, |o| o.len()).min(self.len - start);
        if self.channels() == out.len() {
            for (c, out) in out.iter_mut().enumerate() {
                out[..n].copy_from_slice(&self.channel(c)[start..start + n]);
            }
            return;
        }
        let scale = if self.channels() > 1 { 1.0 / self.channels() as f32 } else { 1.0 };
        for i in 0..n {
            let mono = (0..self.channels()).map(|c| self.channel(c)[start + i]).sum::<f32>() * scale;
            for out in out.iter_mut() {
                out[i] = mono;
            }
        }
    }
}

/// How a plugin plays its ARA playback regions. The default plays the source
/// as it is; the plugin's `process` runs on the result either way.
pub trait AraRender: Process {
    /// Whether the plugin lets the host stretch regions in time (a region
    /// whose playback is longer or shorter than its audio). Hosts only offer
    /// stretching to plugins that say so; override [`render_stretched`]
    /// to stretch your own way, or set this to `false` for a plugin whose
    /// rendering cannot follow a stretched timeline.
    ///
    /// [`render_stretched`]: AraRender::render_stretched
    const TIME_STRETCH: bool = true;

    /// Whether [`analyze_notes`](AraRender::analyze_notes) is implemented. The
    /// host then asks for the notes of a source (and shows them, e.g. in its
    /// pitch editor).
    const ANALYZES_NOTES: bool = false;

    /// Fills `out` (silent on entry) with the sound of the source from sample
    /// `start` on. Called on the audio thread for each part of a playback
    /// region inside the block; overlapping regions add up.
    fn render_region(&mut self, params: &Self::Params, source: &Source<'_>, start: i64, out: &mut [&mut [f32]]) {
        let _ = params;
        source.copy_to(start, out);
    }

    /// Like [`render_region`](AraRender::render_region) for a region the host
    /// stretched in time: `ratio` source samples are played per output sample
    /// (0.5 is half speed). Output sample 0 of `out` plays the source at
    /// `source_pos` and is sample `region_offset` of the playback region, so a
    /// deterministic stretch can anchor itself to the region and give the same
    /// sound however the host splits the audio into blocks. The default is a
    /// granular stretch that keeps the pitch ([`granular`]).
    fn render_stretched(&mut self, params: &Self::Params, source: &Source<'_>, source_pos: f64, ratio: f64, region_offset: i64, out: &mut [&mut [f32]]) {
        let _ = params;
        granular(source, source_pos, ratio, region_offset, out);
    }

    /// The notes in a source. Not tied to an instance: the host may ask for
    /// them while no instance exists, and the analysis runs on a background
    /// thread. The default finds the notes of a monophonic source
    /// ([`detect_notes`]). Only called when
    /// [`ANALYZES_NOTES`](AraRender::ANALYZES_NOTES) is `true`.
    fn analyze_notes(source: &Source<'_>) -> Vec<Note> {
        detect_notes(source)
    }
}

#[repr(C)]
struct Config {
    factory_id: *const c_char,
    name: *const c_char,
    vendor: *const c_char,
    url: *const c_char,
    version: *const c_char,
    archive_id: *const c_char,
    stretch: bool,
    analyze: Option<AnalyzeCallback>,
}

/// One note handed to the bridge (`ARAContentNote` without the ARA types).
#[repr(C)]
struct CNote {
    frequency: f32,
    pitch: i32,
    volume: f32,
    start: f64,
    attack: f64,
    duration: f64,
    signal: f64,
}

type PushNote = unsafe extern "C" fn(*mut c_void, *const CNote);
type AnalyzeCallback = unsafe extern "C" fn(*const *const f32, u32, i64, f64, *mut c_void, PushNote);

/// (plugin, source channels, count, samples, rate, source position, stretch ratio (0 = none), position in the region, out, out channels, frames)
type RegionCallback = unsafe extern "C" fn(*mut c_void, *const *const f32, u32, i64, f64, f64, f64, i64, *const *mut f32, u32, u32);

unsafe extern "C" {
    fn ferritron_ara_factory(config: *const Config) -> *const c_void;
    fn ferritron_ara_new_extension() -> *mut c_void;
    fn ferritron_ara_delete_extension(extension: *mut c_void);
    fn ferritron_ara_bind(extension: *mut c_void, controller: *mut c_void, known: u32, assigned: u32) -> *const c_void;
    fn ferritron_ara_set_rendering(extension: *mut c_void, enable: bool, sample_rate: f64, channels: u32, max_frames: u32);
    fn ferritron_ara_render(
        extension: *mut c_void,
        channels: *const *mut f32,
        num_channels: u32,
        frames: u32,
        position: i64,
        playing: bool,
        callback: RegionCallback,
        ctx: *mut c_void,
    ) -> bool;
}

struct Table<P>(PhantomData<P>);

impl<P: AraRender + PluginMeta> Table<P> {
    const API: AraApi = AraApi {
        get_factory: get_factory::<P>,
        bind: bind::<P>,
        set_rendering: set_rendering::<P>,
        render: render::<P>,
        release: release::<P>,
    };
}

/// The `ferritron.ara.v1` table of plugin `P` (used by `#[ferritron::plugin]`).
pub fn api<P: AraRender + PluginMeta>() -> &'static AraApi {
    &Table::<P>::API
}

unsafe extern "C" fn get_factory<P: AraRender + PluginMeta>() -> *const c_void {
    // One plugin per library, so one description.
    static STRINGS: OnceLock<[CString; 6]> = OnceLock::new();
    let info = P::INFO;
    let strings = STRINGS.get_or_init(|| {
        let c = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
        [
            c(&format!("{}.ara.factory", info.id)),
            c(info.name),
            c(info.vendor),
            c(info.url),
            c(info.version),
            c(&format!("{}.ara.archive.v1", info.id)),
        ]
    });
    let config = Config {
        factory_id: strings[0].as_ptr(),
        name: strings[1].as_ptr(),
        vendor: strings[2].as_ptr(),
        url: strings[3].as_ptr(),
        version: strings[4].as_ptr(),
        archive_id: strings[5].as_ptr(),
        stretch: P::TIME_STRETCH,
        analyze: if P::ANALYZES_NOTES { Some(analyze::<P>) } else { None },
    };
    unsafe { ferritron_ara_factory(&config) }
}

/// The instance's ARA plug-in extension, created on first use.
unsafe fn extension<P: AraRender + PluginMeta>(core: *mut c_void, create: bool) -> *mut c_void {
    let slot = unsafe { extension_slot::<P>(core) };
    let mut extension = slot.load(SeqCst);
    if extension.is_null() && create {
        extension = unsafe { ferritron_ara_new_extension() };
        slot.store(extension, SeqCst);
    }
    extension
}

unsafe extern "C" fn bind<P: AraRender + PluginMeta>(core: *mut c_void, controller: *mut c_void, known: u32, assigned: u32) -> *const c_void {
    if core.is_null() || controller.is_null() {
        return std::ptr::null();
    }
    // The factory must exist before a host can have a document controller.
    unsafe { get_factory::<P>() };
    let extension = unsafe { extension::<P>(core, true) };
    if extension.is_null() {
        return std::ptr::null();
    }
    unsafe { ferritron_ara_bind(extension, controller, known, assigned) }
}

unsafe extern "C" fn set_rendering<P: AraRender + PluginMeta>(core: *mut c_void, enable: bool, sample_rate: f64, channels: u32, max_frames: u32) {
    let extension = unsafe { extension::<P>(core, false) };
    if !extension.is_null() {
        unsafe { ferritron_ara_set_rendering(extension, enable, sample_rate, channels, max_frames) };
    }
}

unsafe extern "C" fn render<P: AraRender + PluginMeta>(
    core: *mut c_void,
    channels: *const *mut f32,
    num_channels: u32,
    frames: u32,
    position: i64,
    playing: bool,
) -> bool {
    let extension = unsafe { extension::<P>(core, false) };
    if extension.is_null() || channels.is_null() {
        return false;
    }
    unsafe { ferritron_ara_render(extension, channels, num_channels, frames, position, playing, region::<P>, core) }
}

unsafe extern "C" fn region<P: AraRender + PluginMeta>(
    core: *mut c_void,
    source: *const *const f32,
    source_channels: u32,
    source_samples: i64,
    source_rate: f64,
    start: f64,
    ratio: f64,
    region_offset: i64,
    out: *const *mut f32,
    out_channels: u32,
    frames: u32,
) {
    let source_channels = (source_channels as usize).min(MAX_CHANNELS);
    let out_channels = (out_channels as usize).min(MAX_CHANNELS);
    let source = Source {
        channels: if source_channels == 0 { &[] } else { unsafe { std::slice::from_raw_parts(source, source_channels) } },
        len: usize::try_from(source_samples).unwrap_or(0),
        sample_rate: source_rate,
    };
    let mut outs: [&mut [f32]; MAX_CHANNELS] = Default::default();
    for (c, slot) in outs.iter_mut().enumerate().take(out_channels) {
        *slot = unsafe { std::slice::from_raw_parts_mut(*out.add(c), frames as usize) };
    }
    let plugin = unsafe { plugin_mut::<P>(core) };
    let params = unsafe { params_of::<P>(core) };
    // A panicking renderer leaves the region silent; the host keeps running.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ratio > 0.0 {
            plugin.render_stretched(params, &source, start, ratio, region_offset, &mut outs[..out_channels])
        } else {
            plugin.render_region(params, &source, start as i64, &mut outs[..out_channels])
        }
    }));
}

/// Runs on the bridge's analysis thread.
unsafe extern "C" fn analyze<P: AraRender + PluginMeta>(
    source: *const *const f32,
    source_channels: u32,
    source_samples: i64,
    source_rate: f64,
    ctx: *mut c_void,
    push: PushNote,
) {
    let count = (source_channels as usize).min(MAX_CHANNELS);
    let source = Source {
        channels: if count == 0 { &[] } else { unsafe { std::slice::from_raw_parts(source, count) } },
        len: usize::try_from(source_samples).unwrap_or(0),
        sample_rate: source_rate,
    };
    // A panicking analysis leaves the source without notes.
    let Ok(notes) = catch_unwind(AssertUnwindSafe(|| P::analyze_notes(&source))) else { return };
    for n in notes {
        let note = CNote {
            frequency: n.frequency,
            pitch: n.pitch.unwrap_or(i32::MIN),
            volume: n.volume,
            start: n.start,
            attack: n.attack,
            duration: n.duration,
            signal: n.signal,
        };
        unsafe { push(ctx, &note) };
    }
}

unsafe extern "C" fn release<P: AraRender + PluginMeta>(core: *mut c_void) {
    let extension = unsafe { extension_slot::<P>(core) }.swap(std::ptr::null_mut(), SeqCst);
    if !extension.is_null() {
        unsafe { ferritron_ara_delete_extension(extension) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_copy_and_convert_channels() {
        let (l, r) = ([1.0f32, 2.0, 3.0, 4.0], [3.0f32, 4.0, 5.0, 6.0]);
        let ptrs = [l.as_ptr(), r.as_ptr()];
        let source = Source { channels: &ptrs, len: 4, sample_rate: 48_000.0 };
        let (mut a, mut b) = ([0.0f32; 2], [0.0f32; 2]);
        source.copy_to(1, &mut [&mut a, &mut b]);
        assert_eq!((a, b), ([2.0, 3.0], [4.0, 5.0]));
        let mut mono = [0.0f32; 3];
        source.copy_to(2, &mut [&mut mono]);
        assert_eq!(mono, [4.0, 5.0, 0.0]); // mixed down; past the end stays silent
        source.copy_to(-1, &mut [&mut mono]);
        source.copy_to(9, &mut [&mut mono]);
    }
}
