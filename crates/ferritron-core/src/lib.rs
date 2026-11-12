//! Format-independent plugin API of Ferritron.
//!
//! Plugin code only ever talks to the types in this crate. Format adapters
//! (VST3, CLAP, AU, ...) translate between these types and the host.

pub mod abi;
#[cfg(any(windows, unix))]
pub mod companion;
mod audio;
mod describe;
pub mod display;
#[cfg(any(windows, unix))]
pub mod directlink;
pub mod i18n;
#[cfg(any(windows, unix))]
pub mod share;
#[cfg(any(windows, unix))]
mod shmem;
#[cfg(any(windows, unix))]
pub mod mcp;
pub mod instance;
pub mod notify;
pub mod watch;
pub mod isolation;
#[cfg(any(windows, unix))]
mod module;
mod parallel;
mod params;
mod plugin;
pub mod presets;
pub mod settings;
pub mod reload;
mod bypass;
mod smooth;
mod state;
pub mod stream;
pub mod testing;
#[cfg(feature = "alloc-check")]
pub mod alloc_check;
mod ui;
pub mod util;
pub mod voice;
pub mod worker;

#[doc(hidden)]
pub mod __private;

pub mod midi;
pub use midi::{MidiEvent, MidiKind, Sidechain, Speaker, SpeakerLayout};
pub mod transport;
pub use transport::Transport;
pub use audio::{AudioIo, ChannelsMut, MAX_CHANNELS};
pub use instance::{InstanceHandle, InstanceId, PluginLocation, Store, TrackInfo, TrackKind, instances, instances_of, instances_revision};
pub use watch::{InstanceEvent, SingletonView, Watch, editors, editors_of, instances_watch};
pub use presets::Preset;
pub use settings::{Settings, settings, settings_for};
pub use smooth::Smoothing;
pub use voice::{Expression, VoiceId, VoiceMods};
pub use stream::{HasStreams, NoStreams, StreamEntry, StreamHub, StreamKind, StreamReader, StreamWriter, TapReader, TapWriter};
pub use worker::{ChangedParams, Handoff, Worker, WorkerContext};
pub use params::{Param, BoolParam, EnumParam, FloatParam, IntParam, ParamCell, ParamInfo, ParamKind, ParamOptions, ParamRef, ParamScale, ParamSet, ParamTextFns, ParsedText};
pub use plugin::{ExpressionKind, VoiceEvent, VoiceEventKind, ScratchItem, ScratchSpec, AudioConfig, Context, NoteEvent, ParamEvent, NoteKind, PluginInfo, PluginKind, Process, Quality, Setup, Tail};
