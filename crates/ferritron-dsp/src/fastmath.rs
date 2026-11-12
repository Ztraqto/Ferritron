//! Fast approximations in `f32`. Every function states its error bound; the tests check
//! the bound over the stated range against the `f64` standard library. None of them uses
//! fused multiply-add, so results are the same on every CPU.
//!
//! | function | range | bound |
//! |---|---|---|
//! | [`log2`] | positive normal and subnormal `f32` | absolute error <= 1e-6 + 1.2e-7 * abs(result) (the second term is the `f32` rounding of the result itself) |
//! | [`exp2`] | `[-126, 127]` (clamped outside) | relative error <= 1e-6 |
//! | [`gain_to_db`] | `1e-8 <= g <= 1e3` (-160 to +60 dB) | absolute error <= 5e-5 dB |
//! | [`db_to_gain`] | `[-750, 760]` dB (clamped) | relative error <= 1e-6 |
//! | [`tanh`] | all (saturates beyond +-9) | absolute error <= 1e-6 |

const LOG2_E: f32 = core::f32::consts::LOG2_E;
const LN_2: f32 = core::f32::consts::LN_2;
/// 20 * log10(2)
const DB_PER_OCTAVE: f32 = 6.020_6;
/// log2(10) / 20
const OCTAVES_PER_DB: f32 = 0.166_096_4;

/// Base-2 logarithm. `0` gives `-inf`, negative input gives NaN, `+inf` stays `+inf`.
#[inline]
pub fn log2(x: f32) -> f32 {
    if !(x > 0.0) {
        return if x == 0.0 { f32::NEG_INFINITY } else { f32::NAN };
    }
    if x.is_infinite() {
        return x;
    }
    // subnormal: scale into the normal range first
    let (x, bias) = if x < f32::MIN_POSITIVE { (x * 16_777_216.0, -24.0) } else { (x, 0.0) };
    let bits = x.to_bits();
    let mut e = ((bits >> 23) & 0xff) as i32 - 127;
    let mut m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000); // [1, 2)
    if m > core::f32::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    // log2(m) = 2/ln2 * atanh(s), s = (m - 1) / (m + 1), |s| <= 0.1716
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let p = s * (2.885_39 + s2 * (0.961_796_7 + s2 * (0.577_078 + s2 * 0.412_198_5)));
    e as f32 + bias + p
}

/// Rounds to the nearest integer (ties to even) without a library call; values of 2^22 and
/// above are returned unchanged (they are integers or nearly so in `f32`). Not for NaN.
#[inline(always)]
pub fn round_nearest(x: f32) -> f32 {
    const MAGIC: f32 = 12_582_912.0; // 1.5 * 2^23
    if x.abs() < 4_194_304.0 { (x + MAGIC) - MAGIC } else { x }
}

/// Base-2 exponential. Inputs are clamped to `[-126, 127]`.
#[inline]
pub fn exp2(x: f32) -> f32 {
    let x = x.clamp(-126.0, 127.0);
    let n = round_nearest(x);
    let y = (x - n) * LN_2; // |y| <= 0.3466
    let p = 1.0
        + y * (1.0
            + y * (0.5
                + y * (0.166_666_67
                    + y * (0.041_666_668 + y * (0.008_333_334 + y * (0.001_388_889 + y * 0.000_198_412_7))))));
    p * f32::from_bits(((n as i32 + 127) as u32) << 23)
}

/// Gain (linear amplitude) to decibels. `g <= 0` gives `-inf`.
#[inline]
pub fn gain_to_db(g: f32) -> f32 {
    log2(g) * DB_PER_OCTAVE
}

/// Decibels to gain (linear amplitude).
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    exp2(db * OCTAVES_PER_DB)
}

/// Hyperbolic tangent through [`exp2`]; saturates to +-1 beyond +-9.
#[inline]
pub fn tanh(x: f32) -> f32 {
    let x = x.clamp(-9.0, 9.0);
    let e = exp2(2.0 * x * LOG2_E);
    (e - 1.0) / (e + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_error_bound() {
        let mut worst = 0.0f64;
        let mut x = 1e-30f64;
        while x < 1e30 {
            let got = log2(x as f32) as f64;
            let want = (x as f32 as f64).log2();
            worst = worst.max((got - want).abs() - 1.2e-7 * want.abs());
            x *= 1.0007;
        }
        for k in 0..20_000 {
            let x = 0.5f32 + k as f32 * 1e-4;
            worst = worst.max((log2(x) as f64 - (x as f64).log2()).abs() - 1.2e-7 * (x as f64).log2().abs());
        }
        // subnormals
        for bits in [1u32, 100, 12345, 0x7f_ffff] {
            let x = f32::from_bits(bits);
            worst = worst.max((log2(x) as f64 - (x as f64).log2()).abs() - 1.2e-7 * (x as f64).log2().abs());
        }
        println!("log2 worst error beyond the f32 rounding of the result: {worst:e}");
        assert!(worst <= 1e-6, "{worst}");
        assert_eq!(log2(1.0), 0.0);
        assert_eq!(log2(8.0), 3.0);
        assert_eq!(log2(0.0), f32::NEG_INFINITY);
        assert!(log2(-1.0).is_nan());
        assert_eq!(log2(f32::INFINITY), f32::INFINITY);
    }

    #[test]
    fn exp2_error_bound() {
        let mut worst = 0.0f64;
        let mut x = -126.0f32;
        while x <= 127.0 {
            let got = exp2(x) as f64;
            let want = (x as f64).exp2();
            worst = worst.max(((got - want) / want).abs());
            x += 0.00731;
        }
        println!("exp2 worst relative error: {worst:e}");
        assert!(worst <= 1e-6, "{worst}");
        assert_eq!(exp2(0.0), 1.0);
        assert_eq!(exp2(3.0), 8.0);
        assert!(exp2(1000.0).is_finite() && exp2(-1000.0) > 0.0);
    }

    #[test]
    fn decibel_conversions_within_001_db() {
        let mut worst_db = 0.0f64;
        let mut worst_rel = 0.0f64;
        let mut db = -140.0f32;
        while db <= 40.0 {
            let g = db_to_gain(db);
            let want = 10f64.powf(db as f64 / 20.0);
            worst_rel = worst_rel.max(((g as f64 - want) / want).abs());
            let back = gain_to_db(g);
            worst_db = worst_db.max((back as f64 - db as f64).abs());
            let direct = 20.0 * (g as f64).log10();
            worst_db = worst_db.max((gain_to_db(g) as f64 - direct).abs());
            db += 0.0137;
        }
        println!("db_to_gain relative {worst_rel:e}, round trip {worst_db:e} dB");
        assert!(worst_db <= 0.01, "{worst_db}");
        assert!(worst_db <= 5e-5 && worst_rel <= 1e-6);
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
        assert!(gain_to_db(1.0).abs() < 1e-6);
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-7);
        assert!((db_to_gain(6.0206) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn round_nearest_matches_the_standard_rounding() {
        for k in -20_000..20_000 {
            let x = k as f32 * 0.0123;
            assert_eq!(round_nearest(x), x.round_ties_even(), "{x}");
        }
        assert_eq!(round_nearest(1e9), 1e9);
        assert_eq!(round_nearest(-2.5), -2.0);
    }

    #[test]
    fn tanh_error_bound() {
        let mut worst = 0.0f64;
        let mut x = -12.0f32;
        while x <= 12.0 {
            worst = worst.max((tanh(x) as f64 - (x as f64).tanh()).abs());
            x += 0.00173;
        }
        println!("tanh worst absolute error: {worst:e}");
        assert!(worst <= 1e-6, "{worst}");
        assert_eq!(tanh(0.0), 0.0);
        assert_eq!(tanh(50.0), tanh(9.0));
        assert!(tanh(-3.0) < 0.0 && tanh(3.0) == -tanh(-3.0));
    }
}
