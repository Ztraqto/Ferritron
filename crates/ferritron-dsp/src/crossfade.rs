//! Cross-fade between two signals: `out = a * (1 - t) + b * t` (linear) or
//! `a * cos(t pi/2) + b * sin(t pi/2)` (equal power). `t` can move to a new value over a
//! number of samples (the bypass fade of the framework is such a ramp).

use crate::simd::F32x4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeLaw {
    Linear,
    EqualPower,
}

impl FadeLaw {
    /// Gains `(for a, for b)` at position `t` in `[0, 1]`.
    #[inline]
    pub fn gains(self, t: f32) -> (f32, f32) {
        match self {
            FadeLaw::Linear => (1.0 - t, t),
            FadeLaw::EqualPower => {
                let (s, c) = (t * core::f32::consts::FRAC_PI_2).sin_cos();
                (c, s)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Crossfade {
    law: FadeLaw,
    pos: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Crossfade {
    /// `pos` 0 = only `a`, 1 = only `b`.
    pub fn new(law: FadeLaw, pos: f32) -> Self {
        let pos = pos.clamp(0.0, 1.0);
        Crossfade { law, pos, target: pos, step: 0.0, remaining: 0 }
    }

    pub fn position(&self) -> f32 {
        self.pos
    }

    pub fn is_fading(&self) -> bool {
        self.remaining > 0
    }

    /// Moves to `pos` over `samples` samples (0 = at once).
    pub fn set_target(&mut self, pos: f32, samples: u32) {
        let pos = pos.clamp(0.0, 1.0);
        self.target = pos;
        if samples == 0 {
            self.pos = pos;
            self.remaining = 0;
        } else {
            self.step = (pos - self.pos) / samples as f32;
            self.remaining = samples;
        }
    }

    #[inline]
    fn advance(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.pos = self.target;
            } else {
                self.pos += self.step;
            }
        }
        self.pos
    }

    /// One sample.
    #[inline]
    pub fn process(&mut self, a: f32, b: f32) -> f32 {
        let t = self.advance();
        match self.law {
            FadeLaw::Linear => (b - a) * t + a,
            FadeLaw::EqualPower => {
                let (ga, gb) = self.law.gains(t);
                a * ga + b * gb
            }
        }
    }

    /// `out[i]` from `a[i]` and `b[i]`; the shortest of the three lengths is processed.
    pub fn process_block(&mut self, a: &[f32], b: &[f32], out: &mut [f32]) {
        let n = a.len().min(b.len()).min(out.len());
        let (a, b, out) = (&a[..n], &b[..n], &mut out[..n]);
        let mut i = 0;
        if self.remaining == 0 {
            // at rest: constant gains, whole vectors
            match self.law {
                FadeLaw::Linear => {
                    let t = F32x4::splat(self.pos);
                    while i + 4 <= n {
                        let (x, y) = (F32x4::load(&a[i..]), F32x4::load(&b[i..]));
                        ((y - x) * t + x).store(&mut out[i..]);
                        i += 4;
                    }
                }
                FadeLaw::EqualPower => {
                    let (ga, gb) = self.law.gains(self.pos);
                    let (ga, gb) = (F32x4::splat(ga), F32x4::splat(gb));
                    while i + 4 <= n {
                        (F32x4::load(&a[i..]) * ga + F32x4::load(&b[i..]) * gb).store(&mut out[i..]);
                        i += 4;
                    }
                }
            }
        }
        while i < n {
            // ramping (or the tail): one sample at a time, the same formula
            out[i] = self.process(a[i], b[i]);
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laws() {
        assert_eq!(FadeLaw::Linear.gains(0.25), (0.75, 0.25));
        for k in 0..=20 {
            let t = k as f32 / 20.0;
            let (a, b) = FadeLaw::EqualPower.gains(t);
            assert!((a * a + b * b - 1.0).abs() < 1e-6);
        }
        let (a, b) = FadeLaw::EqualPower.gains(0.5);
        assert!((a - core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6 && (a - b).abs() < 1e-6);
        assert_eq!(FadeLaw::EqualPower.gains(0.0).0, 1.0);
        assert!(FadeLaw::EqualPower.gains(1.0).0.abs() < 1e-7);
    }

    #[test]
    fn block_at_rest_matches_scalar() {
        let a: Vec<f32> = (0..11).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..11).map(|i| 100.0 - i as f32).collect();
        for law in [FadeLaw::Linear, FadeLaw::EqualPower] {
            for pos in [0.0f32, 0.3, 1.0] {
                let mut c = Crossfade::new(law, pos);
                let mut out = vec![0.0; 11];
                c.process_block(&a, &b, &mut out);
                let (ga, gb) = law.gains(pos);
                for i in 0..11 {
                    let want = if law == FadeLaw::Linear { (b[i] - a[i]) * pos + a[i] } else { a[i] * ga + b[i] * gb };
                    assert!((out[i] - want).abs() < 1e-4, "{law:?} {pos} {i}");
                }
            }
        }
    }

    #[test]
    fn ramp_ends_exactly_and_is_monotonic() {
        let mut c = Crossfade::new(FadeLaw::Linear, 0.0);
        c.set_target(1.0, 100);
        let a = vec![0.0f32; 128];
        let b = vec![1.0f32; 128];
        let mut out = vec![0.0f32; 128];
        c.process_block(&a, &b, &mut out);
        assert!(out.windows(2).all(|w| w[1] >= w[0]));
        assert!((out[0] - 0.01).abs() < 1e-6);
        assert_eq!(out[99], 1.0);
        assert_eq!(out[127], 1.0);
        assert!(!c.is_fading());
        assert_eq!(c.position(), 1.0);
        c.set_target(0.0, 0);
        assert_eq!(c.process(5.0, 9.0), 5.0);
    }

    #[test]
    fn equal_power_keeps_constant_level_for_identical_inputs() {
        let mut c = Crossfade::new(FadeLaw::EqualPower, 0.0);
        c.set_target(1.0, 48);
        let x = vec![1.0f32; 48];
        let mut out = vec![0.0f32; 48];
        c.process_block(&x, &x, &mut out);
        // identical (fully correlated) signals add in amplitude: cos + sin in [1, sqrt 2]
        assert!(out.iter().all(|&v| (0.999..=1.4143).contains(&v)));
    }
}
