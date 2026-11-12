//! Ring-buffer delay line. The buffer length is a power of two (index by mask); it is
//! allocated in [`DelayLine::new`] and never changes.

#[derive(Clone, Debug)]
pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    /// Index of the newest sample.
    pos: usize,
    max_delay: usize,
}

impl DelayLine {
    /// A line that can delay by up to `max_delay` samples (fractional reads included).
    pub fn new(max_delay: usize) -> Self {
        let size = (max_delay + 2).next_power_of_two();
        DelayLine { buf: vec![0.0; size], mask: size - 1, pos: 0, max_delay }
    }

    pub fn max_delay(&self) -> usize {
        self.max_delay
    }

    /// Allocated length in samples (a power of two).
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
    }

    /// Writes the newest sample.
    #[inline(always)]
    pub fn push(&mut self, x: f32) {
        self.pos = (self.pos + 1) & self.mask;
        self.buf[self.pos] = x;
    }

    /// The sample written `delay` pushes ago (`0` = the newest). `delay` is clamped to `max_delay`.
    #[inline(always)]
    pub fn read(&self, delay: usize) -> f32 {
        let d = delay.min(self.max_delay);
        self.buf[self.pos.wrapping_sub(d) & self.mask]
    }

    /// Linear interpolation between the two neighbouring samples. `delay` is clamped to `[0, max_delay]`;
    /// NaN reads as 0.
    #[inline(always)]
    pub fn read_frac(&self, delay: f32) -> f32 {
        let d = if delay.is_nan() { 0.0 } else { delay.clamp(0.0, self.max_delay as f32) };
        let i = d as usize;
        let f = d - i as f32;
        let a = self.buf[self.pos.wrapping_sub(i) & self.mask];
        let b = self.buf[self.pos.wrapping_sub(i + 1) & self.mask];
        a + (b - a) * f
    }

    /// Pushes `x`, then reads at an integer `delay`.
    #[inline(always)]
    pub fn process(&mut self, x: f32, delay: usize) -> f32 {
        self.push(x);
        self.read(delay)
    }

    /// Pushes `x`, then reads at a fractional `delay`.
    #[inline(always)]
    pub fn process_frac(&mut self, x: f32, delay: f32) -> f32 {
        self.push(x);
        self.read_frac(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nan_delay_reads_as_zero() {
        let mut d = DelayLine::new(8);
        d.push(1.0);
        assert_eq!(d.read_frac(f32::NAN), 1.0);
    }


    #[test]
    fn capacity_is_a_power_of_two() {
        for m in [0usize, 1, 2, 5, 62, 63, 64, 1000] {
            let d = DelayLine::new(m);
            assert!(d.capacity().is_power_of_two());
            assert!(d.capacity() >= m + 2);
        }
    }

    #[test]
    fn integer_delay_is_exact() {
        let mut d = DelayLine::new(100);
        for delay in [0usize, 1, 7, 100] {
            d.clear();
            let mut out = Vec::new();
            for n in 0..300 {
                out.push(d.process(n as f32 + 1.0, delay));
            }
            for n in 0..300 {
                let want = if n >= delay { (n - delay) as f32 + 1.0 } else { 0.0 };
                assert_eq!(out[n], want, "delay {delay} at {n}");
            }
        }
    }

    #[test]
    fn fractional_delay_interpolates_linearly() {
        let mut d = DelayLine::new(16);
        for n in 0..40 {
            d.push(n as f32);
        }
        // newest = 39; delay 2.25 is between 37 (delay 2) and 36 (delay 3)
        assert!((d.read_frac(2.25) - 36.75).abs() < 1e-5);
        assert_eq!(d.read_frac(0.0), 39.0);
        assert_eq!(d.read_frac(3.0), 36.0);
        // clamped at both ends
        assert_eq!(d.read_frac(-5.0), 39.0);
        assert_eq!(d.read_frac(1000.0), d.read(16));
        assert_eq!(d.read(1000), d.read(16));
    }

    #[test]
    fn wraps_around_many_times() {
        let mut d = DelayLine::new(5);
        let cap = d.capacity();
        let mut last = 0.0;
        for n in 0..(cap * 20) {
            last = d.process(n as f32, 3);
        }
        assert_eq!(last, (cap * 20 - 1 - 3) as f32);
    }
}
