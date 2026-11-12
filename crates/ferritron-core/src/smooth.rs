//! Parameter smoothing (`smooth = ...` in the project file).
//!
//! The host's value is the *target*. What `FloatParam::value()` returns for a
//! smoothed parameter is the value at the start of the piece being processed;
//! the framework advances it after every piece. The audio thread owns the
//! state; everything is allocated in `activate`.

use crate::ParamSet;
use std::cell::UnsafeCell;

/// How a parameter is smoothed (generated from the project file).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Smoothing {
    pub seconds: f64,
    /// 0 linear, 1 log (ratio), 2 exp (one pole).
    pub kind: u8,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    on: bool,
    kind: u8,
    /// The kind of the ramp in progress (a log ramp falls back to linear near zero).
    mode: u8,
    time_samples: f64,
    /// exp: the per-sample pole, `1 - exp(-1 / (t * fs))`.
    pole: f64,
    current: f64,
    target: f64,
    /// linear: added per sample; log: multiplied per sample.
    step: f64,
    remaining: u32,
    settled: bool,
}

#[derive(Default)]
struct State {
    slots: Vec<Slot>,
    /// Any smoothed parameter still moving.
    active: bool,
}

pub(crate) struct SmoothBank(UnsafeCell<State>);

// SAFETY: the audio thread owns it; `prepare` runs while it is not processing.
unsafe impl Sync for SmoothBank {}
unsafe impl Send for SmoothBank {}

impl SmoothBank {
    pub(crate) fn new() -> Self {
        Self(UnsafeCell::new(State::default()))
    }

    /// Heap bytes held (one slot per smoothed parameter).
    pub(crate) fn heap_bytes(&self) -> usize {
        self.st().slots.capacity() * size_of::<Slot>()
    }

    fn st(&self) -> &mut State {
        // SAFETY: see above.
        unsafe { &mut *self.0.get() }
    }

    /// Whether any parameter is still moving (pieces are then not joined).
    pub(crate) fn active(&self) -> bool {
        self.st().active
    }

    /// Allocates the state; every parameter starts at its target.
    pub(crate) fn prepare<P: ParamSet>(&self, params: &P, sample_rate: f64) {
        let n = P::infos().len();
        let st = self.st();
        st.slots = (0..n)
            .map(|i| match P::smoothing(i) {
                Some(s) => {
                    let t = (s.seconds * sample_rate).max(1.0);
                    let v = params.cell(i).get();
                    Slot { on: true, kind: s.kind, time_samples: t, pole: 1.0 - (-1.0 / t).exp(), current: v, target: v, settled: true, ..Slot::default() }
                }
                None => Slot::default(),
            })
            .collect();
        st.active = false;
        for (i, slot) in st.slots.iter().enumerate() {
            if slot.on {
                params.cell(i).set_shown(Some(slot.current));
            }
        }
    }

    /// Jumps to the targets (preset load, `reset`): no ramp.
    pub(crate) fn snap<P: ParamSet>(&self, params: &P) {
        let st = self.st();
        for (i, slot) in st.slots.iter_mut().enumerate().filter(|(_, s)| s.on) {
            let v = params.cell(i).get();
            slot.current = v;
            slot.target = v;
            slot.remaining = 0;
            slot.settled = true;
            params.cell(i).set_shown(Some(v));
        }
        st.active = false;
    }

    /// Before a piece: starts ramps towards changed targets and publishes the
    /// current values for `FloatParam::value()`.
    pub(crate) fn begin<P: ParamSet>(&self, params: &P) {
        let st = self.st();
        let mut active = false;
        for (i, slot) in st.slots.iter_mut().enumerate().filter(|(_, s)| s.on) {
            let cell = params.cell(i);
            let target = cell.get();
            if target != slot.target {
                slot.target = target;
                slot.settled = false;
                slot.remaining = slot.time_samples.ceil() as u32;
                let n = f64::from(slot.remaining);
                slot.mode = if slot.kind == 1 && !(slot.current > 0.0 && target > 0.0) { 0 } else { slot.kind };
                slot.step = if slot.mode == 1 { (target / slot.current).powf(1.0 / n) } else { (target - slot.current) / n };
            }
            cell.set_shown(Some(slot.current));
            active |= !slot.settled;
        }
        st.active = active;
    }

    /// After a piece of `n` samples: moves every ramp on.
    pub(crate) fn advance(&self, n: u32) {
        let st = self.st();
        for slot in st.slots.iter_mut().filter(|s| s.on && !s.settled) {
            match slot.mode {
                2 => {
                    slot.current = slot.target + (slot.current - slot.target) * (1.0 - slot.pole).powi(n as i32);
                    if (slot.current - slot.target).abs() <= 1e-9 * slot.target.abs().max(1.0) {
                        slot.current = slot.target;
                        slot.settled = true;
                    }
                }
                _ => {
                    if slot.remaining <= n {
                        slot.current = slot.target;
                        slot.remaining = 0;
                        slot.settled = true;
                    } else {
                        slot.remaining -= n;
                        slot.current = if slot.mode == 1 { slot.current * slot.step.powi(n as i32) } else { slot.current + slot.step * f64::from(n) };
                    }
                }
            }
        }
    }

    pub(crate) fn is_smoothing(&self, index: usize) -> bool {
        self.st().slots.get(index).is_some_and(|s| s.on && !s.settled)
    }

    /// Per-sample values of the piece into `out`; false (and a constant
    /// fill) when the parameter is not moving.
    pub(crate) fn ramp(&self, index: usize, plain_now: f64, out: &mut [f32]) -> bool {
        let Some(slot) = self.st().slots.get(index).filter(|s| s.on && !s.settled) else {
            out.fill(plain_now as f32);
            return false;
        };
        let (mut v, mut left) = (slot.current, slot.remaining);
        for o in out.iter_mut() {
            *o = v as f32;
            match slot.mode {
                2 => v = slot.target + (v - slot.target) * (1.0 - slot.pole),
                _ if left <= 1 => {
                    v = slot.target;
                    left = 0;
                }
                1 => {
                    v *= slot.step;
                    left -= 1;
                }
                _ => {
                    v += slot.step;
                    left -= 1;
                }
            }
        }
        true
    }

    /// Start value and per-sample increment of a moving linear ramp.
    pub(crate) fn ramp_linear(&self, index: usize) -> Option<(f32, f32)> {
        let slot = self.st().slots.get(index).filter(|s| s.on && !s.settled && s.kind == 0)?;
        Some((slot.current as f32, slot.step as f32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamCell, ParamInfo, ParamKind, ParamScale};
    use std::sync::OnceLock;

    #[derive(Default)]
    struct Three([ParamCell; 3]);

    fn infos() -> &'static [ParamInfo] {
        static INFOS: OnceLock<Vec<ParamInfo>> = OnceLock::new();
        INFOS.get_or_init(|| {
            (0..3).map(|i| ParamInfo { id: i, key: "p", name: "P", unit: "", kind: ParamKind::Float { min: 1.0, max: 1000.0 }, default: 1.0, scale: ParamScale::Linear, text: None }).collect()
        })
    }

    impl Default for ParamCell {
        fn default() -> Self {
            ParamCell::new(1.0)
        }
    }

    impl ParamSet for Three {
        fn infos() -> &'static [ParamInfo] {
            infos()
        }
        fn cell(&self, index: usize) -> &ParamCell {
            &self.0[index]
        }
        fn index_of(id: u32) -> Option<usize> {
            (id < 3).then_some(id as usize)
        }
        fn smoothing(index: usize) -> Option<Smoothing> {
            Some(Smoothing { seconds: 0.01, kind: index as u8 })
        }
    }

    #[test]
    fn every_kind_arrives_without_overshoot_and_snap_jumps() {
        let params = Three::default();
        let bank = SmoothBank::new();
        bank.prepare(&params, 48_000.0); // 10 ms = 480 samples
        for c in &params.0 {
            c.set(100.0);
        }
        let mut last = [1.0f64; 3];
        for _ in 0..150 {
            bank.begin(&params);
            for (i, c) in params.0.iter().enumerate() {
                let v = c.shown();
                assert!(v >= last[i] - 1e-9 && v <= 100.0 + 1e-9, "kind {i}: {v} overshoots or turns back");
                last[i] = v;
            }
            bank.advance(32);
        }
        // 4800 samples (10 time constants) later every kind is at its target.
        bank.begin(&params);
        for (i, c) in params.0.iter().enumerate() {
            assert!((c.shown() - 100.0).abs() < 1e-3 * 100.0, "kind {i} ended at {}", c.shown());
        }
        // Linear arrives exactly at its time; at half way it is half way.
        for c in &params.0 {
            c.set(1.0);
        }
        bank.begin(&params);
        bank.advance(240);
        bank.begin(&params);
        assert!((params.0[0].shown() - 50.5).abs() < 0.2, "linear half way: {}", params.0[0].shown());
        // Log is half way in the ratio, exp has covered more than half the distance.
        assert!((params.0[1].shown() - 10.0).abs() < 0.2, "log half way: {}", params.0[1].shown());
        bank.snap(&params);
        assert!(!bank.active());
        assert!(params.0.iter().all(|c| c.shown() == 1.0));
    }
}
