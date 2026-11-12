//! TPDF dither with a seedable generator (xorshift64*), so tests and renders repeat.

#[derive(Clone, Debug)]
pub struct Dither {
    state: u64,
    lsb: f32,
}

impl Dither {
    /// `bits` is the target word length (full scale is +-1, so one step is `2^(1 - bits)`).
    /// The same seed gives the same sequence. Seed 0 is replaced by a fixed value.
    pub fn new(seed: u64, bits: u32) -> Self {
        let mut d = Dither { state: 0, lsb: 0.0 };
        d.reseed(seed);
        d.lsb = 2f32.powi(1 - bits.clamp(1, 32) as i32);
        d
    }

    pub fn reseed(&mut self, seed: u64) {
        self.state = if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed };
    }

    pub fn lsb(&self) -> f32 {
        self.lsb
    }

    /// Uniform in `[0, 1)`.
    #[inline]
    pub fn uniform(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let r = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (r >> 40) as f32 / (1u32 << 24) as f32
    }

    /// Triangular noise in `(-1, 1)`.
    #[inline]
    pub fn tpdf(&mut self) -> f32 {
        let a = self.uniform();
        a - self.uniform()
    }

    /// `x` plus TPDF noise of +-1 step (no rounding).
    #[inline]
    pub fn add_noise(&mut self, x: f32) -> f32 {
        x + self.tpdf() * self.lsb
    }

    /// Dither, then round to the word length.
    #[inline]
    pub fn quantize(&mut self, x: f32) -> f32 {
        let v = x + self.tpdf() * self.lsb;
        crate::fastmath::round_nearest(v / self.lsb) * self.lsb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Dither::new(42, 16);
        let mut b = Dither::new(42, 16);
        let mut c = Dither::new(43, 16);
        let va: Vec<f32> = (0..64).map(|_| a.tpdf()).collect();
        let vb: Vec<f32> = (0..64).map(|_| b.tpdf()).collect();
        let vc: Vec<f32> = (0..64).map(|_| c.tpdf()).collect();
        assert_eq!(va, vb);
        assert_ne!(va, vc);
        a.reseed(42);
        assert_eq!(a.tpdf(), va[0]);
        let mut z = Dither::new(0, 16);
        assert!(z.uniform() != 0.0 || z.uniform() != 0.0);
    }

    #[test]
    fn tpdf_statistics() {
        let mut d = Dither::new(1, 16);
        let n = 400_000;
        let (mut sum, mut sq, mut lo, mut hi) = (0.0f64, 0.0f64, 1.0f32, -1.0f32);
        for _ in 0..n {
            let v = d.tpdf();
            sum += v as f64;
            sq += (v * v) as f64;
            lo = lo.min(v);
            hi = hi.max(v);
        }
        let mean = sum / n as f64;
        let var = sq / n as f64 - mean * mean;
        assert!(mean.abs() < 0.005, "{mean}");
        assert!((var - 1.0 / 6.0).abs() < 0.005, "{var}");
        assert!(lo > -1.0 && hi < 1.0);
    }

    #[test]
    fn quantize_lands_on_the_grid_and_is_unbiased() {
        let mut d = Dither::new(7, 8);
        let lsb = d.lsb();
        assert_eq!(lsb, 2f32.powi(-7));
        let x = 0.3 * lsb;
        let n = 200_000;
        let mut sum = 0.0f64;
        for _ in 0..n {
            let q = d.quantize(x);
            assert!(((q / lsb) - (q / lsb).round()).abs() < 1e-4);
            sum += q as f64;
        }
        let mean = sum / n as f64;
        assert!((mean - x as f64).abs() < 0.01 * lsb as f64, "{mean} vs {x}");
        // exact multiples of a step plus noise below one step never move more than one step
        let mut e = Dither::new(9, 12);
        for _ in 0..1000 {
            assert!((e.add_noise(0.5) - 0.5).abs() <= e.lsb());
        }
    }
}
