//! Parameter smoothing: linear, logarithmic (ratio) and one-pole exponential ramps.
//!
//! `set_target(plain, samples)` starts a ramp that ends exactly on `plain` after `samples`
//! calls to [`Smoother::next`]. For [`SmoothKind::Exp`] the path is a one-pole curve whose
//! time constant comes from [`Smoother::set_time_constant`] (computed once, in `activate`);
//! `samples` then only bounds the ramp: after that many samples the value snaps to the
//! target. The state is `f64` inside, so long ramps do not drift.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmoothKind {
    /// Straight line in the plain value.
    Linear,
    /// Straight line in the ratio (frequency-like values). Falls back to `Linear` while the
    /// current or target value is not positive.
    Log,
    /// One-pole exponential approach; never overshoots.
    Exp,
}

#[derive(Clone, Debug)]
pub struct Smoother {
    kind: SmoothKind,
    current: f64,
    target: f64,
    /// Linear: added each sample. Log: multiplier. Exp: unused.
    step: f64,
    /// Exp: `1 - exp(-1 / tau)`.
    coef: f64,
    /// True when the running ramp is logarithmic.
    log_now: bool,
    remaining: u32,
}

impl Smoother {
    pub fn new(kind: SmoothKind, value: f32) -> Self {
        let v = value as f64;
        Smoother { kind, current: v, target: v, step: 0.0, coef: 1.0, log_now: false, remaining: 0 }
    }

    pub fn kind(&self) -> SmoothKind {
        self.kind
    }

    /// Exp only: the time constant in samples (`tau`). Does the `exp` once; call it from `activate`.
    pub fn set_time_constant(&mut self, tau_samples: f32) {
        self.coef = if tau_samples <= 0.0 { 1.0 } else { 1.0 - (-1.0 / tau_samples as f64).exp() };
    }

    /// Time constant in seconds at a sample rate (see [`Smoother::set_time_constant`]).
    pub fn set_time_constant_secs(&mut self, secs: f32, sample_rate: f32) {
        self.set_time_constant(secs * sample_rate);
    }

    /// Starts a ramp to `plain`. `samples == 0` jumps at once.
    pub fn set_target(&mut self, plain: f32, samples: u32) {
        let t = plain as f64;
        self.target = t;
        if samples == 0 || t == self.current {
            self.snap_to(t);
            return;
        }
        self.remaining = samples;
        self.log_now = false;
        match self.kind {
            SmoothKind::Linear => self.step = (t - self.current) / samples as f64,
            SmoothKind::Log => {
                if self.current > 0.0 && t > 0.0 {
                    self.log_now = true;
                    self.step = (t / self.current).powf(1.0 / samples as f64);
                } else {
                    self.step = (t - self.current) / samples as f64;
                }
            }
            SmoothKind::Exp => {}
        }
    }

    /// Jumps to the target (reset, state load, preset change).
    pub fn snap(&mut self) {
        let t = self.target;
        self.snap_to(t);
    }

    /// Jumps to `plain` and forgets any ramp.
    pub fn snap_to(&mut self, plain: f64) {
        self.current = plain;
        self.target = plain;
        self.remaining = 0;
        self.step = 0.0;
    }

    #[inline]
    pub fn is_smoothing(&self) -> bool {
        self.remaining > 0
    }

    /// The value the next [`Smoother::next`] would start from.
    #[inline]
    pub fn value(&self) -> f32 {
        self.current as f32
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target as f32
    }

    /// Advances one sample and returns the new value.
    #[inline]
    pub fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            } else {
                match self.kind {
                    SmoothKind::Exp => self.current += (self.target - self.current) * self.coef,
                    _ if self.log_now => self.current *= self.step,
                    _ => self.current += self.step,
                }
            }
        }
        self.current as f32
    }

    /// Fills `out` with consecutive values (no allocation). A smoother at rest writes a constant.
    pub fn fill(&mut self, out: &mut [f32]) {
        if self.remaining == 0 {
            out.fill(self.current as f32);
        } else {
            for o in out {
                *o = self.next();
            }
        }
    }

    /// Advances `n` samples at once and returns the value after them.
    pub fn skip(&mut self, n: u32) -> f32 {
        if self.remaining == 0 {
            return self.current as f32;
        }
        if n >= self.remaining {
            self.snap_to(self.target);
            return self.current as f32;
        }
        match self.kind {
            SmoothKind::Linear => self.current += self.step * n as f64,
            SmoothKind::Log if self.log_now => self.current *= self.step.powi(n as i32),
            SmoothKind::Log => self.current += self.step * n as f64,
            SmoothKind::Exp => {
                self.current = self.target - (self.target - self.current) * (1.0 - self.coef).powi(n as i32)
            }
        }
        self.remaining -= n;
        self.current as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_reaches_target_exactly() {
        let mut s = Smoother::new(SmoothKind::Linear, 0.0);
        s.set_target(1.0, 100);
        assert!(s.is_smoothing());
        let mut prev = 0.0;
        for i in 0..100 {
            let v = s.next();
            assert!(v > prev, "monotonic at {i}");
            prev = v;
        }
        assert_eq!(prev, 1.0);
        assert!(!s.is_smoothing());
        assert_eq!(s.next(), 1.0);
        let mut t = Smoother::new(SmoothKind::Linear, 0.0);
        t.set_target(1.0, 100);
        assert!((t.next() - 0.01).abs() < 1e-7);
    }

    #[test]
    fn log_is_a_straight_line_in_ratio() {
        let mut s = Smoother::new(SmoothKind::Log, 100.0);
        s.set_target(1600.0, 4);
        let v: Vec<f32> = (0..4).map(|_| s.next()).collect();
        assert!((v[0] - 200.0).abs() < 1e-3);
        assert!((v[1] - 400.0).abs() < 1e-3);
        assert!((v[2] - 800.0).abs() < 1e-3);
        assert_eq!(v[3], 1600.0);
    }

    #[test]
    fn log_falls_back_to_linear_for_non_positive() {
        let mut s = Smoother::new(SmoothKind::Log, 0.0);
        s.set_target(10.0, 10);
        assert!((s.next() - 1.0).abs() < 1e-6);
        let mut d = Smoother::new(SmoothKind::Log, 5.0);
        d.set_target(-5.0, 10);
        assert!((d.next() - 4.0).abs() < 1e-6);
    }

    #[test]
    fn exp_approaches_without_overshoot_and_ends_on_target() {
        let fs = 48_000.0;
        let mut s = Smoother::new(SmoothKind::Exp, 0.0);
        s.set_time_constant_secs(0.005, fs);
        s.set_target(1.0, (fs * 0.05) as u32);
        let mut prev = 0.0f32;
        let mut at_tau = 0.0;
        for i in 0..(fs * 0.05) as usize {
            let v = s.next();
            assert!(v >= prev && v <= 1.0);
            prev = v;
            if i == (fs * 0.005) as usize - 1 {
                at_tau = v;
            }
        }
        // after one time constant the value is 1 - 1/e
        assert!((at_tau - (1.0 - (-1.0f32).exp())).abs() < 2e-3, "{at_tau}");
        assert_eq!(prev, 1.0);
        assert!(!s.is_smoothing());
    }

    #[test]
    fn snap_and_zero_samples_jump() {
        let mut s = Smoother::new(SmoothKind::Linear, 0.0);
        s.set_target(1.0, 1000);
        s.next();
        s.snap();
        assert_eq!(s.value(), 1.0);
        assert!(!s.is_smoothing());
        s.set_target(2.0, 0);
        assert_eq!(s.next(), 2.0);
        let mut e = Smoother::new(SmoothKind::Exp, 0.0);
        e.set_target(3.0, 10);
        e.snap_to(7.0);
        assert_eq!((e.value(), e.target()), (7.0, 7.0));
    }

    #[test]
    fn retarget_midway_continues_from_current() {
        let mut s = Smoother::new(SmoothKind::Linear, 0.0);
        s.set_target(1.0, 10);
        for _ in 0..5 {
            s.next();
        }
        s.set_target(0.0, 5);
        let mut last = 1.0;
        for _ in 0..5 {
            last = s.next();
        }
        assert_eq!(last, 0.0);
    }

    #[test]
    fn fill_and_skip_match_next() {
        for kind in [SmoothKind::Linear, SmoothKind::Log, SmoothKind::Exp] {
            let mk = || {
                let mut s = Smoother::new(kind, 10.0);
                s.set_time_constant(20.0);
                s.set_target(80.0, 64);
                s
            };
            let mut a = mk();
            let mut buf = [0.0f32; 40];
            a.fill(&mut buf);
            let mut b = mk();
            let vals: Vec<f32> = (0..40).map(|_| b.next()).collect();
            assert_eq!(&buf[..], &vals[..]);
            let mut c = mk();
            let v = c.skip(39);
            assert!((v - vals[38]).abs() <= vals[38].abs() * 1e-5, "{kind:?}: {v} vs {}", vals[38]);
            let mut rest = mk();
            assert_eq!(rest.skip(1000), 80.0);
        }
        let mut idle = Smoother::new(SmoothKind::Linear, 3.0);
        let mut out = [0.0f32; 8];
        idle.fill(&mut out);
        assert_eq!(out, [3.0; 8]);
    }
}
