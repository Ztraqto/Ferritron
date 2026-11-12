//! The built-in bypass: crossfades between the plugin's output and the input
//! delayed by the plugin's latency (so bypassing does not click and the
//! bypassed signal stays in time with what the host compensates).
//!
//! While fully bypassed the plugin is not called at all. The audio thread owns
//! the state; nothing here allocates on it.
//!
//! Memory (T5): an instance keeps only the delay rings (`channels × latency`).
//! The copy of the input a crossfade needs (the plugin overwrites the input in
//! place) comes from a process-wide pool of [`POOL_SLOTS`] buffers sized in
//! `prepare`, claimed lock-free for the blocks of a crossfade (10 ms). When
//! every buffer is in use (more than [`POOL_SLOTS`] instances crossfading in
//! the same moment), the instance switches without a fade.

use std::cell::UnsafeCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Length of the crossfade.
const FADE_SECONDS: f64 = 0.010;

/// Input copies shared by every instance in the process.
pub(crate) const POOL_SLOTS: usize = 4;

struct Slot {
    busy: AtomicBool,
    ptr: AtomicPtr<f32>,
    len: AtomicUsize,
}

static POOL: [Slot; POOL_SLOTS] = [const { Slot { busy: AtomicBool::new(false), ptr: AtomicPtr::new(std::ptr::null_mut()), len: AtomicUsize::new(0) } }; POOL_SLOTS];
/// Serializes `grow` (main threads of several instances).
static GROW: Mutex<()> = Mutex::new(());

/// Main thread (activate): makes every free pool buffer at least `len` samples. A buffer in use
/// is grown by a later `prepare`; until then it is too small for this instance, which then
/// takes another or switches without a fade.
fn grow(len: usize) {
    let _guard = GROW.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for slot in &POOL {
        if slot.len.load(Ordering::Acquire) >= len {
            continue;
        }
        if slot.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        let new: Box<[f32]> = vec![0.0; len].into_boxed_slice();
        let old = slot.ptr.swap(Box::into_raw(new) as *mut f32, Ordering::AcqRel);
        let old_len = slot.len.swap(len, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: made by `Box::into_raw` above with `old_len` elements; we hold the slot.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(old, old_len)) });
        }
        slot.busy.store(false, Ordering::Release);
    }
}

/// Audio thread: a free pool buffer of at least `len` samples (lock-free), as a slot index.
fn claim(len: usize) -> Option<usize> {
    POOL.iter().position(|slot| {
        slot.len.load(Ordering::Acquire) >= len && slot.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok() && {
            // Grown meanwhile to a size we checked before the claim? Re-check under the claim.
            let ok = slot.len.load(Ordering::Acquire) >= len;
            if !ok {
                slot.busy.store(false, Ordering::Release);
            }
            ok
        }
    })
}

fn release(slot: usize) {
    POOL[slot].busy.store(false, Ordering::Release);
}

/// Moves `io` through the ring starting at `pos`: the ring takes the samples, and with
/// `swap` the samples take what the ring held. Returns the position after the last sample.
fn exchange(ring: &mut [f32], mut pos: usize, io: &mut [f32], swap: bool) -> usize {
    let cap = ring.len();
    let mut done = 0;
    while done < io.len() {
        let n = (io.len() - done).min(cap - pos);
        let (r, s) = (&mut ring[pos..pos + n], &mut io[done..done + n]);
        if swap {
            r.swap_with_slice(s);
        } else {
            r.copy_from_slice(s);
        }
        done += n;
        pos += n;
        if pos == cap {
            pos = 0;
        }
    }
    pos
}

#[derive(Default)]
struct State {
    channels: usize,
    max_block: usize,
    /// The input delayed by the latency: `channels` rings of `cap` samples.
    ring: Vec<f32>,
    cap: usize,
    pos: usize,
    /// 0 = the plugin's output, 1 = the delayed input.
    mix: f32,
    step: f32,
    /// The plugin was not called in the last block(s) (fully bypassed) and has not been reset since.
    skipped: bool,
}

pub(crate) struct Bypass(UnsafeCell<State>);

// SAFETY: the audio thread owns it; `prepare` runs while it is not processing.
unsafe impl Sync for Bypass {}
unsafe impl Send for Bypass {}

/// What `begin` decided for this block.
#[derive(Clone, Copy)]
pub(crate) struct Plan {
    /// The plugin need not run (fully bypassed).
    pub skip_plugin: bool,
    /// The pool buffer holding this block's input for the crossfade in `finish`.
    fade: Option<usize>,
}

impl Bypass {
    /// Heap bytes this instance holds (the delay rings; the crossfade pool is shared).
    pub(crate) fn heap_bytes(&self) -> usize {
        // SAFETY: read while not processing (tests, reports).
        let st = unsafe { &*self.0.get() };
        st.ring.capacity() * size_of::<f32>()
    }

    pub(crate) fn new() -> Self {
        Self(UnsafeCell::new(State::default()))
    }

    fn st(&self) -> &mut State {
        // SAFETY: see above.
        unsafe { &mut *self.0.get() }
    }

    /// Allocates for `channels` channels, blocks up to `max_block` samples and
    /// a delay of `latency` samples. Memory in bytes of this instance is returned.
    pub(crate) fn prepare(&self, channels: usize, max_block: usize, latency: u32, sample_rate: f64) -> usize {
        let st = self.st();
        st.channels = channels;
        st.max_block = max_block;
        st.cap = latency as usize;
        st.ring = vec![0.0; channels * st.cap];
        st.pos = 0;
        st.step = (1.0 / (FADE_SECONDS * sample_rate).max(1.0)) as f32;
        st.skipped = false;
        grow(channels * max_block);
        st.ring.len() * 4
    }

    /// Jumps to the bypass state without a fade (first block, state load).
    pub(crate) fn snap(&self, bypassed: bool) {
        let st = self.st();
        st.mix = if bypassed { 1.0 } else { 0.0 };
        st.skipped = false;
    }

    /// True once after blocks in which the plugin was not called (fully bypassed): the caller
    /// resets the plugin before it runs again.
    pub(crate) fn take_resume(&self, plan: &Plan) -> bool {
        let st = self.st();
        if plan.skip_plugin || !st.skipped {
            return false;
        }
        st.skipped = false;
        true
    }

    /// Before the plugin runs. Outside a crossfade it feeds the delay ring here (and, fully
    /// bypassed, already writes the delayed input out); during one it keeps the input in a pool
    /// buffer for `finish`.
    pub(crate) fn begin(&self, bypassed: bool, channels: &[*mut f32], frames: usize) -> Plan {
        let st = self.st();
        let target = if bypassed { 1.0 } else { 0.0 };
        let usable = frames <= st.max_block;
        let mut fade = None;
        if st.mix != target && usable {
            fade = claim(st.channels * st.max_block);
            if fade.is_none() {
                st.mix = target; // no buffer free: switch without a fade
            }
        } else if st.mix != target {
            st.mix = target;
        }
        let skip_plugin = bypassed && fade.is_none();
        if let Some(slot) = fade {
            let base = POOL[slot].ptr.load(Ordering::Acquire);
            for (c, &p) in channels.iter().enumerate().take(st.channels) {
                // SAFETY: the shim passes `frames` valid samples per channel; the claimed pool
                // buffer holds `channels * max_block` samples and is ours until `release`.
                unsafe { std::ptr::copy_nonoverlapping(p, base.add(c * st.max_block), frames) };
            }
        } else if st.cap > 0 && usable {
            // Steady state: the ring takes the input; fully bypassed, the output is the
            // delayed input (the plugin is not called, so this is final).
            let (cap, start) = (st.cap, st.pos);
            let mut end = start;
            for (c, &p) in channels.iter().enumerate().take(st.channels) {
                // SAFETY: as above.
                let io = unsafe { std::slice::from_raw_parts_mut(p, frames) };
                let ring = &mut st.ring[c * cap..(c + 1) * cap];
                let pos = exchange(ring, start, io, skip_plugin);
                end = pos;
            }
            st.pos = end;
        }
        if skip_plugin {
            st.skipped = true;
        }
        Plan { skip_plugin, fade }
    }

    /// The plugin panicked after `begin`: gives the pool buffer back without mixing.
    pub(crate) fn abort(&self, plan: Plan) {
        if let Some(slot) = plan.fade {
            release(slot);
        }
    }

    /// After the plugin ran: during a crossfade, feeds the delay and mixes.
    pub(crate) fn finish(&self, plan: Plan, bypassed: bool, channels: &[*mut f32], frames: usize) {
        let Some(slot) = plan.fade else { return };
        let st = self.st();
        let target = if bypassed { 1.0f32 } else { 0.0 };
        let (cap, max_block) = (st.cap, st.max_block);
        let base = POOL[slot].ptr.load(Ordering::Acquire);
        let start_mix = st.mix;
        let mut end_mix = start_mix;
        let mut end_pos = st.pos;
        for (c, &p) in channels.iter().enumerate().take(st.channels) {
            // SAFETY: as in `begin`.
            let out = unsafe { std::slice::from_raw_parts_mut(p, frames) };
            let dry_in = unsafe { std::slice::from_raw_parts(base.add(c * max_block), frames) };
            let mut mix = start_mix;
            let mut pos = st.pos;
            for (o, &x) in out.iter_mut().zip(dry_in) {
                let dry = if cap > 0 {
                    let d = st.ring[c * cap + pos];
                    st.ring[c * cap + pos] = x;
                    pos += 1;
                    if pos == cap {
                        pos = 0;
                    }
                    d
                } else {
                    x
                };
                if mix != target {
                    mix = if mix < target { (mix + st.step).min(target) } else { (mix - st.step).max(target) };
                }
                // The fully processed or fully bypassed case needs no mixing.
                if mix >= 1.0 {
                    *o = dry;
                } else if mix > 0.0 {
                    *o += (dry - *o) * mix;
                }
            }
            end_mix = mix;
            end_pos = pos;
        }
        st.mix = end_mix;
        st.pos = end_pos;
        release(slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pool_hands_out_each_buffer_once_and_grows_only_free_ones() {
        grow(256);
        let taken: Vec<usize> = (0..POOL_SLOTS).filter_map(|_| claim(256)).collect();
        // Other tests may hold some; whatever we got are distinct.
        let mut sorted = taken.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), taken.len());
        if taken.len() == POOL_SLOTS {
            assert!(claim(1).is_none(), "all in use");
        }
        for s in taken {
            release(s);
        }
        assert!(claim(1 << 30).is_none(), "nothing that large");
    }

    /// The per-sample loop that `exchange` replaces (the reference).
    fn per_sample(ring: &mut [f32], mut pos: usize, io: &mut [f32], swap: bool) -> usize {
        let cap = ring.len();
        for s in io.iter_mut() {
            let d = ring[pos];
            ring[pos] = *s;
            if swap {
                *s = d;
            }
            pos += 1;
            if pos == cap {
                pos = 0;
            }
        }
        pos
    }

    #[test]
    fn exchange_matches_the_per_sample_loop() {
        for cap in [1usize, 7, 64] {
            for len in [0usize, 1, 5, 64, 200] {
                for start in [0usize, cap - 1] {
                    for swap in [true, false] {
                        let ring0: Vec<f32> = (0..cap).map(|i| -(i as f32) - 1.0).collect();
                        let io0: Vec<f32> = (0..len).map(|i| i as f32).collect();

                        let (mut ring_a, mut io_a) = (ring0.clone(), io0.clone());
                        let pos_a = per_sample(&mut ring_a, start, &mut io_a, swap);

                        let (mut ring_b, mut io_b) = (ring0.clone(), io0.clone());
                        let pos_b = exchange(&mut ring_b, start, &mut io_b, swap);

                        assert_eq!(io_a, io_b, "io: cap {cap} len {len} start {start} swap {swap}");
                        assert_eq!(ring_a, ring_b, "ring: cap {cap} len {len} start {start} swap {swap}");
                        assert_eq!(pos_a, pos_b, "pos: cap {cap} len {len} start {start} swap {swap}");
                    }
                }
            }
        }
    }

    /// Speed of `exchange` against the per-sample loop, alternating 5 times each.
    /// Run: `cargo test --release -p ferritron-core bench_exchange -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_exchange() {
        use std::time::{Duration, Instant};
        const CHANNELS: usize = 2;
        const CAP: usize = 1024;
        const BLOCK: usize = 512;
        const BLOCKS: usize = 200_000;

        fn time(f: fn(&mut [f32], usize, &mut [f32], bool) -> usize) -> Duration {
            let mut rings = vec![0.0f32; CHANNELS * CAP];
            let mut io = [vec![0.5f32; BLOCK], vec![0.25f32; BLOCK]];
            let started = Instant::now();
            let mut pos = 0;
            for _ in 0..BLOCKS {
                let mut end = pos;
                for c in 0..CHANNELS {
                    let ring = &mut rings[c * CAP..(c + 1) * CAP];
                    end = f(ring, pos, io[c].as_mut_slice(), true);
                }
                pos = end;
            }
            let elapsed = started.elapsed();
            std::hint::black_box(&rings);
            std::hint::black_box(&io);
            elapsed
        }

        let (mut best_loop, mut best_exchange) = (Duration::MAX, Duration::MAX);
        for _ in 0..5 {
            best_loop = best_loop.min(time(per_sample));
            best_exchange = best_exchange.min(time(exchange));
        }
        eprintln!("per-sample loop, best of 5: {best_loop:?} ({BLOCKS} blocks, {CHANNELS} ch, cap {CAP}, block {BLOCK})");
        eprintln!("exchange, best of 5:        {best_exchange:?} ({BLOCKS} blocks, {CHANNELS} ch, cap {CAP}, block {BLOCK})");
    }
}
