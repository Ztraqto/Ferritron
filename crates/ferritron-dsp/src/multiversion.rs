//! `multiversion!`: one function body, compiled several times with different
//! `#[target_feature]` sets, plus a run-time choice of the best copy.
//!
//! ```
//! use ferritron_dsp::multiversion;
//!
//! multiversion! {
//!     /// Sum of squares; the copies differ only in how LLVM may vectorise the loop.
//!     pub fn sum_sq(data: &[f32]) -> f32 {
//!         let mut s = 0.0f32;
//!         for &x in data { s += x * x; }
//!         s
//!     }
//! }
//!
//! // In `activate`: look the ISA up once and keep the function pointer.
//! let kernel: sum_sq::Kernel = sum_sq::best();
//! // In `process`: just call it. No ISA test, no feature detection.
//! assert_eq!(kernel(&[3.0, 4.0]), 25.0);
//! ```
//!
//! The macro creates a module named like the function with
//!
//! * `type Kernel = fn(args) -> ret`,
//! * `generic`, `sse41`, `avx2` (x86_64 only: the last two exist only there): safe functions,
//! * `select(isa) -> Kernel` and `best() -> Kernel` (the level from [`crate::isa`]).
//!
//! `select` never returns a copy the CPU cannot run, even when asked for a higher level
//! than the one detected. The copies compute the same expression, but a different
//! instruction selection may change float results in the last bit when the compiler
//! contracts operations; with `avx2` the macro enables `fma` too, and Rust does not fuse
//! `a * b + c` on its own, so for plain Rust arithmetic the results are identical.

#[macro_export]
macro_rules! multiversion {
    (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident($($arg:ident : $ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        #[allow(non_snake_case, dead_code, unused_unsafe)]
        $vis mod $name {
            #[allow(unused_imports)]
            use super::*;

            /// The function pointer type of every copy.
            pub type Kernel = fn($($ty),*) $(-> $ret)?;

            $(#[$meta])*
            pub fn generic($($arg: $ty),*) $(-> $ret)? $body

            #[cfg(target_arch = "x86_64")]
            #[target_feature(enable = "sse4.1")]
            unsafe fn sse41_impl($($arg: $ty),*) $(-> $ret)? $body
            #[cfg(target_arch = "x86_64")]
            #[target_feature(enable = "avx2,fma")]
            unsafe fn avx2_impl($($arg: $ty),*) $(-> $ret)? $body

            /// Only call when the CPU has SSE4.1 (use [`select`]).
            #[cfg(target_arch = "x86_64")]
            pub fn sse41($($arg: $ty),*) $(-> $ret)? {
                // SAFETY: `select` hands this out only when SSE4.1 was detected; calling it
                // directly on an older CPU is the caller's mistake, checked in debug builds.
                debug_assert!($crate::isa::detect().supports($crate::Isa::Sse41));
                unsafe { sse41_impl($($arg),*) }
            }
            /// Only call when the CPU has AVX2 and FMA (use [`select`]).
            #[cfg(target_arch = "x86_64")]
            pub fn avx2($($arg: $ty),*) $(-> $ret)? {
                // SAFETY: see `sse41`.
                debug_assert!($crate::isa::detect().supports($crate::Isa::Avx2));
                unsafe { avx2_impl($($arg),*) }
            }

            /// The best copy for `isa`, limited to what this CPU can run.
            pub fn select(isa: $crate::Isa) -> Kernel {
                let isa = if $crate::isa::detect().supports(isa) { isa } else { $crate::isa::detect() };
                #[cfg(target_arch = "x86_64")]
                {
                    if isa.supports($crate::Isa::Avx2) {
                        return avx2;
                    }
                    if isa.supports($crate::Isa::Sse41) {
                        return sse41;
                    }
                }
                let _ = isa;
                generic
            }

            /// The copy for the level in use ([`ferritron_dsp::isa`], which honours `FERRITRON_ISA`).
            pub fn best() -> Kernel {
                select($crate::isa::isa())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::Isa;

    multiversion! {
        pub fn dot(a: &[f32], b: &[f32]) -> f32 {
            let mut s = 0.0f32;
            for i in 0..a.len().min(b.len()) {
                s += a[i] * b[i];
            }
            s
        }
    }

    multiversion! {
        fn scale(buf: &mut [f32], g: f32) {
            for x in buf.iter_mut() {
                *x *= g;
            }
        }
    }

    #[test]
    fn every_level_gives_the_same_answer() {
        let a: Vec<f32> = (0..1000).map(|i| (i % 13) as f32 * 0.25).collect();
        let b: Vec<f32> = (0..1000).map(|i| (i % 7) as f32 - 3.0).collect();
        let want = dot::generic(&a, &b);
        for isa in [Isa::Scalar, Isa::Sse2, Isa::Sse41, Isa::Avx2, Isa::Neon] {
            // sums of small multiples of 0.25 are exact in f32, so equality is expected
            assert_eq!((dot::select(isa))(&a, &b), want, "{isa:?}");
        }
        assert_eq!(dot::best()(&a, &b), want);
    }

    #[test]
    fn works_without_a_return_value_and_picks_the_detected_level() {
        let mut v = vec![1.0f32; 9];
        scale::best()(&mut v, 2.5);
        assert!(v.iter().all(|&x| x == 2.5));
        // asking for more than the CPU has never hands out an unusable copy
        let k = scale::select(Isa::Avx2);
        k(&mut v, 2.0);
        assert!(v.iter().all(|&x| x == 5.0));
    }

    /// The intended use: choose once, keep the pointer, call it from the audio callback.
    struct Gain {
        kernel: scale::Kernel,
    }
    impl Gain {
        fn activate() -> Self {
            Gain { kernel: scale::best() }
        }
        fn process(&self, block: &mut [f32]) {
            (self.kernel)(block, 0.5);
        }
    }

    #[test]
    fn plugin_style_usage() {
        let g = Gain::activate();
        let mut b = [2.0f32; 5];
        g.process(&mut b);
        assert_eq!(b, [1.0; 5]);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_copy_runs_on_this_cpu_when_available() {
        if crate::isa::detect().supports(Isa::Avx2) {
            let mut v = vec![3.0f32; 33];
            scale::avx2(&mut v, 2.0);
            assert!(v.iter().all(|&x| x == 6.0));
            assert_eq!(dot::select(Isa::Avx2) as usize, (dot::avx2 as dot::Kernel) as usize);
        } else {
            eprintln!("AVX2 not available: not verified here");
        }
    }
}
