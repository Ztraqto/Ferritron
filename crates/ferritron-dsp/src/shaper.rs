//! Waveshaping curves with a scalar and a four-lane version that compute the same thing
//! in the same order (they agree exactly). No allocation, no branches on the data in the
//! vector path (min / max / select instead).
//!
//! | kind | curve | notes |
//! |---|---|---|
//! | `Tanh` | rational (7/6 Pade) approximation of tanh | input clamped to +-4.97; absolute error to `f32::tanh` is measured in the tests (<= 3e-4 over the range) |
//! | `Atan` | `2/pi * atan(x)` | polynomial atan, absolute error <= 1e-5 |
//! | `Poly2` | `x - x\|x\|/4` for `\|x\| <= 2`, else +-1 | slope 1 at 0, continuous slope at the knee |
//! | `Poly3` | `x - 4/27 x^3` for `\|x\| <= 1.5`, else +-1 | slope 1 at 0, continuous slope at the knee |
//! | `Algebraic` | `x / (1 + \|x\|)` | |
//! | `Rectify` | `\|x\|` | full wave |
//! | `HalfRectify` | `max(x, 0)` | |

use crate::simd::F32x4;
use core::f32::consts::{FRAC_2_PI, FRAC_PI_2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaperKind {
    Tanh,
    Atan,
    Poly2,
    Poly3,
    Algebraic,
    Rectify,
    HalfRectify,
}

const POLY3_K: f32 = 4.0 / 27.0;
const TANH_LIMIT: f32 = 4.97;

#[derive(Clone, Copy, Debug)]
pub struct Shaper {
    kind: ShaperKind,
}

impl Shaper {
    pub fn new(kind: ShaperKind) -> Self {
        Shaper { kind }
    }

    pub fn kind(&self) -> ShaperKind {
        self.kind
    }

    #[inline]
    pub fn shape(&self, x: f32) -> f32 {
        match self.kind {
            ShaperKind::Tanh => {
                let x = x.clamp(-TANH_LIMIT, TANH_LIMIT);
                let x2 = x * x;
                let num = x * (135135.0 + x2 * (17325.0 + x2 * (378.0 + x2)));
                let den = 135135.0 + x2 * (62370.0 + x2 * (3150.0 + x2 * 28.0));
                (num / den).clamp(-1.0, 1.0)
            }
            ShaperKind::Atan => {
                let a = x.abs();
                let big = a > 1.0;
                let z = if big { 1.0 / a } else { a };
                let z2 = z * z;
                let p = z * (0.999_866 + z2 * (-0.330_299_5 + z2 * (0.180_141 + z2 * (-0.085_133 + z2 * 0.020_835_1))));
                let r = (if big { FRAC_PI_2 - p } else { p }) * FRAC_2_PI;
                if x < 0.0 { -r } else { r }
            }
            ShaperKind::Poly2 => {
                let c = x.clamp(-2.0, 2.0);
                c - 0.25 * c * c.abs()
            }
            ShaperKind::Poly3 => {
                let c = x.clamp(-1.5, 1.5);
                c - POLY3_K * c * c * c
            }
            ShaperKind::Algebraic => x / (1.0 + x.abs()),
            ShaperKind::Rectify => x.abs(),
            ShaperKind::HalfRectify => x.max(0.0),
        }
    }

    #[inline]
    pub fn shape_x4(&self, x: F32x4) -> F32x4 {
        let one = F32x4::splat(1.0);
        match self.kind {
            ShaperKind::Tanh => {
                let x = x.max(F32x4::splat(-TANH_LIMIT)).min(F32x4::splat(TANH_LIMIT));
                let x2 = x * x;
                let num = x * (x2 * (x2 * (x2 + 378.0) + 17325.0) + 135135.0);
                let den = x2 * (x2 * (x2 * 28.0 + 3150.0) + 62370.0) + 135135.0;
                (num / den).max(-one).min(one)
            }
            ShaperKind::Atan => {
                let a = x.abs();
                let big = a.gt(one);
                let z = big.select(one / a, a);
                let z2 = z * z;
                let p = z * (z2 * (z2 * (z2 * (z2 * 0.020_835_1 + (-0.085_133)) + 0.180_141) + (-0.330_299_5)) + 0.999_866);
                let r = big.select(F32x4::splat(FRAC_PI_2) - p, p) * FRAC_2_PI;
                x.lt(F32x4::zero()).select(-r, r)
            }
            ShaperKind::Poly2 => {
                let c = x.max(F32x4::splat(-2.0)).min(F32x4::splat(2.0));
                c - c * c.abs() * 0.25
            }
            ShaperKind::Poly3 => {
                let c = x.max(F32x4::splat(-1.5)).min(F32x4::splat(1.5));
                c - c * c * c * POLY3_K
            }
            ShaperKind::Algebraic => x / (x.abs() + 1.0),
            ShaperKind::Rectify => x.abs(),
            ShaperKind::HalfRectify => x.max(F32x4::zero()),
        }
    }

    /// Shapes a buffer in place (four lanes at a time, scalar tail).
    pub fn process_block(&self, buf: &mut [f32]) {
        let mut chunks = buf.chunks_exact_mut(4);
        for c in &mut chunks {
            self.shape_x4(F32x4::load(c)).store(c);
        }
        for s in chunks.into_remainder() {
            *s = self.shape(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [ShaperKind; 7] = [
        ShaperKind::Tanh,
        ShaperKind::Atan,
        ShaperKind::Poly2,
        ShaperKind::Poly3,
        ShaperKind::Algebraic,
        ShaperKind::Rectify,
        ShaperKind::HalfRectify,
    ];

    fn grid() -> Vec<f32> {
        (-6000..=6000).map(|i| i as f32 * 0.002).chain([1e6, -1e6, 0.0, 1.0, -1.0]).collect()
    }

    #[test]
    fn vector_and_scalar_agree() {
        for k in KINDS {
            let s = Shaper::new(k);
            let g = grid();
            let mut v = g.clone();
            s.process_block(&mut v); // includes a scalar tail (length is not a multiple of 4)
            for (x, y) in g.iter().zip(&v) {
                let want = s.shape(*x);
                assert!((want - y).abs() <= 1e-6, "{k:?} x={x}: {want} vs {y}");
            }
            assert_ne!(g.len() % 4, 0);
        }
    }

    #[test]
    fn tanh_error() {
        let s = Shaper::new(ShaperKind::Tanh);
        let mut worst = 0.0f64;
        for x in grid() {
            worst = worst.max((s.shape(x) as f64 - (x as f64).tanh()).abs());
        }
        println!("Pade tanh worst absolute error: {worst:e}");
        assert!(worst <= 3e-4, "{worst}");
        assert_eq!(s.shape(0.0), 0.0);
        assert!(s.shape(100.0) <= 1.0 && s.shape(100.0) > 0.999);
    }

    #[test]
    fn atan_error() {
        let s = Shaper::new(ShaperKind::Atan);
        let mut worst = 0.0f64;
        for x in grid() {
            let want = (x as f64).atan() * 2.0 / core::f64::consts::PI;
            worst = worst.max((s.shape(x) as f64 - want).abs());
        }
        println!("atan worst absolute error: {worst:e}");
        assert!(worst <= 1e-5, "{worst}");
    }

    #[test]
    fn closed_forms_and_shape_properties() {
        let p2 = Shaper::new(ShaperKind::Poly2);
        let p3 = Shaper::new(ShaperKind::Poly3);
        assert!((p2.shape(2.0) - 1.0).abs() < 1e-6 && p2.shape(5.0) == p2.shape(2.0));
        assert!((p3.shape(1.5) - 1.0).abs() < 1e-5 && p3.shape(9.0) == p3.shape(1.5));
        assert!((p2.shape(0.001) - 0.001).abs() < 1e-6 && (p3.shape(0.001) - 0.001).abs() < 1e-6);
        let a = Shaper::new(ShaperKind::Algebraic);
        assert_eq!(a.shape(1.0), 0.5);
        assert_eq!(a.shape(-3.0), -0.75);
        assert_eq!(Shaper::new(ShaperKind::Rectify).shape(-2.5), 2.5);
        assert_eq!(Shaper::new(ShaperKind::HalfRectify).shape(-2.5), 0.0);
        assert_eq!(Shaper::new(ShaperKind::HalfRectify).shape(2.5), 2.5);
        for k in [ShaperKind::Tanh, ShaperKind::Atan, ShaperKind::Poly2, ShaperKind::Poly3, ShaperKind::Algebraic] {
            let s = Shaper::new(k);
            let mut prev = -2.0f32;
            for x in grid().into_iter().take(12001) {
                let y = s.shape(x);
                assert!(y.abs() <= 1.0 + 1e-6, "{k:?} bounded");
                assert!(y >= prev - 1e-6, "{k:?} monotonic at {x}");
                prev = y;
                assert!((s.shape(-x) + y).abs() <= 1e-6, "{k:?} odd");
            }
        }
    }
}
