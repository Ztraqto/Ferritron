//! Second-order sections in transposed direct form II. Coefficients are designed in `f64`
//! ([`crate::design`]) and run in `f32`. [`BiquadX4`] runs four independent filters
//! (channels, bands, voices) in one SIMD register; it performs the same operations in the
//! same order as [`Biquad`], so the results are identical, also while the coefficients
//! move towards a target. The state is flushed to zero below [`crate::DENORMAL_FLOOR`].

use crate::simd::F32x4;
use crate::{DENORMAL_FLOOR, flush_denormal};

/// Normalised coefficients (`a0 = 1`): `y = b0 x + z1`, `z1 = b1 x - a1 y + z2`, `z2 = b2 x - a2 y`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coefs {
    pub const fn passthrough() -> Self {
        Coefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }

    /// Both poles strictly inside the unit circle.
    pub fn is_stable(&self) -> bool {
        self.a2.abs() < 1.0 && self.a1.abs() < 1.0 + self.a2
    }

    /// Gain at the angular frequency `w` (radians per sample).
    pub fn magnitude(&self, w: f64) -> f64 {
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        ((nr * nr + ni * ni) / (dr * dr + di * di)).sqrt()
    }

    /// Gain in dB at `f` Hz for sample rate `fs`.
    pub fn magnitude_db(&self, fs: f64, f: f64) -> f64 {
        20.0 * self.magnitude(2.0 * core::f64::consts::PI * f / fs).max(1e-30).log10()
    }

    fn to_f32(self) -> [f32; 5] {
        [self.b0 as f32, self.b1 as f32, self.b2 as f32, self.a1 as f32, self.a2 as f32]
    }
}

impl Default for Coefs {
    fn default() -> Self {
        Self::passthrough()
    }
}

/// One filter.
#[derive(Clone, Debug)]
pub struct Biquad {
    c: [f32; 5],
    d: [f32; 5],
    t: [f32; 5],
    remaining: u32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new(&Coefs::passthrough())
    }
}

impl Biquad {
    pub fn new(c: &Coefs) -> Self {
        let c = c.to_f32();
        Biquad { c, d: [0.0; 5], t: c, remaining: 0, z1: 0.0, z2: 0.0 }
    }

    /// Takes the coefficients at once.
    pub fn set_coefs(&mut self, c: &Coefs) {
        self.c = c.to_f32();
        self.t = self.c;
        self.remaining = 0;
    }

    /// Moves the coefficients linearly to `c` over `samples` samples (0 = at once).
    /// Linear steps between two stable sets are not guaranteed stable in theory; check
    /// [`Coefs::is_stable`] for extreme jumps.
    pub fn set_target(&mut self, c: &Coefs, samples: u32) {
        if samples == 0 {
            return self.set_coefs(c);
        }
        self.t = c.to_f32();
        for i in 0..5 {
            self.d[i] = (self.t[i] - self.c[i]) / samples as f32;
        }
        self.remaining = samples;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    pub fn is_interpolating(&self) -> bool {
        self.remaining > 0
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.c = self.t;
            } else {
                for i in 0..5 {
                    self.c[i] += self.d[i];
                }
            }
        }
        let [b0, b1, b2, a1, a2] = self.c;
        let y = b0 * x + self.z1;
        let n1 = (b1 * x - a1 * y) + self.z2;
        let n2 = b2 * x - a2 * y;
        self.z1 = flush_denormal(n1);
        self.z2 = flush_denormal(n2);
        y
    }

    pub fn process_block(&mut self, buf: &mut [f32]) {
        for s in buf {
            *s = self.process(*s);
        }
    }
}

/// Four independent filters in one SIMD register (lane `i` is filter `i`).
#[derive(Clone, Debug)]
pub struct BiquadX4 {
    c: [F32x4; 5],
    d: [F32x4; 5],
    t: [F32x4; 5],
    remaining: u32,
    z1: F32x4,
    z2: F32x4,
}

impl Default for BiquadX4 {
    fn default() -> Self {
        Self::new(&[Coefs::passthrough(); 4])
    }
}

fn pack(lanes: &[[f32; 5]; 4]) -> [F32x4; 5] {
    core::array::from_fn(|i| F32x4::from_array([lanes[0][i], lanes[1][i], lanes[2][i], lanes[3][i]]))
}

fn unpack(v: &[F32x4; 5]) -> [[f32; 5]; 4] {
    let a: [[f32; 4]; 5] = core::array::from_fn(|i| v[i].to_array());
    core::array::from_fn(|l| core::array::from_fn(|i| a[i][l]))
}

impl BiquadX4 {
    pub fn new(c: &[Coefs; 4]) -> Self {
        let p = pack(&core::array::from_fn(|i| c[i].to_f32()));
        BiquadX4 { c: p, d: [F32x4::zero(); 5], t: p, remaining: 0, z1: F32x4::zero(), z2: F32x4::zero() }
    }

    pub fn set_coefs(&mut self, c: &[Coefs; 4]) {
        self.c = pack(&core::array::from_fn(|i| c[i].to_f32()));
        self.t = self.c;
        self.remaining = 0;
    }

    /// Sets the coefficients of one lane at once (other lanes unchanged; ends any interpolation).
    pub fn set_lane(&mut self, lane: usize, c: &Coefs) {
        let mut l = unpack(&self.c);
        l[lane] = c.to_f32();
        self.c = pack(&l);
        self.t = self.c;
        self.remaining = 0;
    }

    /// Moves all lanes linearly to `c` over `samples` samples (0 = at once).
    pub fn set_target(&mut self, c: &[Coefs; 4], samples: u32) {
        if samples == 0 {
            return self.set_coefs(c);
        }
        let cur = unpack(&self.c);
        let tgt: [[f32; 5]; 4] = core::array::from_fn(|i| c[i].to_f32());
        let mut d = [[0.0f32; 5]; 4];
        for l in 0..4 {
            for i in 0..5 {
                d[l][i] = (tgt[l][i] - cur[l][i]) / samples as f32;
            }
        }
        self.t = pack(&tgt);
        self.d = pack(&d);
        self.remaining = samples;
    }

    pub fn reset(&mut self) {
        self.z1 = F32x4::zero();
        self.z2 = F32x4::zero();
    }

    pub fn is_interpolating(&self) -> bool {
        self.remaining > 0
    }

    #[inline(always)]
    pub fn process(&mut self, x: F32x4) -> F32x4 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.c = self.t;
            } else {
                for i in 0..5 {
                    self.c[i] += self.d[i];
                }
            }
        }
        let [b0, b1, b2, a1, a2] = self.c;
        let y = b0 * x + self.z1;
        let n1 = (b1 * x - a1 * y) + self.z2;
        let n2 = b2 * x - a2 * y;
        let floor = F32x4::splat(DENORMAL_FLOOR);
        let zero = F32x4::zero();
        self.z1 = n1.abs().lt(floor).select(zero, n1);
        self.z2 = n2.abs().lt(floor).select(zero, n2);
        y
    }

    /// Filters four buffers in place (lane `i` = buffer `i`); the shortest length is processed.
    pub fn process_block(&mut self, bufs: [&mut [f32]; 4]) {
        let [a, b, c, d] = bufs;
        let n = a.len().min(b.len()).min(c.len()).min(d.len());
        for i in 0..n {
            let y = self.process(F32x4::from_array([a[i], b[i], c[i], d[i]])).to_array();
            a[i] = y[0];
            b[i] = y[1];
            c[i] = y[2];
            d[i] = y[3];
        }
    }
}

/// Eight independent filters (lane `i` is filter `i`). Same arithmetic, in the same order,
/// as [`Biquad`], so the output is identical to eight scalar filters. The block methods
/// use a 256-bit AVX2 kernel when [`crate::isa`] says AVX2 is there (chosen once in the
/// constructor) and two 128-bit halves otherwise; neither path uses FMA.
#[derive(Clone, Debug)]
pub struct BiquadX8 {
    c: [[f32; 8]; 5],
    d: [[f32; 8]; 5],
    t: [[f32; 8]; 5],
    remaining: u32,
    z1: [f32; 8],
    z2: [f32; 8],
    avx2: bool,
}

type Lanes = [[f32; 8]; 5];

fn pack8(lanes: &[[f32; 5]; 8]) -> Lanes {
    core::array::from_fn(|i| core::array::from_fn(|l| lanes[l][i]))
}

type Kernel8 = fn(&Lanes, &mut [f32; 8], &mut [f32; 8], &mut [f32]);

fn kernel8_portable(c: &Lanes, z1: &mut [f32; 8], z2: &mut [f32; 8], buf: &mut [f32]) {
    use crate::simd::F32x8;
    let [b0, b1, b2, a1, a2] = core::array::from_fn(|i| F32x8::from_array(c[i]));
    let (mut s1, mut s2) = (F32x8::from_array(*z1), F32x8::from_array(*z2));
    let floor = F32x8::splat(DENORMAL_FLOOR);
    let zero = F32x8::zero();
    for frame in buf.chunks_exact_mut(8) {
        let x = F32x8::load(frame);
        let y = b0 * x + s1;
        let n1 = (b1 * x - a1 * y) + s2;
        let n2 = b2 * x - a2 * y;
        s1 = n1.abs().lt(floor).select(zero, n1);
        s2 = n2.abs().lt(floor).select(zero, n2);
        y.store(frame);
    }
    *z1 = s1.to_array();
    *z2 = s2.to_array();
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn kernel8_avx2_impl(c: &Lanes, z1: &mut [f32; 8], z2: &mut [f32; 8], buf: &mut [f32]) {
    use core::arch::x86_64::*;
    unsafe {
        let b0 = _mm256_loadu_ps(c[0].as_ptr());
        let b1 = _mm256_loadu_ps(c[1].as_ptr());
        let b2 = _mm256_loadu_ps(c[2].as_ptr());
        let a1 = _mm256_loadu_ps(c[3].as_ptr());
        let a2 = _mm256_loadu_ps(c[4].as_ptr());
        let mut s1 = _mm256_loadu_ps(z1.as_ptr());
        let mut s2 = _mm256_loadu_ps(z2.as_ptr());
        let floor = _mm256_set1_ps(DENORMAL_FLOOR);
        let sign = _mm256_set1_ps(-0.0);
        let zero = _mm256_setzero_ps();
        for frame in buf.chunks_exact_mut(8) {
            let x = _mm256_loadu_ps(frame.as_ptr());
            let y = _mm256_add_ps(_mm256_mul_ps(b0, x), s1);
            let n1 = _mm256_add_ps(_mm256_sub_ps(_mm256_mul_ps(b1, x), _mm256_mul_ps(a1, y)), s2);
            let n2 = _mm256_sub_ps(_mm256_mul_ps(b2, x), _mm256_mul_ps(a2, y));
            let m1 = _mm256_cmp_ps::<_CMP_LT_OQ>(_mm256_andnot_ps(sign, n1), floor);
            let m2 = _mm256_cmp_ps::<_CMP_LT_OQ>(_mm256_andnot_ps(sign, n2), floor);
            s1 = _mm256_blendv_ps(n1, zero, m1);
            s2 = _mm256_blendv_ps(n2, zero, m2);
            _mm256_storeu_ps(frame.as_mut_ptr(), y);
        }
        _mm256_storeu_ps(z1.as_mut_ptr(), s1);
        _mm256_storeu_ps(z2.as_mut_ptr(), s2);
    }
}

#[cfg(target_arch = "x86_64")]
fn kernel8_avx2(c: &Lanes, z1: &mut [f32; 8], z2: &mut [f32; 8], buf: &mut [f32]) {
    // SAFETY: only selected when AVX2 was detected (`BiquadX8::with_isa`).
    unsafe { kernel8_avx2_impl(c, z1, z2, buf) }
}

impl Default for BiquadX8 {
    fn default() -> Self {
        Self::new(&[Coefs::passthrough(); 8])
    }
}

impl BiquadX8 {
    pub fn new(c: &[Coefs; 8]) -> Self {
        Self::with_isa(c, crate::isa())
    }

    /// Like [`BiquadX8::new`] with an explicit level (tests, comparisons). Asking for more
    /// than the CPU has gives the portable path.
    pub fn with_isa(c: &[Coefs; 8], isa: crate::Isa) -> Self {
        let p = pack8(&core::array::from_fn(|i| c[i].to_f32()));
        let avx2 = cfg!(target_arch = "x86_64")
            && isa.supports(crate::Isa::Avx2)
            && crate::isa::detect().supports(crate::Isa::Avx2);
        BiquadX8 { c: p, d: [[0.0; 8]; 5], t: p, remaining: 0, z1: [0.0; 8], z2: [0.0; 8], avx2 }
    }

    /// True when the block methods run the 256-bit AVX2 kernel.
    pub fn uses_avx2(&self) -> bool {
        self.avx2
    }

    pub fn set_coefs(&mut self, c: &[Coefs; 8]) {
        self.c = pack8(&core::array::from_fn(|i| c[i].to_f32()));
        self.t = self.c;
        self.remaining = 0;
    }

    /// Moves all lanes linearly to `c` over `samples` frames (0 = at once).
    pub fn set_target(&mut self, c: &[Coefs; 8], samples: u32) {
        if samples == 0 {
            return self.set_coefs(c);
        }
        let tgt = pack8(&core::array::from_fn(|i| c[i].to_f32()));
        for i in 0..5 {
            for l in 0..8 {
                self.d[i][l] = (tgt[i][l] - self.c[i][l]) / samples as f32;
            }
        }
        self.t = tgt;
        self.remaining = samples;
    }

    pub fn reset(&mut self) {
        self.z1 = [0.0; 8];
        self.z2 = [0.0; 8];
    }

    pub fn is_interpolating(&self) -> bool {
        self.remaining > 0
    }

    /// One frame.
    pub fn process(&mut self, x: crate::simd::F32x8) -> crate::simd::F32x8 {
        let mut f = x.to_array();
        self.process_interleaved(&mut f);
        crate::simd::F32x8::from_array(f)
    }

    /// Filters interleaved frames in place: `buf[8 * n + lane]`. A trailing partial frame is left alone.
    pub fn process_interleaved(&mut self, buf: &mut [f32]) {
        let frames = buf.len() / 8;
        let mut done = 0;
        while done < frames && self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.c = self.t;
            } else {
                for i in 0..5 {
                    for l in 0..8 {
                        self.c[i][l] += self.d[i][l];
                    }
                }
            }
            kernel8_portable(&self.c, &mut self.z1, &mut self.z2, &mut buf[done * 8..done * 8 + 8]);
            done += 1;
        }
        if done < frames {
            let k: Kernel8 = self.kernel();
            k(&self.c, &mut self.z1, &mut self.z2, &mut buf[done * 8..frames * 8]);
        }
    }

    fn kernel(&self) -> Kernel8 {
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            return kernel8_avx2;
        }
        kernel8_portable
    }

    /// Filters eight buffers in place (lane `i` = buffer `i`); the shortest length is processed.
    /// Works through a small stack buffer: no allocation.
    pub fn process_block(&mut self, bufs: [&mut [f32]; 8]) {
        const CHUNK: usize = 64;
        let n = bufs.iter().map(|b| b.len()).min().unwrap_or(0);
        let mut tmp = [0.0f32; CHUNK * 8];
        let mut start = 0;
        while start < n {
            let len = CHUNK.min(n - start);
            for f in 0..len {
                for l in 0..8 {
                    tmp[f * 8 + l] = bufs[l][start + f];
                }
            }
            self.process_interleaved(&mut tmp[..len * 8]);
            for f in 0..len {
                for l in 0..8 {
                    bufs[l][start + f] = tmp[f * 8 + l];
                }
            }
            start += len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn set() -> [Coefs; 4] {
        [
            design::peaking(48_000.0, 1000.0, 1.0, 9.0),
            design::lowpass(48_000.0, 300.0, 0.7),
            design::high_shelf(48_000.0, 6000.0, 0.7, -8.0),
            design::notch(48_000.0, 2500.0, 3.0),
        ]
    }

    #[test]
    fn impulse_response_matches_the_difference_equation() {
        let c = design::lowpass(48_000.0, 1000.0, 0.8);
        let mut f = Biquad::new(&c);
        let mut y = vec![0.0f32; 64];
        y[0] = 1.0;
        f.process_block(&mut y);
        // reference in f64
        let (mut x1, mut x2, mut y1, mut y2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (n, got) in y.iter().enumerate() {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let out = c.b0 * x + c.b1 * x1 + c.b2 * x2 - c.a1 * y1 - c.a2 * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = out;
            assert!((out - *got as f64).abs() < 1e-6, "n={n}");
        }
    }

    #[test]
    fn x4_equals_four_scalar_filters() {
        let cs = set();
        let mut x4 = BiquadX4::new(&cs);
        let mut sc: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut worst = 0.0f32;
        for n in 0..20_000 {
            let x = [rng.next(), rng.next() * 0.5, rng.next() * 2.0, rng.next()];
            let y = x4.process(F32x4::from_array(x)).to_array();
            for l in 0..4 {
                let r = sc[l].process(x[l]);
                worst = worst.max((y[l] - r).abs());
            }
            if n == 5000 {
                let next = [cs[1], cs[2], cs[3], cs[0]];
                x4.set_target(&next, 256);
                for (s, c) in sc.iter_mut().zip(next) {
                    s.set_target(&c, 256);
                }
            }
        }
        assert!(worst <= 1e-6, "worst difference {worst}");
        assert!(!x4.is_interpolating());
    }

    #[test]
    fn x4_block_and_lane_setters() {
        let cs = set();
        let mut x4 = BiquadX4::new(&cs);
        let mut b: Vec<Vec<f32>> = (0..4).map(|l| (0..100).map(|i| ((i * (l + 3)) % 17) as f32 / 17.0 - 0.5).collect()).collect();
        let mut s: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
        let want: Vec<Vec<f32>> = b
            .iter()
            .enumerate()
            .map(|(l, v)| {
                let mut v = v.clone();
                s[l].process_block(&mut v);
                v
            })
            .collect();
        let [b0, b1, b2, b3] = &mut b[..] else { unreachable!() };
        x4.process_block([b0, b1, b2, b3]);
        for l in 0..4 {
            for i in 0..100 {
                assert!((b[l][i] - want[l][i]).abs() <= 1e-6);
            }
        }
        // set_lane changes only that lane
        let mut y = BiquadX4::new(&cs);
        y.set_lane(2, &Coefs::passthrough());
        let out = y.process(F32x4::splat(1.0)).to_array();
        assert_eq!(out[2], 1.0);
        let mut ref0 = Biquad::new(&cs[0]);
        assert_eq!(out[0], ref0.process(1.0));
    }

    #[test]
    fn states_do_not_stay_denormal() {
        let c = design::lowpass(48_000.0, 200.0, 0.7);
        let mut f = Biquad::new(&c);
        let mut x4 = BiquadX4::new(&[c; 4]);
        f.process(1.0);
        x4.process(F32x4::splat(1.0));
        for _ in 0..200_000 {
            f.process(0.0);
            x4.process(F32x4::zero());
        }
        assert_eq!(f.z1, 0.0);
        assert_eq!(f.z2, 0.0);
        assert_eq!(x4.z1.to_array(), [0.0; 4]);
        assert_eq!(x4.process(F32x4::zero()).to_array(), [0.0; 4]);
        assert!(f.process(0.0).abs() < 1e-19 || f.process(0.0) == 0.0);
    }

    #[test]
    fn coefficient_ramp_ends_on_target_and_is_continuous() {
        let a = design::peaking(48_000.0, 500.0, 1.0, 0.0);
        let b = design::peaking(48_000.0, 500.0, 1.0, 12.0);
        let mut f = Biquad::new(&a);
        f.set_target(&b, 64);
        for _ in 0..64 {
            f.process(0.1);
        }
        assert!(!f.is_interpolating());
        assert_eq!(f.c, b.to_f32());
    }

    fn set8() -> [Coefs; 8] {
        let s = set();
        [
            s[0],
            s[1],
            s[2],
            s[3],
            design::bandpass(48_000.0, 4000.0, 2.0),
            design::highpass(48_000.0, 90.0, 0.7),
            design::low_shelf(48_000.0, 120.0, 0.7, 6.0),
            design::allpass(48_000.0, 700.0, 1.0),
        ]
    }

    #[test]
    fn x8_equals_eight_scalar_filters_on_every_path() {
        let cs = set8();
        let next = [cs[1], cs[2], cs[3], cs[4], cs[5], cs[6], cs[7], cs[0]];
        let avx_available = crate::isa::detect().supports(crate::Isa::Avx2);
        for isa in [crate::Isa::Scalar, crate::Isa::Avx2] {
            let mut x8 = BiquadX8::with_isa(&cs, isa);
            assert_eq!(x8.uses_avx2(), isa == crate::Isa::Avx2 && avx_available);
            let mut sc: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
            let mut rng = Rng(0xfeed_beef_1234_5678);
            let frames = 6000;
            let mut buf: Vec<f32> = (0..frames * 8).map(|_| rng.next()).collect();
            let mut want = buf.clone();
            for n in 0..frames {
                if n == 2000 {
                    for (s, c) in sc.iter_mut().zip(next) {
                        s.set_target(&c, 300);
                    }
                }
                for l in 0..8 {
                    want[n * 8 + l] = sc[l].process(want[n * 8 + l]);
                }
            }
            let mut at = 0;
            for chunk in [1999usize, 1, 17, 983, 3000] {
                if at == 2000 {
                    x8.set_target(&next, 300);
                }
                x8.process_interleaved(&mut buf[at * 8..(at + chunk) * 8]);
                at += chunk;
            }
            assert_eq!(at, frames);
            let worst = buf.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst <= 1e-6, "{isa:?}: {worst}");
            assert!(!x8.is_interpolating());
        }
    }

    #[test]
    fn x8_block_api_and_denormals() {
        let cs = set8();
        let mut x8 = BiquadX8::new(&cs);
        let mut sc: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
        let mut b: Vec<Vec<f32>> =
            (0..8).map(|l| (0..200).map(|i| ((i * (l + 3)) % 19) as f32 / 19.0 - 0.5).collect()).collect();
        let want: Vec<Vec<f32>> = b
            .iter()
            .enumerate()
            .map(|(l, v)| {
                let mut v = v.clone();
                sc[l].process_block(&mut v);
                v
            })
            .collect();
        let mut it = b.iter_mut();
        let arr: [&mut [f32]; 8] = core::array::from_fn(|_| it.next().unwrap().as_mut_slice());
        x8.process_block(arr);
        for l in 0..8 {
            for i in 0..200 {
                assert!((b[l][i] - want[l][i]).abs() <= 1e-6);
            }
        }
        let mut one = [0.0f32; 8];
        one[0] = 1.0;
        let mut f = BiquadX8::new(&[design::lowpass(48_000.0, 200.0, 0.7); 8]);
        f.process_interleaved(&mut one);
        let mut z = vec![0.0f32; 8 * 100_000];
        f.process_interleaved(&mut z);
        assert_eq!(f.z1, [0.0; 8]);
        assert_eq!(f.z2, [0.0; 8]);
        assert_eq!(f.process(crate::simd::F32x8::zero()).to_array(), [0.0; 8]);
    }

    #[test]
    #[ignore]
    fn bench_x8_against_eight_scalar_filters() {
        use std::hint::black_box;
        use std::time::Instant;
        let cs = set8();
        let n = 4096usize;
        let mut rng = Rng(11);
        let input: Vec<f32> = (0..n * 8).map(|_| rng.next()).collect();
        let mut sc: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
        let mut chans: Vec<Vec<f32>> = (0..8).map(|l| (0..n).map(|i| input[i * 8 + l]).collect()).collect();
        let reps = 1000;
        let t0 = Instant::now();
        for _ in 0..reps {
            for l in 0..8 {
                for i in 0..n {
                    chans[l][i] = input[i * 8 + l];
                }
                sc[l].process_block(black_box(&mut chans[l]));
            }
        }
        let scalar = t0.elapsed();
        black_box(&chans);
        let mut buf = input.clone();
        let mut x8 = BiquadX8::new(&cs);
        let t1 = Instant::now();
        for _ in 0..reps {
            buf.copy_from_slice(&input);
            x8.process_interleaved(black_box(&mut buf));
        }
        let simd = t1.elapsed();
        black_box(&buf);
        println!(
            "scalar x8: {scalar:?}, BiquadX8 (avx2={}): {simd:?}, speed-up {:.2}x",
            x8.uses_avx2(),
            scalar.as_secs_f64() / simd.as_secs_f64()
        );
    }

    /// `cargo test --release -p ferritron-dsp -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_x4_against_four_scalar_filters() {
        use std::hint::black_box;
        use std::time::Instant;
        let cs = set();
        let n = 4096usize;
        let mut rng = Rng(7);
        let input: Vec<Vec<f32>> = (0..4).map(|_| (0..n).map(|_| rng.next()).collect()).collect();
        let mut sc: Vec<Biquad> = cs.iter().map(Biquad::new).collect();
        let mut x4 = BiquadX4::new(&cs);
        let reps = 2000;
        let mut bufs = input.clone();
        let t0 = Instant::now();
        for _ in 0..reps {
            for l in 0..4 {
                bufs[l].copy_from_slice(&input[l]);
                sc[l].process_block(black_box(&mut bufs[l]));
            }
        }
        let scalar = t0.elapsed();
        black_box(&bufs);
        let mut bufs = input.clone();
        let t1 = Instant::now();
        for _ in 0..reps {
            for l in 0..4 {
                bufs[l].copy_from_slice(&input[l]);
            }
            let [a, b, c, d] = &mut bufs[..] else { unreachable!() };
            x4.process_block(black_box([a, b, c, d]));
        }
        let simd = t1.elapsed();
        black_box(&bufs);
        let ratio = scalar.as_secs_f64() / simd.as_secs_f64();
        println!("scalar x4: {scalar:?}, BiquadX4: {simd:?}, speed-up {ratio:.2}x");
        assert!(ratio > 1.0, "BiquadX4 should beat four scalar filters, got {ratio:.2}x");
    }
}
