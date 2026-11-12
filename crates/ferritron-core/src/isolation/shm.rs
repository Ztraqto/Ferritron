//! Shared memory between the host-side client and the child process.

use crate::abi::Event;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const MAX_FRAMES: usize = 8192;
pub const MAX_CHANNELS: usize = 32;
pub const MAX_EVENTS: usize = 1024;
/// Output events the child returns per piece (the same as the VST3 and CLAP shims).
pub const MAX_OUT: usize = 256;
pub const MAX_PARAMS: usize = 1024;
/// How long the host waits for one block before giving up on the child.
pub const PROCESS_TIMEOUT_MS: u32 = 250;
/// How long the host waits for one block while it renders offline (export, bounce): the plugin
/// may legitimately take longer there, so it is not given up on quickly.
pub const OFFLINE_TIMEOUT_MS: u32 = 30_000;
/// How long the host waits for the first block after an activation: the child may still be
/// building its resources (GPU, caches) then.
pub const FIRST_BLOCK_TIMEOUT_MS: u32 = 5_000;

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
    /// Work the child's plugin asked of the host's main thread (`abi::REQUEST_*`).
    pub requests: AtomicU32,
    /// The speaker of each channel of the main bus (`Speaker::id`), set before activation.
    pub layout_len: u32,
    pub layout: [u32; MAX_CHANNELS],

    /// Normalized parameter values, by parameter index (bypass last). The
    /// client writes them; the child applies them before each block.
    pub params: [AtomicU64; MAX_PARAMS],

    // One block: written by the client, processed in place by the child.
    pub frames: u32,
    pub channels: u32,
    pub num_events: u32,
    pub ok: u32,
    pub events: [Event; MAX_EVENTS],
    /// The host's transport for this block.
    pub transport: crate::transport::TransportC,
    /// Sidechain channels of this block (0 = not connected).
    pub sc_channels: u32,

    /// The audio region (see `audio_name`): `audio_channels` channels of the
    /// main bus, then as many of the sidechain, `audio_frames` samples each.
    /// The client makes a new one on the main thread when an activation needs
    /// more room, and bumps `audio_generation` before `setup_generation`.
    pub audio_generation: AtomicU32,
    pub audio_frames: AtomicU32,
    pub audio_channels: AtomicU32,

    /// The host renders offline (`ferritron.render.v1`; 0 = online). The client writes it, and the
    /// child passes changes to the plugin before a block. Appended in 2.0.0; zeroed memory is online.
    pub offline: AtomicU32,

    /// The output events of the last piece the child processed (`ctx.set_param`, `ctx.midi_out`,
    /// voice ends), with offsets from that piece's start. Written by the child, read by the client
    /// after `done`. Appended in 2.0.0 (audit fix f3); zeroed memory means none.
    pub out_count: u32,
    pub out: [Event; MAX_OUT],
}

/// The size in bytes of an audio region.
pub fn audio_len(frames: usize, channels: usize) -> usize {
    2 * frames * channels * size_of::<f32>()
}

/// The name of an instance's audio region of one generation.
pub fn audio_name(id: &str, generation: u32) -> String {
    format!("ferritron-isolation-{id}-audio-{generation}")
}

/// A mapped audio region and its shape.
pub struct AudioRegion {
    pub memory: super::os::Memory,
    pub frames: usize,
    pub channels: usize,
}

impl AudioRegion {
    /// Channel `ch` of the main bus.
    pub fn main(&self, ch: usize) -> *mut f32 {
        debug_assert!(ch < self.channels);
        unsafe { (self.memory.ptr() as *mut f32).add(ch * self.frames) }
    }

    /// Channel `ch` of the sidechain.
    pub fn sidechain(&self, ch: usize) -> *mut f32 {
        debug_assert!(ch < self.channels);
        unsafe { (self.memory.ptr() as *mut f32).add((self.channels + ch) * self.frames) }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// T6: an instance's shared memory follows its setup. 1.4.0 mapped about
    /// 2.1 MB per instance (32 channels × 8192 samples, twice); stereo at 512
    /// samples now needs the control region and 8 KB of audio.
    #[test]
    fn shared_memory_follows_the_setup() {
        let control = size_of::<Shared>();
        let audio = audio_len(512, 2);
        assert_eq!(audio, 8 * 1024);
        let old = control + 2 * MAX_FRAMES * MAX_CHANNELS * size_of::<f32>();
        assert!((control + audio) * 4 <= old, "{} bytes now, {old} before", control + audio);
        println!("isolation memory per instance: {} bytes now (control {control} + audio {audio}), {old} in 1.4.0", control + audio);
    }
}
