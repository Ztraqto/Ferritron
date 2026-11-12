//! Sliding minimum / maximum over the last `window` samples (monotonic deque, amortised
//! O(1) per sample) and a peak-hold follower. NaN inputs are not supported.

use crate::flush_denormal;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
struct Extremum {
    window: u64,
    cap: u64,
    index: u64,
    dq: VecDeque<(u64, f32)>,
    max: bool,
}

impl Extremum {
    fn new(window: usize, max: bool) -> Self {
        let window = window.max(1);
        // The deque never holds more than `window` entries: no allocation in `push`.
        Extremum { window: window as u64, cap: window as u64, index: 0, dq: VecDeque::with_capacity(window + 1), max }
    }

    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        while let Some(&(_, v)) = self.dq.back() {
            if (self.max && v <= x) || (!self.max && v >= x) {
                self.dq.pop_back();
            } else {
                break;
            }
        }
        self.dq.push_back((self.index, x));
        while let Some(&(i, _)) = self.dq.front() {
            if self.index - i >= self.window {
                self.dq.pop_front();
            } else {
                break;
            }
        }
        self.index += 1;
        self.dq.front().map_or(x, |&(_, v)| v)
    }

    fn set_window(&mut self, window: usize) {
        self.window = (window as u64).clamp(1, self.cap);
    }

    fn reset(&mut self) {
        self.dq.clear();
        self.index = 0;
    }
}

macro_rules! sliding {
    ($name:ident, $max:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Debug)]
        pub struct $name(Extremum);
        impl $name {
            /// `window` is the number of samples covered, the newest included (at least 1).
            pub fn new(window: usize) -> Self {
                $name(Extremum::new(window, $max))
            }
            /// Adds a sample and returns the extremum of the last `window` samples.
            #[inline]
            pub fn push(&mut self, x: f32) -> f32 {
                self.0.push(x)
            }
            pub fn window(&self) -> usize {
                self.0.window as usize
            }
            /// Changes the window without allocating; limited to the size given to `new`.
            /// A shorter window takes effect with the next `push`.
            pub fn set_window(&mut self, window: usize) {
                self.0.set_window(window)
            }
            pub fn reset(&mut self) {
                self.0.reset()
            }
        }
    };
}
sliding!(SlidingMax, true, "Maximum of the last `window` samples.");
sliding!(SlidingMin, false, "Minimum of the last `window` samples.");

/// Peak follower: jumps up at once, holds for `hold` samples, then falls by a fixed ratio
/// per sample (60 dB over `release` samples). Input is rectified inside.
#[derive(Clone, Debug)]
pub struct PeakHold {
    hold: u32,
    decay: f32,
    level: f32,
    counter: u32,
}

impl PeakHold {
    pub fn new(hold_samples: u32, release_samples: f32) -> Self {
        let decay = if release_samples <= 0.0 { 0.0 } else { (-6.907_755_3f32 / release_samples).exp() };
        PeakHold { hold: hold_samples, decay, level: 0.0, counter: 0 }
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn reset(&mut self) {
        self.level = 0.0;
        self.counter = 0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let a = x.abs();
        if a >= self.level {
            self.level = a;
            self.counter = self.hold;
        } else if self.counter > 0 {
            self.counter -= 1;
        } else {
            self.level = flush_denormal(self.level * self.decay).max(a);
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(data: &[f32], w: usize, max: bool) -> Vec<f32> {
        (0..data.len())
            .map(|i| {
                let lo = (i + 1).saturating_sub(w);
                let it = data[lo..=i].iter().copied();
                if max { it.fold(f32::MIN, f32::max) } else { it.fold(f32::MAX, f32::min) }
            })
            .collect()
    }

    #[test]
    fn matches_a_brute_force_scan() {
        let mut s = 12345u32;
        let data: Vec<f32> = (0..2000)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
            })
            .collect();
        for w in [1usize, 2, 3, 17, 64, 500, 5000] {
            let mut mx = SlidingMax::new(w);
            let mut mn = SlidingMin::new(w);
            let a: Vec<f32> = data.iter().map(|&x| mx.push(x)).collect();
            let b: Vec<f32> = data.iter().map(|&x| mn.push(x)).collect();
            assert_eq!(a, naive(&data, w, true), "max w={w}");
            assert_eq!(b, naive(&data, w, false), "min w={w}");
        }
    }

    #[test]
    fn monotonic_input_and_equal_values() {
        let mut mx = SlidingMax::new(3);
        let out: Vec<f32> = [1.0, 2.0, 3.0, 4.0, 3.0, 2.0, 1.0, 1.0, 1.0].iter().map(|&x| mx.push(x)).collect();
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 4.0, 4.0, 3.0, 2.0, 1.0]);
        let mut mn = SlidingMin::new(2);
        let out: Vec<f32> = [5.0, 5.0, 5.0, 1.0, 9.0, 9.0].iter().map(|&x| mn.push(x)).collect();
        assert_eq!(out, [5.0, 5.0, 5.0, 1.0, 1.0, 9.0]);
        mn.reset();
        assert_eq!(mn.push(7.0), 7.0);
        assert_eq!(mn.window(), 2);
    }

    #[test]
    fn peak_hold_holds_then_falls() {
        let mut p = PeakHold::new(10, 100.0);
        assert_eq!(p.process(-0.8), 0.8);
        for _ in 0..10 {
            assert_eq!(p.process(0.0), 0.8, "held");
        }
        let after_hold = p.process(0.0);
        assert!((after_hold - 0.8 * (-6.907_755_3f32 / 100.0).exp()).abs() < 1e-5, "{after_hold}");
        let mut v = after_hold;
        for _ in 0..99 {
            v = p.process(0.0);
        }
        // 60 dB over 100 samples of release (the first falling sample is included above)
        assert!((v - 0.8e-3).abs() < 1e-4, "{v}");
        assert_eq!(p.process(0.9), 0.9);
        p.reset();
        assert_eq!(p.level(), 0.0);
        let mut q = PeakHold::new(0, 4.0);
        q.process(1.0);
        for _ in 0..2000 {
            q.process(0.0);
        }
        assert_eq!(q.level(), 0.0, "decays to exactly zero");
    }
}
