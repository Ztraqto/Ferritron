//! RBJ ("Audio EQ Cookbook") filter design in `f64`. Every function returns coefficients
//! normalised to `a0 = 1`. `f0` is clamped to `[1 Hz, 0.49 fs]`, `q` to at least `1e-3`.

use crate::biquad::Coefs;
use core::f64::consts::PI;

struct W {
    cos: f64,
    alpha: f64,
}

fn w(fs: f64, f0: f64, q: f64) -> W {
    let f0 = f0.clamp(1.0, 0.49 * fs);
    let w0 = 2.0 * PI * f0 / fs;
    W { cos: w0.cos(), alpha: w0.sin() / (2.0 * q.max(1e-3)) }
}

fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coefs {
    Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

pub fn lowpass(fs: f64, f0: f64, q: f64) -> Coefs {
    let W { cos, alpha } = w(fs, f0, q);
    norm((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
}

pub fn highpass(fs: f64, f0: f64, q: f64) -> Coefs {
    let W { cos, alpha } = w(fs, f0, q);
    norm((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
}

pub fn notch(fs: f64, f0: f64, q: f64) -> Coefs {
    let W { cos, alpha } = w(fs, f0, q);
    norm(1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
}

/// Band-pass with 0 dB gain at `f0` (constant peak gain).
pub fn bandpass(fs: f64, f0: f64, q: f64) -> Coefs {
    let W { cos, alpha } = w(fs, f0, q);
    norm(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
}

pub fn allpass(fs: f64, f0: f64, q: f64) -> Coefs {
    let W { cos, alpha } = w(fs, f0, q);
    norm(1.0 - alpha, -2.0 * cos, 1.0 + alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
}

/// Peaking bell: `gain_db` at `f0`, 0 dB far away.
pub fn peaking(fs: f64, f0: f64, q: f64, gain_db: f64) -> Coefs {
    let a = 10f64.powf(gain_db / 40.0);
    let W { cos, alpha } = w(fs, f0, q);
    norm(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
}

/// Low shelf: `gain_db` below `f0` (`q` of 0.7071 is the usual steepest monotonic slope).
pub fn low_shelf(fs: f64, f0: f64, q: f64, gain_db: f64) -> Coefs {
    let a = 10f64.powf(gain_db / 40.0);
    let W { cos, alpha } = w(fs, f0, q);
    let s = 2.0 * a.sqrt() * alpha;
    norm(
        a * ((a + 1.0) - (a - 1.0) * cos + s),
        2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
        a * ((a + 1.0) - (a - 1.0) * cos - s),
        (a + 1.0) + (a - 1.0) * cos + s,
        -2.0 * ((a - 1.0) + (a + 1.0) * cos),
        (a + 1.0) + (a - 1.0) * cos - s,
    )
}

/// High shelf: `gain_db` above `f0`.
pub fn high_shelf(fs: f64, f0: f64, q: f64, gain_db: f64) -> Coefs {
    let a = 10f64.powf(gain_db / 40.0);
    let W { cos, alpha } = w(fs, f0, q);
    let s = 2.0 * a.sqrt() * alpha;
    norm(
        a * ((a + 1.0) + (a - 1.0) * cos + s),
        -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
        a * ((a + 1.0) + (a - 1.0) * cos - s),
        (a + 1.0) - (a - 1.0) * cos + s,
        2.0 * ((a - 1.0) - (a + 1.0) * cos),
        (a + 1.0) - (a - 1.0) * cos - s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    fn db_at(c: &Coefs, f: f64) -> f64 {
        c.magnitude_db(FS, f)
    }

    #[test]
    fn peaking_hits_its_gain_and_is_flat_far_away() {
        for gain in [-12.0, -3.0, 6.0, 18.0] {
            let c = peaking(FS, 1000.0, 1.2, gain);
            assert!((db_at(&c, 1000.0) - gain).abs() < 1e-9, "{gain}");
            assert!(db_at(&c, 20.0).abs() < 0.2);
            assert!(c.is_stable());
        }
    }

    #[test]
    fn low_pass_and_high_pass() {
        let lp = lowpass(FS, 2000.0, core::f64::consts::FRAC_1_SQRT_2);
        assert!(db_at(&lp, 1.0).abs() < 1e-3);
        assert!((db_at(&lp, 2000.0) + 3.0103).abs() < 1e-3);
        assert!(db_at(&lp, 20_000.0) < -35.0);
        let hp = highpass(FS, 2000.0, core::f64::consts::FRAC_1_SQRT_2);
        assert!((db_at(&hp, 2000.0) + 3.0103).abs() < 1e-3);
        assert!(db_at(&hp, 23_999.0).abs() < 0.1);
        assert!(db_at(&hp, 50.0) < -45.0);
        assert!(lp.is_stable() && hp.is_stable());
    }

    #[test]
    fn notch_bandpass_allpass() {
        let n = notch(FS, 1000.0, 4.0);
        assert!(db_at(&n, 1000.0) < -100.0);
        assert!(db_at(&n, 100.0).abs() < 0.1);
        let bp = bandpass(FS, 1000.0, 4.0);
        assert!(db_at(&bp, 1000.0).abs() < 1e-9);
        assert!(db_at(&bp, 100.0) < -20.0);
        let ap = allpass(FS, 1000.0, 1.0);
        for f in [50.0, 1000.0, 9000.0] {
            assert!(db_at(&ap, f).abs() < 1e-9);
        }
    }

    #[test]
    fn shelves() {
        let ls = low_shelf(FS, 200.0, std::f64::consts::FRAC_1_SQRT_2, 9.0);
        assert!((db_at(&ls, 1.0) - 9.0).abs() < 0.05);
        assert!((db_at(&ls, 200.0) - 4.5).abs() < 0.05, "midpoint");
        assert!(db_at(&ls, 20_000.0).abs() < 0.1);
        let hs = high_shelf(FS, 5000.0, std::f64::consts::FRAC_1_SQRT_2, -6.0);
        assert!((db_at(&hs, 23_990.0) + 6.0).abs() < 0.2);
        assert!(db_at(&hs, 50.0).abs() < 0.05);
        assert!(ls.is_stable() && hs.is_stable());
    }

    #[test]
    fn extreme_arguments_stay_finite_and_stable() {
        for f0 in [0.0, 1.0, 24_000.0, 1e9] {
            for q in [0.0, 0.01, 50.0] {
                let c = peaking(FS, f0, q, 12.0);
                assert!(c.b0.is_finite() && c.a1.is_finite() && c.a2.is_finite());
                assert!(c.is_stable(), "f0={f0} q={q}");
            }
        }
    }
}
