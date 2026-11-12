//! GPU compute backend of Ferritron (design doc, chapter 9).
//!
//! Building blocks:
//!
//! - [`iir`]: linear IIR filter banks by parallel scan (9.4);
//! - [`Convolver`], below;
//! - [`nodes`]: both as nodes of a `ferritron_graph::Graph`, placed on the
//!   CPU or the GPU by measurement (9.2).
//!
//! [`Convolver`] is a long convolution (reverb impulse
//! responses, cabinet IRs) split by latency (9.5):
//!
//! - the **head**, the first `D·G` taps, is convolved directly on the CPU in
//!   the current block, so the result has zero latency;
//! - the **tail**, everything after, is convolved on the GPU with uniformly
//!   partitioned FFT convolution (overlap-save, partition and grid size `G`).
//!   The tail for grid block `m` only needs input up to block `m - D`, and
//!   `(D - 1)·G ≥ max_block`, so the GPU always gets at least one full host
//!   block of time, whatever block sizes (up to `max_block`) the host sends.
//!
//! The audio thread never calls the GPU API. A dedicated thread owns the GPU
//! work; the audio thread only exchanges samples with it through a
//! non-blocking `try_lock`. When the GPU misses a deadline, that grid block
//! gets the head only, and [`Convolver::missed_blocks`] counts it.
//!
//! Offline rendering ([`Mode::Offline`], design doc 9.9) has no deadlines:
//! hosts call `process` as fast as it returns. There the convolver waits for
//! the GPU instead, so a bounce is always complete.

mod batch;
pub mod iir;
pub mod kernel;
pub mod nodes;
pub mod room;
pub use iir::{BatchJob, Biquad, CpuIirBank, IirBank, Lane, RealtimeIirBank};
pub use kernel::{GpuKernel, Kernel, KernelArgs, KernelFn, KernelNode, KernelState};
pub use nodes::{ConvolverNode, IirNode, MatrixMixerNode};
/// The node graph, re-exported so plugins need one dependency.
pub use ferritron_graph as graph;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use batch::{Batcher, Registry};
use std::time::{Duration, Instant};

/// Longest the GPU may take to answer one round (or, offline, one tail)
/// before it counts as lost. Real-time callers never wait for it.
const GPU_WAIT: Duration = Duration::from_secs(2);

/// After a failed attempt to open the GPU again (or to build a convolver on
/// it), the next attempt waits this long.
const REBUILD_RETRY: Duration = Duration::from_secs(5);

/// Largest grid size: the FFT (2G points) must fit in workgroup memory.
const MAX_GRID: usize = 1024;
/// Partitions per parallel chunk of the spectral sum (`CHUNK` in the shader).
const CHUNK: usize = 16;

const SHADER: &str = r#"
struct Params {
    src_base: u32,    // forward: first source sample
    src_mask: u32,    // forward: source index mask (ring) or 0xffffffff
    src_count: u32,   // forward: source samples from src_base on
    dst_offset: u32,  // forward: first complex output element
    n: u32,           // FFT size (2G)
    logn: u32,
    g: u32,           // partition / grid size
    partitions: u32,  // tail partitions
    slots: u32,       // length of the input spectrum delay line
    newest: u32,      // slot of the newest input spectrum
    chunks: u32,      // partition chunks summed in parallel (CHUNK each)
    window: u32,      // forward: samples per transform; the rest is zero padding
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> h: array<vec2<f32>>;
@group(0) @binding(4) var<storage, read> fdl: array<vec2<f32>>;
@group(0) @binding(5) var<storage, read_write> spec: array<vec2<f32>>;
@group(0) @binding(6) var<storage, read_write> out: array<f32>;

var<workgroup> re: array<f32, 2048>;
var<workgroup> im: array<f32, 2048>;

fn bitrev(i: u32) -> u32 {
    return reverseBits(i) >> (32u - p.logn);
}

// In-place radix-2 FFT of re/im, whose inputs are in bit-reversed order.
fn fft(lid: u32) {
    for (var s = 1u; s <= p.logn; s++) {
        let half = 1u << (s - 1u);
        for (var t = lid; t < (p.n >> 1u); t += 256u) {
            let k = t & (half - 1u);
            let i = ((t >> (s - 1u)) << s) + k;
            let j = i + half;
            let angle = -3.141592653589793 * f32(k) / f32(half);
            let w = vec2<f32>(cos(angle), sin(angle));
            let xr = re[j] * w.x - im[j] * w.y;
            let xi = re[j] * w.y + im[j] * w.x;
            re[j] = re[i] - xr;
            im[j] = im[i] - xi;
            re[i] = re[i] + xr;
            im[i] = im[i] + xi;
        }
        workgroupBarrier();
    }
}

// Spectrum of `window` real samples, zero padded to n. Workgroup w
// transforms the samples from src_base + w·g on (one partition each).
@compute @workgroup_size(256)
fn forward(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let first = wg.x * p.g;
    for (var i = lid; i < p.n; i += 256u) {
        var x = 0.0;
        if (i < p.window && first + i < p.src_count) {
            x = src[(p.src_base + first + i) & p.src_mask];
        }
        re[bitrev(i)] = x;
        im[bitrev(i)] = 0.0;
    }
    workgroupBarrier();
    fft(lid);
    for (var i = lid; i < p.n; i += 256u) {
        dst[p.dst_offset + wg.x * p.n + i] = vec2<f32>(re[i], im[i]);
    }
}

const CHUNK: u32 = 16u;

// Per frequency bin b and chunk c: sum over the chunk's partitions j of
// H_j * X_(newest - j). Chunks run in parallel; `inverse` adds them up.
@compute @workgroup_size(256)
fn accumulate(@builtin(global_invocation_id) id: vec3<u32>) {
    let b = id.x;
    let c = id.y;
    if (b >= p.n) {
        return;
    }
    var acc = vec2<f32>(0.0, 0.0);
    let first = c * CHUNK;
    var slot = (p.newest + p.slots - first) % p.slots;
    for (var j = first; j < min(first + CHUNK, p.partitions); j++) {
        let a = h[j * p.n + b];
        let x = fdl[slot * p.n + b];
        acc += vec2<f32>(a.x * x.x - a.y * x.y, a.x * x.y + a.y * x.x);
        slot = select(slot - 1u, p.slots - 1u, slot == 0u);
    }
    spec[c * p.n + b] = acc;
}

// Inverse FFT (by conjugation); overlap-save keeps the last g samples.
@compute @workgroup_size(256)
fn inverse(@builtin(local_invocation_index) lid: u32) {
    for (var i = lid; i < p.n; i += 256u) {
        var c = vec2<f32>(0.0, 0.0);
        for (var k = 0u; k < p.chunks; k++) {
            c += spec[k * p.n + i];
        }
        re[bitrev(i)] = c.x;
        im[bitrev(i)] = -c.y;
    }
    workgroupBarrier();
    fft(lid);
    for (var i = lid + p.g; i < p.n; i += 256u) {
        out[i - p.g] = re[i] / f32(p.n);
    }
}
"#;

/// How a GPU component meets the host's timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Never wait on the audio thread; late GPU work is skipped and counted.
    #[default]
    Realtime,
    /// Offline rendering (`Setup::offline`): wait for the GPU, skip nothing.
    Offline,
}

impl Mode {
    /// `Mode::Offline` when the host renders offline.
    pub fn from_offline(offline: bool) -> Self {
        if offline { Mode::Offline } else { Mode::Realtime }
    }
}

/// Samples handed from the audio thread to the GPU thread, and results back.
struct Exchange {
    /// New input samples not yet uploaded, starting at absolute `pending_start`.
    pending: Vec<f32>,
    pending_start: u64,
    /// Number of grid blocks whose input is complete.
    completed: u64,
    /// Tail output by grid block, in slot `block % results.len()`.
    results: Vec<(u64, Vec<f32>)>,
    /// The GPU's input stream starts again at this window (a grid block):
    /// it forgets its history, and results of earlier windows are dropped.
    restart: Option<u64>,
    stop: bool,
}

struct Shared {
    exchange: Mutex<Exchange>,
    /// Signalled when a tail result is stored (offline mode waits on it).
    done: Condvar,
    /// The GPU failed (device lost, a panic, or no answer within
    /// [`GPU_WAIT`]). The convolver then passes its input through, until the
    /// GPU thread has built it again on a device.
    lost: AtomicBool,
    last_gpu_micros: AtomicU64,
    /// Times the GPU thread built this convolver again. The audio thread
    /// starts its input stream again when this changes (G1).
    rebuilds: AtomicU64,
    /// Generation of the device the convolver's GPU state is on.
    device_generation: AtomicU64,
    /// Set by the audio thread once when a call is longer than `max_block`;
    /// the GPU thread prints the warning and clears it.
    oversized: AtomicBool,
}

/// A long convolution with its tail on the GPU. See the crate docs.
///
/// If the GPU is lost (or stops answering), the convolver passes its input
/// through unchanged ([`device_lost`](Self::device_lost)) while the GPU
/// thread builds it again on a new device; then its tail comes back, with
/// the input stream restarted from the current position (counted by
/// [`restarts`](Self::restarts)). Building it again needs a device that can
/// be opened: until one can, it stays lost.
pub struct Convolver {
    head: Vec<f32>,
    /// Input history (head taps, and input not yet handed to the GPU thread),
    /// a ring of power-of-two size.
    history: Vec<f32>,
    pos: u64,
    /// Input before this position has been handed to the GPU thread.
    sent: u64,
    max_block: usize,
    grid: u64,
    /// Tail delay in grid blocks (`D`).
    delay: u64,
    /// First grid block that has a tail (earlier ones have no tail input yet).
    first_tail_block: u64,
    /// The audio thread's copy of the tail of grid block `tail_block`.
    tail: Vec<f32>,
    tail_block: Option<u64>,
    tail_valid: bool,
    /// The GPU must start its input stream again; no tail until it has.
    restart_due: bool,
    /// Times the input stream started again (see [`restarts`](Self::restarts)).
    restarts: AtomicU64,
    has_tail: bool,
    offline: bool,
    shared: Arc<Shared>,
    /// Registered with the GPU thread shared by every convolver (none
    /// without a tail).
    slot: Option<(Arc<ConvSlot>, Arc<Batcher<ConvSlot>>)>,
    misses: AtomicU64,
    adapter: String,
    /// `Shared::rebuilds` as the audio thread last saw it.
    rebuilds_seen: u64,
    /// The loss was announced to the GPU thread (once per loss).
    lost_announced: bool,
    /// A call longer than `max_block` was seen and reported to the GPU thread.
    oversize_reported: bool,
}

impl Convolver {
    /// Prepares a real-time convolver for blocks of up to `max_block`
    /// samples (the size may change every block). Call off the audio thread:
    /// it opens the GPU and transforms the impulse response.
    pub fn new(ir: &[f32], max_block: usize) -> Result<Self, String> {
        Self::with_mode(ir, max_block, Mode::Realtime)
    }

    /// Like [`new`](Self::new), for real-time use or offline rendering.
    pub fn with_mode(ir: &[f32], max_block: usize, mode: Mode) -> Result<Self, String> {
        let max_block = max_block.max(1);
        // The largest power of two not above max_block keeps the CPU head
        // (D·G taps) short: D·G < 3·max_block.
        let grid = (1usize << max_block.ilog2()).clamp(64, MAX_GRID);
        let delay = 1 + max_block.div_ceil(grid); // D
        let split = (delay * grid).min(ir.len());
        let tail = &ir[split..];
        let history_len = (split + max_block * 4).next_power_of_two();
        // Results the GPU may finish before the audio thread reads them.
        let results = 2 * (delay + max_block.div_ceil(grid)) + 4;
        let shared = Arc::new(Shared {
            exchange: Mutex::new(Exchange {
                pending: Vec::with_capacity(max_block * 16),
                pending_start: 0,
                completed: 0,
                results: (0..results).map(|_| (u64::MAX, vec![0.0; grid])).collect(),
                restart: None,
                stop: false,
            }),
            done: Condvar::new(),
            lost: AtomicBool::new(false),
            last_gpu_micros: AtomicU64::new(0),
            rebuilds: AtomicU64::new(0),
            device_generation: AtomicU64::new(0),
            oversized: AtomicBool::new(false),
        });
        let (slot, adapter) = if tail.is_empty() {
            (None, String::from("(no tail: CPU only)"))
        } else {
            let gpu = Gpu::new(tail, grid, delay, max_block)?;
            let adapter = gpu.ctx.adapter.clone();
            shared.device_generation.store(gpu.ctx.generation, Relaxed);
            let slot = Arc::new(ConvSlot {
                shared: shared.clone(),
                work: Mutex::new(ConvWork {
                    result: vec![0.0; grid],
                    samples: Vec::with_capacity(max_block * 16),
                    next: 0,
                    completed: 0,
                    gpu,
                    // A copy of the tail, to build the GPU state again after a loss.
                    ir: Arc::from(tail),
                    grid,
                    delay,
                    max_block,
                    retry_at: None,
                }),
            });
            let batcher = CONVOLVERS.register("ferritron-gpu-convolution", run_convolutions, slot.clone())?;
            (Some((slot, batcher)), adapter)
        };
        Ok(Self {
            head: ir[..split].to_vec(),
            history: vec![0.0; history_len],
            pos: 0,
            sent: 0,
            max_block,
            grid: grid as u64,
            delay: delay as u64,
            first_tail_block: delay as u64,
            tail: vec![0.0; grid],
            tail_block: None,
            tail_valid: false,
            restart_due: false,
            restarts: AtomicU64::new(0),
            has_tail: slot.is_some(),
            offline: mode == Mode::Offline,
            shared,
            slot,
            misses: AtomicU64::new(0),
            adapter,
            rebuilds_seen: 0,
            lost_announced: false,
            oversize_reported: false,
        })
    }

    /// The GPU in use.
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Grid blocks whose tail was not ready in time (they got the head only).
    pub fn missed_blocks(&self) -> u64 {
        self.misses.load(Relaxed)
    }

    /// GPU time of the most recent tail computation.
    pub fn last_gpu_micros(&self) -> u64 {
        self.shared.last_gpu_micros.load(Relaxed)
    }

    /// Times the GPU fell too far behind and its input stream started again
    /// from the current position (G1). Each restart silences the tail for
    /// about `delay` grid blocks (a few milliseconds per restart) and the
    /// tail then resumes from silence before the restart point. A host or
    /// plugin that cares can notify its editor when this grows.
    pub fn restarts(&self) -> u64 {
        self.restarts.load(Relaxed)
    }

    /// True while the GPU is lost (or stopped answering) and this convolver
    /// has not been built again yet: it passes its input through unchanged.
    /// The GPU thread then tries to build it again on a new device, every
    /// [`REBUILD_RETRY`] at most, and the tail comes back (see
    /// [`device_generation`](Self::device_generation) and
    /// [`rebuilds`](Self::rebuilds)).
    pub fn device_lost(&self) -> bool {
        self.shared.lost.load(Acquire)
    }

    /// Times the GPU state was built again after a loss.
    pub fn rebuilds(&self) -> u64 {
        self.shared.rebuilds.load(Acquire)
    }

    /// Generation of the GPU device the convolver runs on (it goes up each
    /// time a new device is opened after a loss). 0 without a tail.
    pub fn device_generation(&self) -> u64 {
        self.shared.device_generation.load(Acquire)
    }

    /// Hands input not yet sent to the GPU thread. In real time it never
    /// waits: if the lock is busy, the input stays in `history` for the next
    /// block (and a due restart waits for it too).
    fn send_input(&mut self) {
        let mut ex = if self.offline {
            self.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner)
        } else {
            match self.shared.exchange.try_lock() {
                Ok(ex) => ex,
                Err(_) => return,
            }
        };
        let mask = self.history.len() - 1;
        let unsent = (self.pos - self.sent) as usize;
        // Offline waits for the GPU instead of dropping input, so only real time overflows.
        let overflow = !self.offline && ex.pending.len() + unsent > ex.pending.capacity();
        if self.restart_due || overflow {
            // The GPU fell too far behind (or lost input): drop the backlog and
            // start its input stream again at the next window, from here on.
            let window = self.pos.div_ceil(self.grid);
            ex.pending.clear();
            ex.pending_start = self.pos;
            ex.restart = Some(window);
            self.sent = self.pos;
            self.restart_due = false;
            self.first_tail_block = window + self.delay;
            self.tail_block = None;
            self.tail_valid = false;
            self.restarts.fetch_add(1, Relaxed);
        } else {
            if ex.pending.is_empty() {
                ex.pending_start = self.sent;
            }
            for t in self.sent..self.pos {
                ex.pending.push(self.history[t as usize & mask]);
            }
            self.sent = self.pos;
        }
        ex.completed = self.pos / self.grid;
        drop(ex);
        if let Some((_, batcher)) = &self.slot {
            batcher.notify();
        }
    }

    /// Fetches the tail of grid block `g` from the GPU thread. Waits only
    /// in offline mode.
    fn fetch_tail(&mut self, g: u64) {
        if !self.has_tail || g < self.first_tail_block || self.restart_due {
            self.tail_block = Some(g);
            self.tail_valid = false;
            return;
        }
        let mut ex = if self.offline {
            let ex = self.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
            let ready = |ex: &mut Exchange| {
                let n = ex.results.len();
                ex.results[g as usize % n].0 != g && !ex.stop && !self.shared.lost.load(Relaxed)
            };
            let (ex, timeout) = self.shared.done.wait_timeout_while(ex, GPU_WAIT, ready).unwrap_or_else(PoisonError::into_inner);
            if timeout.timed_out() {
                // No answer in time: the GPU counts as lost (G3).
                self.shared.lost.store(true, Relaxed);
                self.tail_block = Some(g);
                self.tail_valid = false;
                return;
            }
            ex
        } else {
            match self.shared.exchange.try_lock() {
                Ok(ex) => ex,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                // The GPU thread holds the lock for a moment: try again at the next sample.
                Err(std::sync::TryLockError::WouldBlock) => return,
            }
        };
        self.tail_block = Some(g);
        self.tail_valid = false;
        let n = ex.results.len();
        let slot = &mut ex.results[g as usize % n];
        if slot.0 == g {
            std::mem::swap(&mut self.tail, &mut slot.1);
            self.tail_valid = true;
        }
        if !self.tail_valid {
            self.misses.fetch_add(1, Relaxed);
        }
    }

    /// Convolves `block` in place. Real-time safe: no allocation, never waits.
    /// Blocks longer than `max_block` are convolved in pieces of `max_block`:
    /// the whole block is always processed.
    pub fn process(&mut self, block: &mut [f32]) {
        if block.len() > self.max_block && !self.oversize_reported {
            // Once per convolver: the GPU thread prints the warning (the audio thread never prints).
            self.oversize_reported = true;
            self.shared.oversized.store(true, Release);
        }
        for piece in block.chunks_mut(self.max_block) {
            self.process_piece(piece);
        }
    }

    /// One piece of at most `max_block` samples.
    fn process_piece(&mut self, block: &mut [f32]) {
        let n = block.len();
        let mask = self.history.len() - 1;
        if self.shared.lost.load(Acquire) {
            // The GPU is gone: the input passes through unchanged. The history
            // and the position keep running, so the stream can start again
            // from here when the GPU thread has built this convolver anew.
            for (i, &x) in block.iter().enumerate() {
                self.history[(self.pos as usize + i) & mask] = x;
            }
            self.pos += n as u64;
            self.sent = self.pos;
            self.tail_block = None;
            self.tail_valid = false;
            if self.has_tail && !self.lost_announced {
                // Once per loss: wake the GPU thread so it tries to build us again.
                self.lost_announced = true;
                if let Some((_, batcher)) = &self.slot {
                    batcher.notify();
                }
            }
            return;
        }
        self.lost_announced = false;
        let rebuilds = self.shared.rebuilds.load(Acquire);
        if rebuilds != self.rebuilds_seen {
            // Built again on a device: its input stream starts again from here (G1).
            self.rebuilds_seen = rebuilds;
            self.restart_due = true;
        }
        for (i, &x) in block.iter().enumerate() {
            self.history[(self.pos as usize + i) & mask] = x;
        }
        for (i, out) in block.iter_mut().enumerate() {
            let t = self.pos + i as u64;
            let g = t / self.grid;
            if self.tail_block != Some(g) {
                self.fetch_tail(g);
            }
            // Head on the CPU: zero latency.
            let mut acc = 0.0;
            for (k, &h) in self.head.iter().enumerate() {
                if k as u64 > t {
                    break;
                }
                acc += h * self.history[(t as usize - k) & mask];
            }
            if self.tail_valid {
                acc += self.tail[(t % self.grid) as usize];
            }
            *out = acc;
        }
        self.pos += n as u64;

        if self.has_tail {
            if self.pos - self.sent > (self.history.len() - self.head.len()) as u64 {
                // Unsent input was overwritten: the GPU would see a gap, so it starts again.
                self.restart_due = true;
            }
            self.send_input();
        }
    }
}

impl Drop for Convolver {
    fn drop(&mut self) {
        self.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner).stop = true;
        self.shared.done.notify_all();
        if let Some((slot, batcher)) = self.slot.take() {
            CONVOLVERS.unregister(&slot, &batcher);
        }
    }
}

/// A convolver as the shared GPU thread sees it.
struct ConvSlot {
    shared: Arc<Shared>,
    work: Mutex<ConvWork>,
}

struct ConvWork {
    gpu: Gpu,
    samples: Vec<f32>,
    /// Next input window to transform, and windows whose input is complete.
    next: u64,
    completed: u64,
    result: Vec<f32>,
    /// The tail (the impulse response after the head), to build `gpu` again.
    ir: Arc<[f32]>,
    grid: usize,
    delay: usize,
    max_block: usize,
    /// A failed rebuild is not tried again before this.
    retry_at: Option<std::time::Instant>,
}

/// The GPU thread shared by every convolver of the process (see `batch`):
/// each round it encodes one window of every convolver that has one due
/// into a single submission, and reads all their results back with one wait.
static CONVOLVERS: Registry<ConvSlot> = Registry::new();

/// Number of convolvers sharing the GPU thread (for tests and diagnostics).
pub fn convolvers_on_gpu_thread() -> usize {
    CONVOLVERS.len()
}

/// Test hook: the GPU thread waits on this before each round, so a test can
/// hold it back and make the audio side fall behind.
#[cfg(test)]
static GPU_GATE: Mutex<()> = Mutex::new(());

fn run_convolutions(batcher: &Batcher<ConvSlot>) {
    loop {
        // A lost convolver is tried again every REBUILD_RETRY; otherwise the
        // thread sleeps until a convolver notifies it.
        let lost_now = batcher.slots().iter().any(|s| s.shared.lost.load(Acquire));
        let limit = if lost_now { REBUILD_RETRY } else { Duration::MAX };
        match batcher.wait_within(limit) {
            Some(true) | None => {}
            Some(false) => return,
        }
        #[cfg(test)]
        drop(GPU_GATE.lock().unwrap_or_else(PoisonError::into_inner));
        let slots = batcher.slots();
        for slot in &slots {
            if slot.shared.oversized.swap(false, Acquire) {
                eprintln!("ferritron: a convolution was called with a block longer than its max_block; the GPU cannot keep up with it in real time, so some tail blocks are missed (see missed_blocks())");
            }
        }
        let mut works: Vec<_> = slots.iter().map(|s| s.work.lock().unwrap_or_else(PoisonError::into_inner)).collect();
        // Build lost convolvers again (on a new device if the old one is lost).
        for (slot, work) in slots.iter().zip(works.iter_mut()) {
            // A convolver whose device was replaced under it counts as lost.
            if work.gpu.ctx.generation != GENERATION.load(Acquire) {
                slot.shared.lost.store(true, Release);
            }
            if !slot.shared.lost.load(Acquire) || work.retry_at.is_some_and(|t| std::time::Instant::now() < t) {
                continue;
            }
            if let Err(why) = rebuild(slot, work) {
                eprintln!("ferritron: convolution waits for the GPU ({why}); it is tried again in {} s", REBUILD_RETRY.as_secs());
                work.retry_at = Some(std::time::Instant::now() + REBUILD_RETRY);
            }
        }
        // Take every convolver's new input, and its restart if one is due.
        for (slot, work) in slots.iter().zip(works.iter_mut()) {
            let (start, completed, restart) = {
                let mut ex = slot.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
                let restart = ex.restart.take();
                if restart.is_some() {
                    // Results of the old stream are void.
                    ex.results.iter_mut().for_each(|r| r.0 = u64::MAX);
                }
                work.samples.clear();
                work.samples.extend_from_slice(&ex.pending);
                ex.pending.clear();
                (ex.pending_start, ex.completed, restart)
            };
            if let Some(window) = restart {
                work.gpu.reset_stream();
                work.next = window;
            }
            if !work.samples.is_empty() {
                work.gpu.upload(start, &work.samples);
            }
            work.completed = completed;
        }
        // Rounds: one window of every convolver that has one due, per submission.
        loop {
            let due: Vec<usize> = (0..works.len()).filter(|&i| !slots[i].shared.lost.load(Relaxed) && works[i].next < works[i].completed).collect();
            let Some(&first) = due.first() else { break };
            let begun = Instant::now();
            let ctx = works[first].gpu.ctx.clone();
            // The device answers, or it is lost: a panic (a wgpu call that
            // failed), or no answer within GPU_WAIT.
            let round = catch_unwind(AssertUnwindSafe(|| {
                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                for &i in &due {
                    let k = works[i].next;
                    works[i].gpu.encode(k, &mut encoder);
                }
                ctx.queue.submit([encoder.finish()]);
                for &i in &due {
                    works[i].gpu.staging.map_async(wgpu::MapMode::Read, .., |_| {});
                }
                ctx.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(GPU_WAIT) })
            }));
            if !matches!(round, Ok(Ok(_))) {
                let why = match round {
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "a GPU call panicked".into(),
                    Ok(Ok(_)) => unreachable!(),
                };
                eprintln!("ferritron: the GPU is lost for convolution ({why}); the convolvers pass their input through");
                lose(&ctx, &slots);
                break;
            }
            let micros = begun.elapsed().as_micros() as u64;
            for &i in &due {
                let work = &mut *works[i];
                work.gpu.read(&mut work.result);
                let target = work.next + work.gpu.delay;
                let shared = &slots[i].shared;
                shared.last_gpu_micros.store(micros, Relaxed);
                let mut ex = shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
                let n = ex.results.len();
                let slot = &mut ex.results[target as usize % n];
                slot.0 = target;
                std::mem::swap(&mut slot.1, &mut work.result);
                shared.done.notify_all();
                work.next += 1;
            }
        }
    }
}

/// The device failed: every convolver on it is lost. The GPU thread builds
/// them again on a new device (see [`rebuild`]); the audio side passes its
/// input through until then.
fn lose(ctx: &Context, slots: &[Arc<ConvSlot>]) {
    ctx.lost.store(true, Relaxed);
    for slot in slots {
        // Under the lock, so an offline waiter cannot miss the notification.
        let _ex = slot.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
        slot.shared.lost.store(true, Release);
        slot.shared.done.notify_all();
    }
}

/// Builds a convolver's GPU state again, on the current device (a new one
/// if the old one was lost). The audio side starts its input stream again
/// once it sees the new `rebuilds` count (the impulse response is uploaded
/// from the copy the convolver keeps). Runs on the GPU thread only.
fn rebuild(slot: &ConvSlot, work: &mut ConvWork) -> Result<(), String> {
    let gpu = Gpu::new(&work.ir[..], work.grid, work.delay, work.max_block)?;
    work.gpu = gpu;
    work.samples.clear();
    // Nothing is due until the audio side restarts the stream (it does so at once).
    work.next = u64::MAX;
    work.completed = 0;
    work.retry_at = None;
    slot.shared.device_generation.store(work.gpu.ctx.generation, Relaxed);
    slot.shared.rebuilds.fetch_add(1, Release);
    slot.shared.lost.store(false, Release);
    Ok(())
}

/// The process's GPU device and pipelines, shared by every convolver
/// (design doc 2.3): a device per instance costs time and memory, and many
/// devices at once strain drivers.
pub(crate) struct Context {
    pub(crate) adapter: String,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    /// Set by the device's error and loss callbacks, or by a failed round.
    /// Once set, this device is not used again: [`context`] opens a new one.
    pub(crate) lost: Arc<AtomicBool>,
    /// 1 for the first device this process opened, then up by one each time
    /// a new one replaces a lost device (V1).
    pub(crate) generation: u64,
    forward: wgpu::ComputePipeline,
    accumulate: wgpu::ComputePipeline,
    inverse: wgpu::ComputePipeline,
    pub(crate) scan: wgpu::ComputePipeline,
    pub(crate) mix: wgpu::ComputePipeline,
    pub(crate) sum: wgpu::ComputePipeline,
    /// Impulse-response spectra by (content hash, partition size, length):
    /// convolvers with the same IR share them (resource sharing, Phase 7).
    spectra: Mutex<HashMap<(u64, usize, usize), std::sync::Weak<wgpu::Buffer>>>,
}

/// The process's device (see [`context`]).
struct ContextSlot {
    /// The device in use: a lost one stays here until a new one replaces it.
    current: Option<Arc<Context>>,
    /// Why no device could be opened, if none ever was (cached for good).
    never: Option<String>,
    /// Generation of the newest device.
    generation: u64,
    /// After a failed attempt to open a device again: no attempt before this.
    retry_at: Option<std::time::Instant>,
}

static CONTEXT: Mutex<ContextSlot> = Mutex::new(ContextSlot { current: None, never: None, generation: 0, retry_at: None });

/// Devices replaced after a loss, kept until the process ends (see [`context`]).
static RETIRED: Mutex<Vec<Arc<Context>>> = Mutex::new(Vec::new());

/// Generation of the newest device. The GPU thread compares it with the
/// generation each convolver runs on: a convolver on an older device is
/// built again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The GPU to use. Only the native backend is started first (Vulkan;
/// Metal on Apple): starting every backend wgpu has (DX12, OpenGL) takes
/// hundreds of milliseconds, and Ferritron's kernels share Vulkan's device
/// anyway. The others are tried only when it has no hardware GPU.
/// `WGPU_BACKEND` (e.g. `dx12`) chooses instead.
fn adapter() -> Result<wgpu::Adapter, String> {
    let options = wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() };
    let open = |backends: wgpu::Backends| {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = backends;
        pollster::block_on(wgpu::Instance::new(descriptor).request_adapter(&options)).map_err(|e| format!("no GPU adapter: {e}"))
    };
    if let Some(chosen) = wgpu::Backends::from_env() {
        return open(chosen);
    }
    match open(wgpu::Backends::VULKAN | wgpu::Backends::METAL) {
        Ok(adapter) if adapter.get_info().device_type != wgpu::DeviceType::Cpu => Ok(adapter),
        _ => open(wgpu::Backends::all()),
    }
}

/// The process's GPU device. After a device is lost, the next call opens a
/// new one (generation up by one), at most once per [`REBUILD_RETRY`] while
/// opening fails. A failure before any device was ever opened is final.
pub(crate) fn context() -> Result<Arc<Context>, String> {
    let mut slot = CONTEXT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(ctx) = &slot.current {
        if !ctx.lost.load(Relaxed) {
            return Ok(ctx.clone());
        }
    }
    if let Some(why) = &slot.never {
        return Err(why.clone());
    }
    if slot.retry_at.is_some_and(|t| std::time::Instant::now() < t) {
        return Err("the GPU was lost and a new device is not ready yet".into());
    }
    let generation = slot.generation + 1;
    match open_context(generation) {
        Ok(ctx) => {
            slot.generation = generation;
            slot.retry_at = None;
            if let Some(old) = slot.current.replace(ctx.clone()) {
                // The Vulkan side may still hold the old device's raw handle
                // (its `adopt` keeps a device it does not see as lost): keep
                // the old one alive, so that handle never dangles.
                RETIRED.lock().unwrap_or_else(PoisonError::into_inner).push(old);
            }
            GENERATION.store(generation, Release);
            Ok(ctx)
        }
        Err(why) => {
            if slot.current.is_none() {
                slot.never = Some(why.clone());
            } else {
                slot.retry_at = Some(std::time::Instant::now() + REBUILD_RETRY);
            }
            Err(why)
        }
    }
}

/// The shared Vulkan device for persistent kernels (f7 1-2). [`context`] is
/// the one place that reopens the shared device after a loss, so it is
/// called first; `Gpu::get` then returns the device in use instead of
/// opening a device of its own.
pub(crate) fn shared_vulkan() -> Result<Arc<ferritron_vulkan::Gpu>, String> {
    context()?;
    ferritron_vulkan::Gpu::get()
}

/// Opens a GPU device and its pipelines (see [`context`]).
fn open_context(generation: u64) -> Result<Arc<Context>, String> {
    {
        {
            let started = std::time::Instant::now();
            let trace = |what: &str| {
                if std::env::var_os("FERRITRON_TRACE").is_some() {
                    eprintln!("ferritron: gpu {what} ({:.1} ms)", started.elapsed().as_secs_f64() * 1e3);
                }
            };
            let adapter = adapter()?;
            let info = adapter.get_info();
            trace("adapter");
            // A software rasterizer (llvmpipe/lavapipe, WARP) is slower than
            // Ferritron's own CPU paths: treat it as no GPU at all.
            if info.device_type == wgpu::DeviceType::Cpu {
                return Err(format!("only a software GPU is available ({})", info.name));
            }
            let name = format!("{} ({:?})", info.name, info.backend);
            // Large banks and offline blocks need big buffers: allow what the GPU can do.
            // Ray queries (RT cores) for room acoustics, where the GPU has them.
            let required_features = adapter.features() & wgpu::Features::EXPERIMENTAL_RAY_QUERY;
            let descriptor = wgpu::DeviceDescriptor {
                required_limits: adapter.limits(),
                required_features,
                // SAFETY: only `room` uses an experimental feature (ray
                // queries), and only through wgpu's documented API.
                experimental_features: unsafe { wgpu::ExperimentalFeatures::enabled() },
                ..Default::default()
            };
            let (device, queue) = match unsafe { shared_vulkan_device(&adapter, &descriptor, &info.name) } {
                Some(opened) => opened,
                None => pollster::block_on(adapter.request_device(&descriptor)).map_err(|e| format!("cannot open the GPU: {e}"))?,
            };
            trace("device");
            // With handlers, wgpu reports errors and losses here instead of
            // panicking on an error nobody receives (V1).
            let lost = Arc::new(AtomicBool::new(false));
            let flag = lost.clone();
            device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
                eprintln!("ferritron: GPU error: {error}");
                flag.store(true, Relaxed);
            }));
            let flag = lost.clone();
            device.set_device_lost_callback(move |reason, message| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    eprintln!("ferritron: GPU device lost: {message}");
                    flag.store(true, Relaxed);
                }
            });
            let shader = |label: &str, source: &str| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(source.into()) })
            };
            let convolution = shader("ferritron-convolution", SHADER);
            let iir = shader("ferritron-iir", iir::SHADER);
            let pipeline = |module: &wgpu::ShaderModule, entry: &str| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: None,
                    module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            };
            Ok(Arc::new(Context {
                lost,
                generation,
                forward: pipeline(&convolution, "forward"),
                accumulate: pipeline(&convolution, "accumulate"),
                inverse: pipeline(&convolution, "inverse"),
                spectra: Mutex::new(HashMap::new()),
                scan: pipeline(&iir, "scan"),
                mix: pipeline(&iir, "mix"),
                sum: pipeline(&iir, "sum"),
                adapter: name,
                device,
                queue,
            }))
            .inspect(|_| trace("ready"))
        }
    }
}

/// On Vulkan, opens wgpu's device ourselves with a second queue on the same
/// family, and hands that queue to `ferritron-vulkan` for persistent
/// kernels: one device for the process, so its work and wgpu's run side by
/// side instead of in turns. `None` (and wgpu opens the device as usual)
/// on other backends or if anything does not fit.
///
/// # Safety
/// Follows wgpu-hal's `device_from_raw` contract: the device is created
/// from this adapter with the extensions and features wgpu asks for.
#[cfg(not(target_vendor = "apple"))]
unsafe fn shared_vulkan_device(adapter: &wgpu::Adapter, descriptor: &wgpu::DeviceDescriptor, name: &str) -> Option<(wgpu::Device, wgpu::Queue)> {
    use wgpu::hal::api::Vulkan;
    use ferritron_vulkan::ash::vk;
    if adapter.get_info().backend != wgpu::Backend::Vulkan || std::env::var_os("FERRITRON_NO_SHARED_VULKAN").is_some() {
        return None;
    }
    let hal = unsafe { adapter.as_hal::<Vulkan>() }?;
    let instance = hal.shared_instance().raw_instance().clone();
    let physical = hal.raw_physical_device();
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    // The family wgpu would use (graphics and compute), with room for two queues.
    let family = families.iter().position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE))? as u32;
    if families[family as usize].queue_count < 2 {
        return None;
    }
    let features = descriptor.required_features;
    let mut extensions = hal.required_device_extensions(features);
    // Tensor cores for ferritron-vulkan: cooperative matrices and 16-bit
    // floats, enabled in Vulkan only (wgpu does not use them).
    let available = unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
    let has = |name: &std::ffi::CStr| available.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
    let coop = has(ferritron_vulkan::ash::khr::cooperative_matrix::NAME) && adapter.features().contains(wgpu::Features::SHADER_F16);
    let mut vulkan_features = features;
    if coop {
        extensions.push(ferritron_vulkan::ash::khr::cooperative_matrix::NAME);
        vulkan_features |= wgpu::Features::SHADER_F16 | wgpu::Features::EXPERIMENTAL_COOPERATIVE_MATRIX;
    }
    let mut physical_features = hal.physical_device_features(&extensions, vulkan_features);
    let priorities = [1.0f32, 1.0];
    let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priorities)];
    let names: Vec<*const std::ffi::c_char> = extensions.iter().map(|e| e.as_ptr()).collect();
    let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&names);
    let info = physical_features.add_to_device_create(info);
    let raw = unsafe { instance.create_device(physical, &info, None) }.ok()?;
    // wgpu must not destroy the device: ferritron-vulkan uses it too, for
    // the life of the process.
    let open = unsafe {
        hal.device_from_raw(raw.clone(), Some(Box::new(|| {})), &extensions, features, &descriptor.required_limits, &descriptor.memory_hints, family, 0)
    }
    .ok()?;
    drop(hal);
    let opened = unsafe { adapter.create_device_from_hal(open, descriptor) }.ok()?;
    let native = ferritron_vulkan::Features { cooperative_matrix: coop, ray_query: false };
    unsafe { ferritron_vulkan::Gpu::adopt(instance, physical, raw, family, 1, name, native) };
    Some(opened)
}

/// Apple: wgpu runs on Metal; persistent kernels need native Vulkan
/// (MoltenVK cannot share wgpu's device), so they use their own device.
#[cfg(target_vendor = "apple")]
unsafe fn shared_vulkan_device(_adapter: &wgpu::Adapter, _descriptor: &wgpu::DeviceDescriptor, _name: &str) -> Option<(wgpu::Device, wgpu::Queue)> {
    None
}

pub(crate) fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: buffer.as_entire_binding() }
}

/// One convolver's GPU state, owned by its GPU thread.
struct Gpu {
    ctx: Arc<Context>,
    /// Shared with every convolver of the same impulse response.
    _spectra: Arc<wgpu::Buffer>,
    params: wgpu::Buffer,
    ring: wgpu::Buffer,
    fdl: wgpu::Buffer,
    out: wgpu::Buffer,
    staging: wgpu::Buffer,
    forward_ring: wgpu::BindGroup,
    accumulate: wgpu::BindGroup,
    inverse: wgpu::BindGroup,
    ring_len: usize,
    grid: usize,
    n: usize,
    delay: u64,
    partitions: usize,
    chunks: usize,
    slots: usize,
}

impl Gpu {
    fn new(tail: &[f32], grid: usize, delay: usize, max_block: usize) -> Result<Self, String> {
        let ctx = context()?;
        if ctx.lost.load(Relaxed) {
            return Err("the GPU was lost earlier in this process".into());
        }
        let device = &ctx.device;
        let n = grid * 2;
        let partitions = tail.len().div_ceil(grid);
        if partitions > 65_535 {
            // One workgroup per partition, and a dispatch has at most 65,535.
            return Err(format!("impulse response too long ({} samples)", tail.len()));
        }
        let slots = partitions + 1;
        let chunks = partitions.div_ceil(CHUNK);
        // Every sample a window reads, plus input queued ahead of the GPU.
        let ring_len = (n + max_block * 20).next_power_of_two();
        let buffer = |label: &'static str, size: usize, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size as u64, usage, mapped_at_creation: false })
        };
        use wgpu::BufferUsages as U;
        let params = buffer("params", 48, U::UNIFORM | U::COPY_DST);
        let ring = buffer("input ring", ring_len * 4, U::STORAGE | U::COPY_DST);
        // The IR's spectra: shared if another convolver computed them already.
        let key = (fnv(tail), grid, tail.len());
        let cached = ctx.spectra.lock().unwrap_or_else(PoisonError::into_inner).get(&key).and_then(std::sync::Weak::upgrade);
        let fresh = cached.is_none();
        let h = cached.unwrap_or_else(|| Arc::new(buffer("ir spectra", partitions * n * 8, U::STORAGE)));
        let fdl = buffer("input spectra", slots * n * 8, U::STORAGE | U::COPY_DST);
        let spec = buffer("partial sums", chunks * n * 8, U::STORAGE);
        let out = buffer("tail out", grid * 4, U::STORAGE | U::COPY_SRC);
        let staging = buffer("staging", grid * 4, U::MAP_READ | U::COPY_DST);
        ctx.queue.write_buffer(&ring, 0, &vec![0u8; ring_len * 4]);

        let group = |pipeline: &wgpu::ComputePipeline, entries: &[wgpu::BindGroupEntry]| {
            device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries })
        };
        let forward_ring = group(&ctx.forward, &[entry(0, &params), entry(1, &ring), entry(2, &fdl)]);
        let accumulate = group(&ctx.accumulate, &[entry(0, &params), entry(3, &h), entry(4, &fdl), entry(5, &spec)]);
        let inverse = group(&ctx.inverse, &[entry(0, &params), entry(5, &spec), entry(6, &out)]);

        let gpu = Self {
            ctx: ctx.clone(),
            _spectra: h.clone(),
            params,
            ring,
            fdl,
            out,
            staging,
            forward_ring,
            accumulate,
            inverse,
            ring_len,
            grid,
            n,
            delay: delay as u64,
            partitions,
            chunks,
            slots,
        };
        if fresh {
            // Spectra of the impulse response partitions, once (a workgroup each).
            let ir = buffer("ir tail", tail.len() * 4, U::STORAGE | U::COPY_DST);
            ctx.queue.write_buffer(&ir, 0, &f32_bytes(tail));
            let forward_ir = group(&ctx.forward, &[entry(0, &gpu.params), entry(1, &ir), entry(2, &h)]);
            gpu.write_params(0, u32::MAX, tail.len() as u32, grid as u32, 0, 0);
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                pass.set_pipeline(&ctx.forward);
                pass.set_bind_group(0, &forward_ir, &[]);
                pass.dispatch_workgroups(partitions as u32, 1, 1);
            }
            ctx.queue.submit([encoder.finish()]);
            let mut cache = ctx.spectra.lock().unwrap_or_else(PoisonError::into_inner);
            cache.retain(|_, w| w.strong_count() > 0);
            cache.insert(key, Arc::downgrade(&h));
        }
        // Pay pipeline setup now, not on the first audio block; then clear
        // the spectrum the warm-up left behind.
        gpu.transform_and_sum(0, &mut vec![0.0; grid])?;
        ctx.queue.write_buffer(&gpu.fdl, 0, &vec![0u8; slots * n * 8]);
        Ok(gpu)
    }

    /// Clears the input history and the spectra: the stream starts again
    /// from silence (the GPU thread writes the new input after this).
    fn reset_stream(&self) {
        self.ctx.queue.write_buffer(&self.ring, 0, &vec![0u8; self.ring_len * 4]);
        self.ctx.queue.write_buffer(&self.fdl, 0, &vec![0u8; self.slots * self.n * 8]);
    }

    fn write_params(&self, src_base: u32, src_mask: u32, src_count: u32, window: u32, dst_offset: u32, newest: u32) {
        let p = [
            src_base,
            src_mask,
            src_count,
            dst_offset,
            self.n as u32,
            self.n.trailing_zeros(),
            self.grid as u32,
            self.partitions as u32,
            self.slots as u32,
            newest,
            self.chunks as u32,
            window,
        ];
        self.ctx.queue.write_buffer(&self.params, 0, &u32_bytes(&p));
    }

    /// Stores the spectrum of input window `k` (grid blocks `k-1` and `k`)
    /// and computes the tail output of grid block `k + D` into `out`. One
    /// submission: forward FFT, multiply-accumulate, inverse FFT.
    fn transform_and_sum(&self, k: u64, out: &mut [f32]) -> Result<(), String> {
        let ctx = &self.ctx;
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.encode(k, &mut encoder);
        ctx.queue.submit([encoder.finish()]);
        self.staging.map_async(wgpu::MapMode::Read, .., |_| {});
        ctx.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(GPU_WAIT) }).map_err(|e| format!("the GPU did not answer: {e}"))?;
        self.read(out);
        Ok(())
    }

    /// Records the work of window `k` (and the copy of its result to the
    /// staging buffer) into `encoder`. Writes this convolver's parameters:
    /// at most one window per convolver per submission.
    fn encode(&self, k: u64, encoder: &mut wgpu::CommandEncoder) {
        let grid = self.grid as u64;
        let start = (k * grid).wrapping_sub(grid) as usize & (self.ring_len - 1);
        let slot = (k % self.slots as u64) as u32;
        self.write_params(start as u32, (self.ring_len - 1) as u32, self.n as u32, self.n as u32, slot * self.n as u32, slot);
        let ctx = &self.ctx;
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&ctx.forward);
            pass.set_bind_group(0, &self.forward_ring, &[]);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&ctx.accumulate);
            pass.set_bind_group(0, &self.accumulate, &[]);
            pass.dispatch_workgroups((self.n as u32).div_ceil(256), self.chunks as u32, 1);
            pass.set_pipeline(&ctx.inverse);
            pass.set_bind_group(0, &self.inverse, &[]);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.out, 0, &self.staging, 0, (self.grid * 4) as u64);
    }

    /// The result of the last encoded window, once its staging buffer is mapped.
    fn read(&self, out: &mut [f32]) {
        if let Ok(view) = self.staging.get_mapped_range(..) {
            for (o, chunk) in out.iter_mut().zip(view.as_chunks::<4>().0) {
                *o = f32::from_le_bytes(*chunk);
            }
        }
        self.staging.unmap();
    }

    /// Uploads samples at absolute position `start` into the ring.
    fn upload(&self, start: u64, samples: &[f32]) {
        let mut offset = 0;
        while offset < samples.len() {
            let at = (start as usize + offset) & (self.ring_len - 1);
            let len = (samples.len() - offset).min(self.ring_len - at);
            self.ctx.queue.write_buffer(&self.ring, (at * 4) as u64, &f32_bytes(&samples[offset..offset + len]));
            offset += len;
        }
    }
}

/// FNV-1a over the samples' bits: identifies an impulse response.
fn fnv(samples: &[f32]) -> u64 {
    samples.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, s| (h ^ s.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// How many convolvers share the spectra of the impulse response `ir`
/// (its tail, as the convolver stores it). Diagnostics and tests.
#[doc(hidden)]
pub fn spectra_users(tail: &[f32]) -> usize {
    let key = fnv(tail);
    context().map_or(0, |ctx| {
        let cache = ctx.spectra.lock().unwrap_or_else(PoisonError::into_inner);
        cache.iter().filter(|(k, _)| k.0 == key).map(|(_, w)| w.strong_count()).sum()
    })
}

pub(crate) fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub(crate) fn u32_bytes(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// GPU tests run one at a time: several of them check real-time deadlines,
/// which other tests loading the same GPU (offline renders, a persistent
/// kernel) would make them miss.
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Several convolvers share one GPU thread, and each still gets its own
    /// result: their windows go into the same submissions.
    #[test]
    fn convolvers_share_one_gpu_thread_and_stay_exact() {
        let _serial = crate::serial();
        let irs: Vec<Vec<f32>> = (0..4)
            .map(|k| (0..6000).map(|i| ((i * (k + 3)) as f32 * 0.37).sin() * (-(i as f32) / 2000.0).exp()).collect())
            .collect();
        let mut convolvers = Vec::new();
        for ir in &irs {
            match Convolver::with_mode(ir, 256, Mode::Offline) {
                Ok(c) => convolvers.push(c),
                Err(e) => return eprintln!("skipping: {e}"),
            }
        }
        // Other tests may run convolvers at the same time: at least these four.
        assert!(convolvers_on_gpu_thread() >= 4);
        let input: Vec<f32> = (0..8192).map(|i| ((i * 7919) % 1000) as f32 / 500.0 - 1.0).collect();
        let mut outputs = vec![Vec::new(); 4];
        for chunk in input.chunks(256) {
            for (c, out) in convolvers.iter_mut().zip(&mut outputs) {
                let mut block = chunk.to_vec();
                c.process(&mut block);
                out.extend(block);
            }
        }
        for (k, (ir, got)) in irs.iter().zip(&outputs).enumerate() {
            let want: Vec<f32> = (0..input.len()).map(|t| (0..=t.min(ir.len() - 1)).map(|j| ir[j] * input[t - j]).sum()).collect();
            let peak = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
            let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-3 * peak, "convolver {k}: off by {worst} (peak {peak})");
        }
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn decaying_ir(len: usize) -> Vec<f32> {
        noise(len, 7).iter().enumerate().map(|(i, v)| v * (-(i as f32) / (len as f32 / 3.0)).exp()).collect()
    }

    /// Direct convolution in f64: the reference.
    fn reference(ir: &[f32], x: &[f32]) -> Vec<f32> {
        (0..x.len()).map(|t| (0..ir.len().min(t + 1)).map(|k| ir[k] as f64 * x[t - k] as f64).sum::<f64>() as f32).collect()
    }

    /// Plays `input` through a convolver in blocks of `sizes` (cycled), at
    /// real-time pace. None when this machine has no GPU.
    fn play(ir: &[f32], input: &[f32], max_block: usize, sizes: &[usize]) -> Option<(Vec<f32>, u64)> {
        // Bring the GPU to its working clocks first: right after other GPU
        // work stops (another test), its first rounds can be slow.
        if let Ok(mut warm) = Convolver::new(ir, max_block) {
            for _ in 0..20 {
                warm.process(&mut vec![0.0f32; max_block]);
                std::thread::sleep(std::time::Duration::from_micros(max_block as u64 * 1_000_000 / 48_000));
            }
        }
        let mut conv = match Convolver::new(ir, max_block) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping: {e}");
                return None;
            }
        };
        let mut out = Vec::new();
        let (mut at, mut i) = (0, 0);
        while at < input.len() {
            let n = sizes[i % sizes.len()].min(input.len() - at);
            let mut buf = input[at..at + n].to_vec();
            conv.process(&mut buf);
            out.extend_from_slice(&buf);
            at += n;
            i += 1;
            std::thread::sleep(std::time::Duration::from_micros(n as u64 * 1_000_000 / 48_000));
        }
        Some((out, conv.missed_blocks()))
    }

    fn assert_close(out: &[f32], want: &[f32]) {
        let scale = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max).max(1.0);
        let worst = out.iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-4 * scale, "largest difference {worst} (signal peak {scale})");
    }

    #[test]
    fn fixed_blocks_match_direct_convolution() {
        let _serial = crate::serial();
        let ir = decaying_ir(9000);
        let input = noise(256 * 80, 11);
        let Some((out, misses)) = play(&ir, &input, 256, &[256]) else { return };
        assert_eq!(misses, 0, "missed GPU deadlines");
        assert_close(&out, &reference(&ir, &input));
    }

    #[test]
    fn varying_block_sizes_match_direct_convolution() {
        let _serial = crate::serial();
        let ir = decaying_ir(12_000);
        let input = noise(30_000, 3);
        // Hosts may send any size up to max_block, changing every block.
        let Some((out, misses)) = play(&ir, &input, 300, &[300, 37, 256, 1, 299, 128, 64, 5]) else { return };
        assert_eq!(misses, 0, "missed GPU deadlines");
        assert_close(&out, &reference(&ir, &input));
    }

    #[test]
    fn offline_rendering_waits_for_the_gpu() {
        let _serial = crate::serial();
        // A bounce: blocks as fast as process returns, no real-time pacing,
        // and a large max_block (many results in flight).
        let ir = decaying_ir(20_000);
        let input = noise(60_000, 21);
        let mut conv = match Convolver::with_mode(&ir, 4096, Mode::Offline) {
            Ok(c) => c,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let mut out = Vec::new();
        for (i, chunk) in input.chunks(4096).enumerate() {
            let n = if i % 3 == 1 { 1000 } else { chunk.len() };
            for part in chunk.chunks(n) {
                let mut buf = part.to_vec();
                conv.process(&mut buf);
                out.extend_from_slice(&buf);
            }
        }
        assert_eq!(conv.missed_blocks(), 0);
        assert_close(&out, &reference(&ir, &input));
    }

    #[test]
    fn convolvers_with_the_same_impulse_response_share_its_spectra() {
        let _serial = crate::serial();
        let ir = decaying_ir(30_000);
        let Ok(a) = Convolver::new(&ir, 256) else { return eprintln!("skipping: no GPU") };
        let tail = |ir: &[f32]| ir[a.head.len()..].to_vec();
        assert_eq!(spectra_users(&tail(&ir)), 1);
        let b = Convolver::new(&ir, 256).unwrap();
        assert_eq!(spectra_users(&tail(&ir)), 2, "the second convolver shares the spectra");
        let mut other = ir.clone();
        other[20_000] += 0.5;
        let c = Convolver::new(&other, 256).unwrap();
        assert_eq!(spectra_users(&tail(&ir)), 2, "a different IR has its own");
        assert_eq!(spectra_users(&tail(&other)), 1);
        // Shared spectra still convolve correctly.
        let input = noise(256 * 40, 17);
        let run = |mut conv: Convolver| {
            let mut out = Vec::new();
            for chunk in input.chunks(256) {
                let mut buf = chunk.to_vec();
                conv.process(&mut buf);
                out.extend(buf);
                std::thread::sleep(std::time::Duration::from_micros(5_400));
            }
            out
        };
        drop(a);
        assert_close(&run(b), &reference(&ir, &input));
        drop(c);
    }

    /// f7 1-1: a call longer than max_block is processed whole, and the
    /// convolver notes it once (the GPU thread prints the warning).
    #[test]
    fn an_oversized_call_is_processed_whole_and_noted_once() {
        let _serial = crate::serial();
        let Ok(mut conv) = Convolver::new(&decaying_ir(9000), 256) else { return eprintln!("skipping: no GPU") };
        let mut block = vec![0.0f32; 1024];
        conv.process(&mut block);
        assert!(conv.oversize_reported, "the first oversized call is noted");
        conv.process(&mut vec![0.0f32; 1024]);
        assert!(conv.oversize_reported);
    }

    /// f7 1-1: at real-time pace, a 1024-sample block fits when max_block is 1024
    /// (the grid is then 1024 and one grid block of GPU time is enough).
    #[test]
    fn paced_1024_blocks_fit_a_max_block_of_1024() {
        let _serial = crate::serial();
        let ir = decaying_ir(9000);
        let input = noise(96000, 5);
        let Some((_, misses)) = play(&ir, &input, 1024, &[1024]) else { return };
        assert_eq!(misses, 0, "a 1024 block with max_block 1024 must not miss deadlines");
    }

    /// f7 1-2: after a loss, the shared device is reopened once; later calls get the same one.
    #[test]
    fn a_lost_shared_device_is_reopened_once() {
        let _serial = crate::serial();
        let Ok(before) = context() else { return eprintln!("skipping: no GPU") };
        before.lost.store(true, Relaxed);
        let a = context().unwrap();
        let b = context().unwrap();
        assert!(a.generation > before.generation, "a new device was opened");
        assert!(Arc::ptr_eq(&a, &b) && a.generation == b.generation, "later calls reuse the new device");
        if let (Ok(x), Ok(y)) = (shared_vulkan(), shared_vulkan()) {
            assert!(Arc::ptr_eq(&x, &y), "the Vulkan side returns one device");
        }
    }

    #[test]
    fn short_impulse_responses_need_no_gpu() {
        let _serial = crate::serial();
        let mut conv = Convolver::new(&[0.5, 0.25], 64).unwrap();
        let mut buf = [1.0, 0.0, 0.0];
        conv.process(&mut buf);
        assert_eq!(buf, [0.5, 0.25, 0.0]);
    }

    /// V1 (simulated): once the GPU is marked lost, the convolver passes its input through.
    /// The loss itself is not produced here; only the flag the GPU thread sets is set by hand.
    #[test]
    fn a_lost_gpu_passes_the_convolver_input_through() {
        let _serial = crate::serial();
        let ir = decaying_ir(9000);
        let Ok(mut conv) = Convolver::new(&ir, 256) else { return eprintln!("skipping: no GPU") };
        conv.shared.lost.store(true, Relaxed);
        assert!(conv.device_lost());
        let input = noise(1000, 4);
        let mut block = input.clone();
        conv.process(&mut block);
        assert_eq!(block, input, "a lost convolver must not change its input");
    }

    /// V1 (rebuild, simulated loss): the convolver passes its input through,
    /// the GPU thread opens a new device and builds the convolver on it, and
    /// the tail comes back from the restarted stream. The loss is simulated by
    /// setting the flags by hand (no device was lost); the new device is a real one.
    #[test]
    fn a_lost_convolver_is_built_again_and_its_tail_comes_back() {
        let _serial = crate::serial();
        let ir = decaying_ir(9000);
        let Ok(mut conv) = Convolver::new(&ir, 256) else { return eprintln!("skipping: no GPU") };
        let before = conv.device_generation();
        // Simulate the loss: the device and the convolver are marked lost.
        context().unwrap().lost.store(true, Relaxed);
        conv.shared.lost.store(true, Relaxed);
        let mut block = noise(256, 5);
        let input = block.clone();
        conv.process(&mut block);
        assert_eq!(block, input, "a lost convolver passes its input through");
        // Paced silence until the GPU thread has built it again (or give up after 10 s).
        let pace = std::time::Duration::from_micros(256 * 1_000_000 / 48_000);
        let started = Instant::now();
        while conv.rebuilds() == 0 {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the convolver was not built again in 10 s");
            let mut silent = vec![0.0f32; 256];
            conv.process(&mut silent);
            std::thread::sleep(pace);
        }
        assert!(!conv.device_lost(), "built again: no longer lost");
        assert!(conv.device_generation() > before, "built again on a new device");
        // Then an impulse well after the restart: the tail must come back and reproduce the IR.
        let impulse_block = 60;
        let mut out = Vec::new();
        for b in 0..impulse_block + 40 {
            let mut block = vec![0.0f32; 256];
            if b == impulse_block {
                block[0] = 1.0;
            }
            conv.process(&mut block);
            out.extend_from_slice(&block);
            std::thread::sleep(pace);
        }
        assert!(conv.restarts() >= 1, "the stream starts again after a rebuild");
        assert!(!conv.device_lost());
        let at = impulse_block * 256;
        let peak = ir.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        for j in 0..ir.len() {
            let got = out[at + j];
            assert!((got - ir[j]).abs() < 1e-3 * peak.max(1.0), "sample {j} after the impulse: got {got}, want {}", ir[j]);
        }
    }

    /// G5: a block longer than max_block is processed whole, not left dry after the first max_block samples.
    #[test]
    fn blocks_longer_than_max_block_are_processed_whole() {
        let _serial = crate::serial();
        let mut conv = Convolver::new(&[0.5, 0.25], 64).unwrap();
        let mut buf = vec![1.0f32; 200];
        conv.process(&mut buf);
        assert_eq!(buf[0], 0.5);
        assert!(buf[1..].iter().all(|&v| v == 0.75), "samples after the first 64 were not convolved");
    }

    /// G1: when the GPU falls behind by more than its backlog, the input
    /// stream restarts (counted), and the tail comes back once paced again.
    #[test]
    fn a_stalled_gpu_restarts_its_stream_and_the_tail_comes_back() {
        let _serial = crate::serial();
        let ir = decaying_ir(9000);
        let Ok(mut conv) = Convolver::new(&ir, 256) else { return eprintln!("skipping: no GPU") };
        {
            // Hold the GPU thread back while the audio side runs past its backlog (16 blocks).
            let _held = GPU_GATE.lock().unwrap_or_else(PoisonError::into_inner);
            let noise_in = noise(256, 9);
            for _ in 0..40 {
                let mut block = noise_in.clone();
                conv.process(&mut block);
            }
        }
        assert!(conv.restarts() >= 1, "the backlog must force a restart");
        // Paced again: silence, then one impulse. The output must reproduce the IR after it.
        let total = 100 * 256;
        let at = 60 * 256;
        let mut input = vec![0.0f32; total];
        input[at] = 1.0;
        let mut out = Vec::with_capacity(total);
        for chunk in input.chunks(256) {
            let mut block = chunk.to_vec();
            conv.process(&mut block);
            out.extend_from_slice(&block);
            std::thread::sleep(std::time::Duration::from_micros(256 * 1_000_000 / 48_000));
        }
        assert!(!conv.device_lost());
        let peak = ir.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        for j in 0..ir.len() {
            let got = out[at + j];
            assert!((got - ir[j]).abs() < 1e-3 * peak.max(1.0), "sample {j} after the impulse: got {got}, want {}", ir[j]);
        }
    }
}
