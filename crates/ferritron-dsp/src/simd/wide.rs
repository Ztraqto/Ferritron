//! Wider vectors: [`F32x8`] (two [`F32x4`], always available) and [`F64x2`].
//!
//! `F32x8` is built from two 4-lane halves so it is safe on every CPU. Where a kernel
//! profits from real 256-bit registers (see `BiquadX8::process_interleaved`), the kernel
//! itself is compiled twice and the AVX2 copy is chosen once by [`crate::isa`]; a value
//! type cannot change its representation at run time without paying for it in every
//! operation.

use super::{F32x4, Mask4};
use core::ops::{Add, Div, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, Default)]
pub struct F32x8 {
    pub lo: F32x4,
    pub hi: F32x4,
}

#[derive(Clone, Copy)]
pub struct Mask8 {
    pub lo: Mask4,
    pub hi: Mask4,
}

impl F32x8 {
    pub const LANES: usize = 8;

    #[inline(always)]
    pub fn new(lo: F32x4, hi: F32x4) -> Self {
        F32x8 { lo, hi }
    }
    #[inline(always)]
    pub fn splat(x: f32) -> Self {
        F32x8 { lo: F32x4::splat(x), hi: F32x4::splat(x) }
    }
    #[inline(always)]
    pub fn zero() -> Self {
        Self::splat(0.0)
    }
    #[inline(always)]
    pub fn from_array(a: [f32; 8]) -> Self {
        F32x8 { lo: F32x4::from_array([a[0], a[1], a[2], a[3]]), hi: F32x4::from_array([a[4], a[5], a[6], a[7]]) }
    }
    #[inline(always)]
    pub fn to_array(self) -> [f32; 8] {
        let (l, h) = (self.lo.to_array(), self.hi.to_array());
        [l[0], l[1], l[2], l[3], h[0], h[1], h[2], h[3]]
    }
    /// Loads the first eight values of `s` (panics when shorter).
    #[inline(always)]
    pub fn load(s: &[f32]) -> Self {
        F32x8 { lo: F32x4::load(s), hi: F32x4::load(&s[4..]) }
    }
    /// Stores into the first eight values of `s` (panics when shorter).
    #[inline(always)]
    pub fn store(self, s: &mut [f32]) {
        self.lo.store(s);
        self.hi.store(&mut s[4..]);
    }
    #[inline(always)]
    pub fn lane(self, i: usize) -> f32 {
        self.to_array()[i]
    }
    #[inline(always)]
    pub fn min(self, o: Self) -> Self {
        F32x8 { lo: self.lo.min(o.lo), hi: self.hi.min(o.hi) }
    }
    #[inline(always)]
    pub fn max(self, o: Self) -> Self {
        F32x8 { lo: self.lo.max(o.lo), hi: self.hi.max(o.hi) }
    }
    #[inline(always)]
    pub fn abs(self) -> Self {
        F32x8 { lo: self.lo.abs(), hi: self.hi.abs() }
    }
    #[inline(always)]
    pub fn sqrt(self) -> Self {
        F32x8 { lo: self.lo.sqrt(), hi: self.hi.sqrt() }
    }
    #[inline(always)]
    pub fn lt(self, o: Self) -> Mask8 {
        Mask8 { lo: self.lo.lt(o.lo), hi: self.hi.lt(o.hi) }
    }
    #[inline(always)]
    pub fn le(self, o: Self) -> Mask8 {
        Mask8 { lo: self.lo.le(o.lo), hi: self.hi.le(o.hi) }
    }
    #[inline(always)]
    pub fn gt(self, o: Self) -> Mask8 {
        Mask8 { lo: self.lo.gt(o.lo), hi: self.hi.gt(o.hi) }
    }
    #[inline(always)]
    pub fn ge(self, o: Self) -> Mask8 {
        Mask8 { lo: self.lo.ge(o.lo), hi: self.hi.ge(o.hi) }
    }
    /// Sum of the lanes: `((l0+l1)+(l2+l3)) + ((l4+l5)+(l6+l7))`.
    #[inline(always)]
    pub fn sum(self) -> f32 {
        self.lo.sum() + self.hi.sum()
    }
    #[inline(always)]
    pub fn max_lane(self) -> f32 {
        self.lo.max_lane().max(self.hi.max_lane())
    }
    #[inline(always)]
    pub fn min_lane(self) -> f32 {
        self.lo.min_lane().min(self.hi.min_lane())
    }
}

impl Mask8 {
    #[inline(always)]
    pub fn select(self, a: F32x8, b: F32x8) -> F32x8 {
        F32x8 { lo: self.lo.select(a.lo, b.lo), hi: self.hi.select(a.hi, b.hi) }
    }
    #[inline(always)]
    pub fn and(self, o: Self) -> Self {
        Mask8 { lo: self.lo.and(o.lo), hi: self.hi.and(o.hi) }
    }
    #[inline(always)]
    pub fn or(self, o: Self) -> Self {
        Mask8 { lo: self.lo.or(o.lo), hi: self.hi.or(o.hi) }
    }
    #[inline(always)]
    pub fn bitmask(self) -> u32 {
        self.lo.bitmask() | (self.hi.bitmask() << 4)
    }
    #[inline(always)]
    pub fn any(self) -> bool {
        self.bitmask() != 0
    }
    #[inline(always)]
    pub fn all(self) -> bool {
        self.bitmask() == 0xff
    }
}

macro_rules! op8 {
    ($tr:ident, $f:ident) => {
        impl $tr for F32x8 {
            type Output = F32x8;
            #[inline(always)]
            fn $f(self, o: F32x8) -> F32x8 {
                F32x8 { lo: self.lo.$f(o.lo), hi: self.hi.$f(o.hi) }
            }
        }
        impl $tr<f32> for F32x8 {
            type Output = F32x8;
            #[inline(always)]
            fn $f(self, o: f32) -> F32x8 {
                self.$f(F32x8::splat(o))
            }
        }
    };
}
op8!(Add, add);
op8!(Sub, sub);
op8!(Mul, mul);
op8!(Div, div);

impl Neg for F32x8 {
    type Output = F32x8;
    #[inline(always)]
    fn neg(self) -> F32x8 {
        F32x8::zero() - self
    }
}

// ---------------------------------------------------------------------------------------
// F64x2

#[cfg(target_arch = "x86_64")]
#[allow(unused_unsafe)]
mod f64x2 {
    use core::arch::x86_64::*;
    #[derive(Clone, Copy)]
    pub struct F64x2(__m128d);
    #[derive(Clone, Copy)]
    pub struct Mask2(__m128d);
    impl F64x2 {
        #[inline(always)]
        pub fn splat(x: f64) -> Self {
            Self(unsafe { _mm_set1_pd(x) })
        }
        #[inline(always)]
        pub fn from_array(a: [f64; 2]) -> Self {
            Self(unsafe { _mm_loadu_pd(a.as_ptr()) })
        }
        #[inline(always)]
        pub fn to_array(self) -> [f64; 2] {
            let mut o = [0.0f64; 2];
            unsafe { _mm_storeu_pd(o.as_mut_ptr(), self.0) };
            o
        }
        #[inline(always)]
        pub fn add(self, o: Self) -> Self {
            Self(unsafe { _mm_add_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn sub(self, o: Self) -> Self {
            Self(unsafe { _mm_sub_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn mul(self, o: Self) -> Self {
            Self(unsafe { _mm_mul_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn div(self, o: Self) -> Self {
            Self(unsafe { _mm_div_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn min(self, o: Self) -> Self {
            Self(unsafe { _mm_min_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn max(self, o: Self) -> Self {
            Self(unsafe { _mm_max_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn abs(self) -> Self {
            Self(unsafe { _mm_andnot_pd(_mm_set1_pd(-0.0), self.0) })
        }
        #[inline(always)]
        pub fn sqrt(self) -> Self {
            Self(unsafe { _mm_sqrt_pd(self.0) })
        }
        #[inline(always)]
        pub fn lt(self, o: Self) -> Mask2 {
            Mask2(unsafe { _mm_cmplt_pd(self.0, o.0) })
        }
        #[inline(always)]
        pub fn gt(self, o: Self) -> Mask2 {
            Mask2(unsafe { _mm_cmpgt_pd(self.0, o.0) })
        }
    }
    impl Mask2 {
        #[inline(always)]
        pub fn select(self, a: F64x2, b: F64x2) -> F64x2 {
            F64x2(unsafe { _mm_or_pd(_mm_and_pd(self.0, a.0), _mm_andnot_pd(self.0, b.0)) })
        }
        #[inline(always)]
        pub fn bitmask(self) -> u32 {
            unsafe { _mm_movemask_pd(self.0) as u32 }
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod f64x2 {
    // Plain arrays: LLVM vectorises these on NEON; no intrinsics are needed for two lanes.
    #[derive(Clone, Copy)]
    pub struct F64x2([f64; 2]);
    #[derive(Clone, Copy)]
    pub struct Mask2([bool; 2]);
    macro_rules! bin {
        ($n:ident, |$x:ident, $y:ident| $e:expr) => {
            pub fn $n(self, o: Self) -> Self {
                let (a, b) = (self.0, o.0);
                let f = |$x: f64, $y: f64| $e;
                Self([f(a[0], b[0]), f(a[1], b[1])])
            }
        };
    }
    impl F64x2 {
        pub fn splat(x: f64) -> Self {
            Self([x; 2])
        }
        pub fn from_array(a: [f64; 2]) -> Self {
            Self(a)
        }
        pub fn to_array(self) -> [f64; 2] {
            self.0
        }
        bin!(add, |x, y| x + y);
        bin!(sub, |x, y| x - y);
        bin!(mul, |x, y| x * y);
        bin!(div, |x, y| x / y);
        bin!(min, |x, y| if x < y { x } else { y });
        bin!(max, |x, y| if x > y { x } else { y });
        pub fn abs(self) -> Self {
            Self([self.0[0].abs(), self.0[1].abs()])
        }
        pub fn sqrt(self) -> Self {
            Self([self.0[0].sqrt(), self.0[1].sqrt()])
        }
        pub fn lt(self, o: Self) -> Mask2 {
            Mask2([self.0[0] < o.0[0], self.0[1] < o.0[1]])
        }
        pub fn gt(self, o: Self) -> Mask2 {
            Mask2([self.0[0] > o.0[0], self.0[1] > o.0[1]])
        }
    }
    impl Mask2 {
        pub fn select(self, a: F64x2, b: F64x2) -> F64x2 {
            F64x2([if self.0[0] { a.0[0] } else { b.0[0] }, if self.0[1] { a.0[1] } else { b.0[1] }])
        }
        pub fn bitmask(self) -> u32 {
            self.0[0] as u32 | ((self.0[1] as u32) << 1)
        }
    }
}

pub use f64x2::{F64x2, Mask2};

impl F64x2 {
    pub const LANES: usize = 2;
    #[inline(always)]
    pub fn zero() -> Self {
        Self::splat(0.0)
    }
    #[inline(always)]
    pub fn load(s: &[f64]) -> Self {
        Self::from_array([s[0], s[1]])
    }
    #[inline(always)]
    pub fn store(self, s: &mut [f64]) {
        s[..2].copy_from_slice(&self.to_array());
    }
    #[inline(always)]
    pub fn lane(self, i: usize) -> f64 {
        self.to_array()[i]
    }
    /// `l0 + l1`.
    #[inline(always)]
    pub fn sum(self) -> f64 {
        let a = self.to_array();
        a[0] + a[1]
    }
    /// Widens two `f32` values.
    #[inline(always)]
    pub fn from_f32(a: f32, b: f32) -> Self {
        Self::from_array([a as f64, b as f64])
    }
    /// Narrows both lanes to `f32` (round to nearest).
    #[inline(always)]
    pub fn to_f32(self) -> [f32; 2] {
        let a = self.to_array();
        [a[0] as f32, a[1] as f32]
    }
}

impl Mask2 {
    #[inline(always)]
    pub fn any(self) -> bool {
        self.bitmask() != 0
    }
    #[inline(always)]
    pub fn all(self) -> bool {
        self.bitmask() == 0b11
    }
}

macro_rules! op64 {
    ($tr:ident, $f:ident) => {
        impl $tr for F64x2 {
            type Output = F64x2;
            #[inline(always)]
            fn $f(self, o: F64x2) -> F64x2 {
                F64x2::$f(self, o)
            }
        }
        impl $tr<f64> for F64x2 {
            type Output = F64x2;
            #[inline(always)]
            fn $f(self, o: f64) -> F64x2 {
                F64x2::$f(self, F64x2::splat(o))
            }
        }
    };
}
op64!(Add, add);
op64!(Sub, sub);
op64!(Mul, mul);
op64!(Div, div);

impl Default for F64x2 {
    fn default() -> Self {
        Self::zero()
    }
}

impl core::fmt::Debug for F64x2 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "F64x2({:?})", self.to_array())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32x8_matches_scalar() {
        let a = [1.5f32, -2.0, 0.25, 1e10, 3.0, -0.5, 7.0, 0.0];
        let b = [0.5f32, 4.0, -8.0, 3.0, -3.0, 0.5, 2.0, 1.0];
        let (x, y) = (F32x8::from_array(a), F32x8::from_array(b));
        for i in 0..8 {
            assert_eq!((x + y).lane(i), a[i] + b[i]);
            assert_eq!((x - y).lane(i), a[i] - b[i]);
            assert_eq!((x * y).lane(i), a[i] * b[i]);
            assert_eq!((x / y).lane(i), a[i] / b[i]);
            assert_eq!(x.min(y).lane(i), a[i].min(b[i]));
            assert_eq!(x.max(y).lane(i), a[i].max(b[i]));
            assert_eq!(x.abs().lane(i), a[i].abs());
            assert_eq!((-x).lane(i), -a[i]);
            assert_eq!((x * 3.0).lane(i), a[i] * 3.0);
        }
        assert_eq!(F32x8::splat(4.0).sqrt().to_array(), [2.0; 8]);
        assert_eq!(x.sum(), ((a[0] + a[1]) + (a[2] + a[3])) + ((a[4] + a[5]) + (a[6] + a[7])));
        assert_eq!(x.max_lane(), 1e10);
        assert_eq!(x.min_lane(), -2.0);
        let m = x.lt(y);
        let want: u32 = (0..8).map(|i| ((a[i] < b[i]) as u32) << i).sum();
        assert_eq!(m.bitmask(), want);
        assert_eq!(x.ge(y).bitmask(), !want & 0xff);
        assert_eq!(x.le(y).bitmask() | x.gt(y).bitmask(), 0xff);
        let s = m.select(x, y).to_array();
        for i in 0..8 {
            assert_eq!(s[i], if a[i] < b[i] { a[i] } else { b[i] });
        }
        assert!(m.or(x.ge(y)).all());
        assert!(!m.and(x.ge(y)).any());
    }

    #[test]
    fn f32x8_memory() {
        let src: Vec<f32> = (0..12).map(|i| i as f32).collect();
        let v = F32x8::load(&src[2..]);
        assert_eq!(v.to_array(), [2., 3., 4., 5., 6., 7., 8., 9.]);
        let mut dst = [0.0f32; 10];
        v.store(&mut dst[1..]);
        assert_eq!(&dst[1..9], &src[2..10]);
    }

    #[test]
    fn f64x2_matches_scalar() {
        let a = [1.5f64, -2.0];
        let b = [0.25f64, 8.0];
        let (x, y) = (F64x2::from_array(a), F64x2::from_array(b));
        for i in 0..2 {
            assert_eq!((x + y).lane(i), a[i] + b[i]);
            assert_eq!((x - y).lane(i), a[i] - b[i]);
            assert_eq!((x * y).lane(i), a[i] * b[i]);
            assert_eq!((x / y).lane(i), a[i] / b[i]);
            assert_eq!(x.min(y).lane(i), a[i].min(b[i]));
            assert_eq!(x.max(y).lane(i), a[i].max(b[i]));
            assert_eq!(x.abs().lane(i), a[i].abs());
            assert_eq!((x * 2.0).lane(i), a[i] * 2.0);
        }
        assert_eq!(F64x2::from_array([4.0, 9.0]).sqrt().to_array(), [2.0, 3.0]);
        assert_eq!(x.sum(), -0.5);
        assert_eq!(x.lt(y).bitmask(), 0b10);
        assert_eq!(x.gt(y).bitmask(), 0b01);
        let z = F64x2::from_array([0.1, 9.0]);
        assert_eq!(z.gt(y).bitmask(), 0b10);
        assert_eq!(z.gt(y).select(x, y).to_array(), [0.25, -2.0]);
        assert!(x.lt(y).any() && !x.lt(y).all());
        let mut o = [0.0f64; 3];
        x.store(&mut o[1..]);
        assert_eq!(o, [0.0, 1.5, -2.0]);
        assert_eq!(F64x2::load(&o[1..]).to_array(), a);
        assert_eq!(F64x2::from_f32(0.1, 2.0).to_f32(), [0.1, 2.0]);
        // f64 keeps what f32 cannot
        let big = F64x2::splat(16_777_216.0) + 1.0;
        assert_eq!(big.lane(0), 16_777_217.0);
    }
}
