//! Four-lane `f32` vector on top of `core::arch`: SSE2 on x86_64 (always available there),
//! NEON on aarch64, plain arrays elsewhere. The type is chosen at compile time; use
//! [`crate::isa`] to pick between wider paths. `min` / `max` treat NaN differently on
//! each backend. The horizontal operations add in a fixed order on every backend.
//! There is no fused multiply-add: `a * b + c` is a multiply then an add everywhere, so
//! results do not depend on the CPU.

mod wide;
pub use wide::{F32x8, F64x2, Mask2, Mask8};

use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

#[cfg(target_arch = "x86_64")]
#[allow(unused_unsafe)]
mod backend {
    use core::arch::x86_64::*;
    #[derive(Clone, Copy)]
    pub struct F32x4(__m128);
    #[derive(Clone, Copy)]
    pub struct Mask4(__m128);
    impl F32x4 {
        #[inline(always)]
        pub fn splat(x: f32) -> Self {
            Self(unsafe { _mm_set1_ps(x) })
        }
        #[inline(always)]
        pub fn from_array(a: [f32; 4]) -> Self {
            Self(unsafe { _mm_loadu_ps(a.as_ptr()) })
        }
        #[inline(always)]
        pub fn to_array(self) -> [f32; 4] {
            let mut o = [0.0f32; 4];
            unsafe { _mm_storeu_ps(o.as_mut_ptr(), self.0) };
            o
        }
        #[inline(always)]
        pub fn add(self, o: Self) -> Self {
            Self(unsafe { _mm_add_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn sub(self, o: Self) -> Self {
            Self(unsafe { _mm_sub_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn mul(self, o: Self) -> Self {
            Self(unsafe { _mm_mul_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn div(self, o: Self) -> Self {
            Self(unsafe { _mm_div_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn min(self, o: Self) -> Self {
            Self(unsafe { _mm_min_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn max(self, o: Self) -> Self {
            Self(unsafe { _mm_max_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn abs(self) -> Self {
            Self(unsafe { _mm_andnot_ps(_mm_set1_ps(-0.0), self.0) })
        }
        #[inline(always)]
        pub fn sqrt(self) -> Self {
            Self(unsafe { _mm_sqrt_ps(self.0) })
        }
        #[inline(always)]
        pub fn lt(self, o: Self) -> Mask4 {
            Mask4(unsafe { _mm_cmplt_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn le(self, o: Self) -> Mask4 {
            Mask4(unsafe { _mm_cmple_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn gt(self, o: Self) -> Mask4 {
            Mask4(unsafe { _mm_cmpgt_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn ge(self, o: Self) -> Mask4 {
            Mask4(unsafe { _mm_cmpge_ps(self.0, o.0) })
        }
    }
    impl Mask4 {
        /// Lanes of `a` where the mask is set, else lanes of `b`.
        #[inline(always)]
        pub fn select(self, a: F32x4, b: F32x4) -> F32x4 {
            F32x4(unsafe { _mm_or_ps(_mm_and_ps(self.0, a.0), _mm_andnot_ps(self.0, b.0)) })
        }
        #[inline(always)]
        pub fn and(self, o: Self) -> Self {
            Self(unsafe { _mm_and_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn or(self, o: Self) -> Self {
            Self(unsafe { _mm_or_ps(self.0, o.0) })
        }
        #[inline(always)]
        pub fn bitmask(self) -> u32 {
            unsafe { _mm_movemask_ps(self.0) as u32 }
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod backend {
    use core::arch::aarch64::*;
    #[derive(Clone, Copy)]
    pub struct F32x4(float32x4_t);
    #[derive(Clone, Copy)]
    pub struct Mask4(uint32x4_t);
    impl F32x4 {
        #[inline(always)]
        pub fn splat(x: f32) -> Self {
            Self(unsafe { vdupq_n_f32(x) })
        }
        #[inline(always)]
        pub fn from_array(a: [f32; 4]) -> Self {
            Self(unsafe { vld1q_f32(a.as_ptr()) })
        }
        #[inline(always)]
        pub fn to_array(self) -> [f32; 4] {
            let mut o = [0.0f32; 4];
            unsafe { vst1q_f32(o.as_mut_ptr(), self.0) };
            o
        }
        #[inline(always)]
        pub fn add(self, o: Self) -> Self {
            Self(unsafe { vaddq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn sub(self, o: Self) -> Self {
            Self(unsafe { vsubq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn mul(self, o: Self) -> Self {
            Self(unsafe { vmulq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn div(self, o: Self) -> Self {
            Self(unsafe { vdivq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn min(self, o: Self) -> Self {
            Self(unsafe { vminq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn max(self, o: Self) -> Self {
            Self(unsafe { vmaxq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn abs(self) -> Self {
            Self(unsafe { vabsq_f32(self.0) })
        }
        #[inline(always)]
        pub fn sqrt(self) -> Self {
            Self(unsafe { vsqrtq_f32(self.0) })
        }
        #[inline(always)]
        pub fn lt(self, o: Self) -> Mask4 {
            Mask4(unsafe { vcltq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn le(self, o: Self) -> Mask4 {
            Mask4(unsafe { vcleq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn gt(self, o: Self) -> Mask4 {
            Mask4(unsafe { vcgtq_f32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn ge(self, o: Self) -> Mask4 {
            Mask4(unsafe { vcgeq_f32(self.0, o.0) })
        }
    }
    impl Mask4 {
        #[inline(always)]
        pub fn select(self, a: F32x4, b: F32x4) -> F32x4 {
            F32x4(unsafe { vbslq_f32(self.0, a.0, b.0) })
        }
        #[inline(always)]
        pub fn and(self, o: Self) -> Self {
            Self(unsafe { vandq_u32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn or(self, o: Self) -> Self {
            Self(unsafe { vorrq_u32(self.0, o.0) })
        }
        #[inline(always)]
        pub fn bitmask(self) -> u32 {
            let mut o = [0u32; 4];
            unsafe { vst1q_u32(o.as_mut_ptr(), self.0) };
            (o[0] >> 31) | ((o[1] >> 31) << 1) | ((o[2] >> 31) << 2) | ((o[3] >> 31) << 3)
        }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod backend {
    #[derive(Clone, Copy)]
    pub struct F32x4([f32; 4]);
    #[derive(Clone, Copy)]
    pub struct Mask4([bool; 4]);
    macro_rules! lanes {
        ($a:expr, $b:expr, |$x:ident, $y:ident| $e:expr) => {{
            let (a, b) = ($a, $b);
            let mut o = [0.0f32; 4];
            for i in 0..4 {
                let ($x, $y) = (a[i], b[i]);
                o[i] = $e;
            }
            o
        }};
    }
    macro_rules! cmp {
        ($a:expr, $b:expr, |$x:ident, $y:ident| $e:expr) => {{
            let (a, b) = ($a, $b);
            let mut o = [false; 4];
            for i in 0..4 {
                let ($x, $y) = (a[i], b[i]);
                o[i] = $e;
            }
            Mask4(o)
        }};
    }
    impl F32x4 {
        pub fn splat(x: f32) -> Self {
            Self([x; 4])
        }
        pub fn from_array(a: [f32; 4]) -> Self {
            Self(a)
        }
        pub fn to_array(self) -> [f32; 4] {
            self.0
        }
        pub fn add(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| x + y))
        }
        pub fn sub(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| x - y))
        }
        pub fn mul(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| x * y))
        }
        pub fn div(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| x / y))
        }
        pub fn min(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| if x < y { x } else { y }))
        }
        pub fn max(self, o: Self) -> Self {
            Self(lanes!(self.0, o.0, |x, y| if x > y { x } else { y }))
        }
        pub fn abs(self) -> Self {
            Self(lanes!(self.0, self.0, |x, _y| x.abs()))
        }
        pub fn sqrt(self) -> Self {
            Self(lanes!(self.0, self.0, |x, _y| x.sqrt()))
        }
        pub fn lt(self, o: Self) -> Mask4 {
            cmp!(self.0, o.0, |x, y| x < y)
        }
        pub fn le(self, o: Self) -> Mask4 {
            cmp!(self.0, o.0, |x, y| x <= y)
        }
        pub fn gt(self, o: Self) -> Mask4 {
            cmp!(self.0, o.0, |x, y| x > y)
        }
        pub fn ge(self, o: Self) -> Mask4 {
            cmp!(self.0, o.0, |x, y| x >= y)
        }
    }
    impl Mask4 {
        pub fn select(self, a: F32x4, b: F32x4) -> F32x4 {
            let mut o = [0.0f32; 4];
            for i in 0..4 {
                o[i] = if self.0[i] { a.0[i] } else { b.0[i] };
            }
            F32x4(o)
        }
        pub fn and(self, o: Self) -> Self {
            Self([self.0[0] & o.0[0], self.0[1] & o.0[1], self.0[2] & o.0[2], self.0[3] & o.0[3]])
        }
        pub fn or(self, o: Self) -> Self {
            Self([self.0[0] | o.0[0], self.0[1] | o.0[1], self.0[2] | o.0[2], self.0[3] | o.0[3]])
        }
        pub fn bitmask(self) -> u32 {
            (self.0[0] as u32) | ((self.0[1] as u32) << 1) | ((self.0[2] as u32) << 2) | ((self.0[3] as u32) << 3)
        }
    }
}

pub use backend::{F32x4, Mask4};

impl F32x4 {
    pub const LANES: usize = 4;

    #[inline(always)]
    pub fn zero() -> Self {
        Self::splat(0.0)
    }

    /// Loads the first four values of `s` (panics when shorter).
    #[inline(always)]
    pub fn load(s: &[f32]) -> Self {
        Self::from_array([s[0], s[1], s[2], s[3]])
    }

    /// Stores into the first four values of `s` (panics when shorter).
    #[inline(always)]
    pub fn store(self, s: &mut [f32]) {
        s[..4].copy_from_slice(&self.to_array());
    }

    /// Sum of the lanes, added as `(l0 + l1) + (l2 + l3)` on every backend.
    #[inline(always)]
    pub fn sum(self) -> f32 {
        let a = self.to_array();
        (a[0] + a[1]) + (a[2] + a[3])
    }

    /// Largest lane.
    #[inline(always)]
    pub fn max_lane(self) -> f32 {
        let a = self.to_array();
        a[0].max(a[1]).max(a[2].max(a[3]))
    }

    /// Smallest lane.
    #[inline(always)]
    pub fn min_lane(self) -> f32 {
        let a = self.to_array();
        a[0].min(a[1]).min(a[2].min(a[3]))
    }

    /// Lane `i` (0 to 3).
    #[inline(always)]
    pub fn lane(self, i: usize) -> f32 {
        self.to_array()[i]
    }

    /// `self * m + a` as a multiply then an add (never fused).
    #[inline(always)]
    pub fn mul_add(self, m: Self, a: Self) -> Self {
        self * m + a
    }

    /// Lanes where the mask is set keep `self`, the others become zero.
    #[inline(always)]
    pub fn masked(self, m: Mask4) -> Self {
        m.select(self, Self::zero())
    }
}

impl Mask4 {
    #[inline(always)]
    pub fn any(self) -> bool {
        self.bitmask() != 0
    }
    #[inline(always)]
    pub fn all(self) -> bool {
        self.bitmask() == 0b1111
    }
}

macro_rules! op {
    ($tr:ident, $f:ident, $atr:ident, $af:ident) => {
        impl $tr for F32x4 {
            type Output = F32x4;
            #[inline(always)]
            fn $f(self, o: F32x4) -> F32x4 {
                F32x4::$f(self, o)
            }
        }
        impl $tr<f32> for F32x4 {
            type Output = F32x4;
            #[inline(always)]
            fn $f(self, o: f32) -> F32x4 {
                F32x4::$f(self, F32x4::splat(o))
            }
        }
        impl $atr for F32x4 {
            #[inline(always)]
            fn $af(&mut self, o: F32x4) {
                *self = F32x4::$f(*self, o);
            }
        }
    };
}
op!(Add, add, AddAssign, add_assign);
op!(Sub, sub, SubAssign, sub_assign);
op!(Mul, mul, MulAssign, mul_assign);

impl Div for F32x4 {
    type Output = F32x4;
    #[inline(always)]
    fn div(self, o: F32x4) -> F32x4 {
        F32x4::div(self, o)
    }
}

impl Neg for F32x4 {
    type Output = F32x4;
    #[inline(always)]
    fn neg(self) -> F32x4 {
        F32x4::zero() - self
    }
}

impl Default for F32x4 {
    fn default() -> Self {
        Self::zero()
    }
}

impl core::fmt::Debug for F32x4 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "F32x4({:?})", self.to_array())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: [f32; 4]) -> F32x4 {
        F32x4::from_array(a)
    }

    #[test]
    fn arithmetic_matches_scalar() {
        let a = [1.5f32, -2.0, 0.25, 1e10];
        let b = [0.5f32, 4.0, -8.0, 3.0];
        let (x, y) = (v(a), v(b));
        for i in 0..4 {
            assert_eq!((x + y).lane(i), a[i] + b[i]);
            assert_eq!((x - y).lane(i), a[i] - b[i]);
            assert_eq!((x * y).lane(i), a[i] * b[i]);
            assert_eq!((x / y).lane(i), a[i] / b[i]);
            assert_eq!(x.min(y).lane(i), a[i].min(b[i]));
            assert_eq!(x.max(y).lane(i), a[i].max(b[i]));
            assert_eq!(x.abs().lane(i), a[i].abs());
            assert_eq!((-x).lane(i), -a[i]);
            assert_eq!((x * 2.0).lane(i), a[i] * 2.0);
        }
        assert_eq!(v([4.0, 9.0, 16.0, 0.0]).sqrt().to_array(), [2.0, 3.0, 4.0, 0.0]);
        assert_eq!(
            x.mul_add(y, y).to_array(),
            [a[0] * b[0] + b[0], a[1] * b[1] + b[1], a[2] * b[2] + b[2], a[3] * b[3] + b[3]]
        );
        let mut z = x;
        z += y;
        z *= y;
        z -= x;
        assert_eq!(z.lane(1), (a[1] + b[1]) * b[1] - a[1]);
    }

    #[test]
    fn compare_and_select() {
        let x = v([1.0, 2.0, 3.0, 4.0]);
        let y = v([4.0, 2.0, 1.0, 5.0]);
        assert_eq!(x.lt(y).bitmask(), 0b1001);
        assert_eq!(x.le(y).bitmask(), 0b1011);
        assert_eq!(x.gt(y).bitmask(), 0b0100);
        assert_eq!(x.ge(y).bitmask(), 0b0110);
        assert_eq!(x.lt(y).select(x, y).to_array(), [1.0, 2.0, 1.0, 4.0]);
        assert_eq!(x.lt(y).and(x.gt(F32x4::splat(1.5))).bitmask(), 0b1000);
        assert_eq!(x.lt(y).or(x.gt(y)).bitmask(), 0b1101);
        assert!(x.lt(y).any() && !x.lt(y).all());
        assert!(x.lt(F32x4::splat(9.0)).all());
        assert!(!x.gt(F32x4::splat(9.0)).any());
        assert_eq!(x.masked(x.gt(F32x4::splat(2.5))).to_array(), [0.0, 0.0, 3.0, 4.0]);
    }

    #[test]
    fn horizontal_and_memory() {
        let x = v([1.0, -5.0, 3.0, 0.5]);
        assert_eq!(x.sum(), (1.0 - 5.0) + (3.0 + 0.5));
        assert_eq!(x.max_lane(), 3.0);
        assert_eq!(x.min_lane(), -5.0);
        let src = [9.0, 8.0, 7.0, 6.0, 5.0];
        assert_eq!(F32x4::load(&src[1..]).to_array(), [8.0, 7.0, 6.0, 5.0]);
        let mut dst = [0.0f32; 6];
        x.store(&mut dst[2..]);
        assert_eq!(dst, [0.0, 0.0, 1.0, -5.0, 3.0, 0.5]);
        assert_eq!(F32x4::splat(2.0).to_array(), [2.0; 4]);
    }
}
