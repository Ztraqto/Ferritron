//! The plugin the framework-cost benchmarks drive: a gain whose level is the
//! sum of eight parameters (so automating all eight is a realistic dense
//! automation load). It counts its own `process` calls.

use ferritron::prelude::*;
use ferritron::MidiKind;
use std::cell::Cell;

thread_local! {
    static PROCESS_CALLS: Cell<u64> = const { Cell::new(0) };
    /// When set, `process` records which of p0..p7 `ctx.changed` reports.
    static TRACK: Cell<bool> = const { Cell::new(false) };
    /// When set, `process` sends a MIDI note and moves p7 from the audio thread.
    static EMIT: Cell<bool> = const { Cell::new(false) };
    static CHANGED_MASK: Cell<u32> = const { Cell::new(0) };
}

/// Makes `process` write output events (`midi_out`, `set_param`) for the tests.
pub fn emit_events(on: bool) {
    EMIT.with(|e| e.set(on));
}

/// Starts (or stops) recording `ctx.changed` for the tests.
pub fn track_changes(on: bool) {
    TRACK.with(|t| t.set(on));
}

/// The p0..p7 bits `ctx.changed` reported in the last `process` call.
pub fn changed_mask() -> u32 {
    CHANGED_MASK.with(Cell::get)
}

/// How many times `process` ran on this thread (the Harness runs it inline).
pub fn process_calls() -> u64 {
    PROCESS_CALLS.with(Cell::get)
}

#[derive(Default)]
pub struct Fx;

#[ferritron::plugin]
impl Process for Fx {
    fn process(&mut self, ctx: &mut Context) {
        PROCESS_CALLS.with(|c| c.set(c.get() + 1));
        if EMIT.with(Cell::get) {
            ctx.midi_out(3, 0, MidiKind::NoteOn { key: 60, velocity: 0.5 });
            ctx.set_param(&ctx.params().p7, 6.0);
        }
        if TRACK.with(Cell::get) {
            let p = &ctx.params();
            let mut mask = 0;
            for (i, c) in [ctx.changed(&p.p0), ctx.changed(&p.p1), ctx.changed(&p.p2), ctx.changed(&p.p3), ctx.changed(&p.p4), ctx.changed(&p.p5), ctx.changed(&p.p6), ctx.changed(&p.p7)].into_iter().enumerate() {
                mask |= (c as u32) << i;
            }
            CHANGED_MASK.with(|m| m.set(mask));
        }
        let p = &ctx.params();
        let db = p.p0.value() + p.p1.value() + p.p2.value() + p.p3.value() + p.p4.value() + p.p5.value() + p.p6.value() + p.p7.value();
        let gain = db_to_gain(db);
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::{AllocGuard, Harness};

    #[global_allocator]
    static ALLOC: AllocGuard = AllocGuard;

    #[test]
    fn the_framework_does_not_allocate_while_processing() {
        let mut h = Harness::<Fx>::new(48_000.0, 512);
        let (mut l, mut r) = (vec![0.1f32; 512], vec![0.1f32; 512]);
        h.process(&mut [&mut l, &mut r]);
        let before = h.audio_allocations();
        for i in 0..200 {
            if i % 3 == 0 {
                h.automate(100, "p0", 0.5);
            }
            h.process(&mut [&mut l, &mut r]);
        }
        assert_eq!(h.audio_allocations(), before, "framework allocated in process");
        h.assert_no_alloc();
    }

    #[test]
    fn dense_automation_runs_once_per_control_period() {
        let mut h = Harness::<Fx>::new(48_000.0, 512);
        let (mut l, mut r) = (vec![0.1f32; 512], vec![0.1f32; 512]);
        h.process(&mut [&mut l, &mut r]);
        let before = process_calls();
        for i in 0..512u32 {
            for k in ["p0", "p1", "p2"] {
                h.automate(i, k, if i % 2 == 0 { 3.0 } else { -3.0 });
            }
        }
        h.automate(511, "p0", 6.0);
        assert!(h.process(&mut [&mut l, &mut r]));
        let calls = process_calls() - before;
        assert!(calls <= 16, "{calls} process calls for one block (limit ceil(512/32) = 16)");
        assert_eq!(h.param("p0"), 6.0, "the last value must stick");
        assert_eq!(h.param("p1"), -3.0);
    }

    #[test]
    fn a_resent_value_neither_splits_nor_wakes() {
        let mut h = Harness::<Fx>::new(48_000.0, 512);
        let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        h.set_param("p0", 2.0);
        for _ in 0..300 {
            h.process(&mut [&mut l, &mut r]);
        }
        let before = process_calls();
        h.automate(100, "p0", 2.0);
        h.process(&mut [&mut l, &mut r]);
        assert!(process_calls() - before <= 1);
    }

    #[test]
    fn output_events_reach_the_host_buffer() {
        let mut h = Harness::<Fx>::new(48_000.0, 256);
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        emit_events(true);
        assert!(h.process(&mut [&mut l, &mut r]));
        emit_events(false);
        let midi = h.midi_out();
        assert_eq!(midi.len(), 1);
        assert_eq!(midi[0].0, 3);
        assert!(matches!(midi[0].2, MidiKind::NoteOn { key: 60, .. }));
        let edits = h.param_edits();
        assert_eq!(edits.len(), 1);
        assert!((edits[0].1 - 0.75).abs() < 1e-9, "6 dB of -12..12 is 0.75 normalized: {edits:?}");
        assert_eq!(h.param("p7"), 6.0);
        // Nothing the next block when the plugin stays quiet.
        assert!(h.process(&mut [&mut l, &mut r]));
        assert!(h.midi_out().is_empty() && h.param_edits().is_empty());
    }

    #[test]
    fn only_changed_parameters_are_reported() {
        let mut h = Harness::<Fx>::new(48_000.0, 512);
        let (mut l, mut r) = (vec![0.1f32; 512], vec![0.1f32; 512]);
        track_changes(true);
        h.process(&mut [&mut l, &mut r]);
        assert_eq!(changed_mask(), 0xFF, "the first block reports everything");
        h.process(&mut [&mut l, &mut r]);
        assert_eq!(changed_mask(), 0);
        h.set_param("p2", 3.0);
        h.set_param("p5", -3.0);
        h.set_param("p1", 0.0); // same value as before: not a change
        h.process(&mut [&mut l, &mut r]);
        assert_eq!(changed_mask(), (1 << 2) | (1 << 5));
        h.process(&mut [&mut l, &mut r]);
        assert_eq!(changed_mask(), 0);
        track_changes(false);
    }

    #[test]
    fn the_guard_catches_a_deliberate_allocation() {
        let (_, n) = ferritron::alloc_check::audio_scope(|| std::hint::black_box(vec![1u8; 64]));
        assert!(n > 0);
    }
}
