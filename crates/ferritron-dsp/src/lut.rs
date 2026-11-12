//! Look-up table with linear interpolation over `[lo, hi]`; inputs outside are clamped.
//! The table is built in [`Lut::new`] (not on the audio thread); [`Lut::get`] does not allocate.

#[derive(Clone, Debug)]
pub struct Lut {
    table: Vec<f32>,
    lo: f32,
    hi: f32,
    scale: f32,
    size: usize,
}

impl Lut {
    /// `size` intervals (rounded up to a power of two, at least 2); `f` is sampled at
    /// `size + 1` points from `lo` to `hi` inclusive.
    pub fn new(lo: f32, hi: f32, size: usize, f: impl Fn(f32) -> f32) -> Self {
        assert!(hi > lo, "Lut range must be increasing");
        let size = size.max(2).next_power_of_two();
        let step = (hi as f64 - lo as f64) / size as f64;
        let table = (0..=size).map(|i| f((lo as f64 + step * i as f64) as f32)).collect();
        Lut { table, lo, hi, scale: (size as f64 / (hi as f64 - lo as f64)) as f32, size }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn range(&self) -> (f32, f32) {
        (self.lo, self.hi)
    }

    /// Interpolated value. NaN in gives NaN out.
    #[inline]
    pub fn get(&self, x: f32) -> f32 {
        let p = (x.clamp(self.lo, self.hi) - self.lo) * self.scale;
        let i = (p as usize).min(self.size - 1);
        let f = p - i as f32;
        let a = self.table[i];
        a + (self.table[i + 1] - a) * f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_at_nodes_and_linear_between() {
        let l = Lut::new(0.0, 8.0, 8, |x| x * x);
        assert_eq!(l.size(), 8);
        for i in 0..=8 {
            assert_eq!(l.get(i as f32), (i * i) as f32);
        }
        assert_eq!(l.get(2.5), (4.0 + 9.0) / 2.0);
        assert_eq!(l.range(), (0.0, 8.0));
    }

    #[test]
    fn clamps_outside_the_range() {
        let l = Lut::new(-1.0, 1.0, 100, |x| x.sin());
        assert_eq!(l.size(), 128);
        assert_eq!(l.get(-5.0), l.get(-1.0));
        assert_eq!(l.get(5.0), l.get(1.0));
        assert!((l.get(1.0) - 1f32.sin()).abs() < 1e-6);
        assert!(l.get(f32::NAN).is_nan());
    }

    #[test]
    fn interpolation_error_is_small() {
        let l = Lut::new(0.0, core::f32::consts::PI, 1024, |x| x.sin());
        let mut worst = 0.0f32;
        for k in 0..10_000 {
            let x = k as f32 * core::f32::consts::PI / 10_000.0;
            worst = worst.max((l.get(x) - x.sin()).abs());
        }
        // h^2/8 * max|f''| = (pi/1024)^2 / 8
        assert!(worst < 1.2e-6, "{worst}");
    }
}
