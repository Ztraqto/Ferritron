//! Format-independent plugin API of Ferritron.
//!
//! Plugin code only ever talks to the types in this crate. Format adapters
//! (VST3, CLAP, AU, ...) translate between these types and the host.

pub mod abi;
#[cfg(any(windows, unix))]
pub mod companion;
mod audio;
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
pub mod isolation;
#[cfg(any(windows, unix))]
mod module;
mod parallel;
mod params;
mod plugin;
pub mod reload;
mod state;
pub mod testing;
mod ui;
pub mod util;

#[doc(hidden)]
pub mod __private;

pub use audio::AudioIo;
pub use instance::{InstanceHandle, InstanceId, Store, TrackInfo, instances, instances_of, instances_revision};
pub use params::{BoolParam, EnumParam, FloatParam, IntParam, ParamCell, ParamInfo, ParamKind, ParamOptions, ParamSet};
pub use plugin::{Context, NoteEvent, NoteKind, PluginInfo, PluginKind, Process, Quality, Setup, Tail};
