//! Shared memory between the host-side client and the child process.

use crate::abi::Event;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const MAX_FRAMES: usize = 8192;
pub const MAX_CHANNELS: usize = 8;
pub const MAX_EVENTS: usize = 1024;
pub const MAX_PARAMS: usize = 1024;
/// How long the host waits for one block before giving up on the child.
pub const PROCESS_TIMEOUT_MS: u32 = 250;

/// The shared region of one plugin instance. Lives as long as the client.
#[repr(C)]
pub struct Shared {
    /// Child has created its instance and waits for blocks.
    pub ready: AtomicU32,
    /// Client asks the child to exit.
    pub quit: AtomicU32,

    /// Activation: the child (re)activates when `setup_generation` changes.
    pub setup_generation: AtomicU32,
    pub sample_rate: AtomicU64,
    pub max_block: AtomicU32,
    /// The plugin's latency in samples, reported by the child after activating.
    pub latency: AtomicU32,

    /// Normalized parameter values, by parameter index (bypass last). The
    /// client writes them; the child applies them before each block.
    pub params: [AtomicU64; MAX_PARAMS],

    // One block: written by the client, processed in place by the child.
    pub frames: u32,
    pub channels: u32,
    pub num_events: u32,
    pub ok: u32,
    pub events: [Event; MAX_EVENTS],
    pub audio: [[f32; MAX_FRAMES]; MAX_CHANNELS],
}

pub const MAX_COMMANDS: usize = 64;
pub const COMMAND_LEN: usize = 64;

/// The control region of a group: one child process serving several
/// instances. The host queues instance IDs to serve; the child starts a
/// thread for each, which ends when that instance's `quit` is set.
#[repr(C)]
pub struct Control {
    /// The child runs and takes commands.
    pub ready: AtomicU32,
    /// The child exits.
    pub quit: AtomicU32,
    /// Commands: the host writes at `head`, the child reads at `tail`.
    pub head: AtomicU32,
    pub tail: AtomicU32,
    pub commands: [std::cell::UnsafeCell<[u8; COMMAND_LEN]>; MAX_COMMANDS],
}

/// Names of a group's control region and signals (see `os`).
pub fn group_names(group: &str) -> (String, String, String) {
    let n = |suffix: &str| format!("ferritron-isolation-group-{group}-{suffix}");
    (n("memory"), n("command"), n("ready"))
}

/// Names of an instance's region and signals.
pub fn names(id: &str) -> (String, String, String, String) {
    let n = |suffix: &str| format!("ferritron-isolation-{id}-{suffix}");
    (n("memory"), n("request"), n("done"), n("ready"))
}
