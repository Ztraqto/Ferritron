//! Oversampling by 2, 4, 8 or 16 with cascaded linear-phase half-band FIR stages
//! (Kaiser-windowed, designed in `f64`, run in `f32`).
//!
//! Stage 0 (closest to the host rate) is the steep one (65 taps, passband to about 86 % of
//! Nyquist, stopband about 70 dB); later stages work on a signal that is already limited
//! and use 33 taps. The whole chain is symmetric, so up- followed by down-sampling delays
//! the signal by exactly [`Oversampler::latency`] host-rate samples, an integer. Report it
//! to the host (and add it to your own look-ahead). Allocation happens in `new` only;
//! `upsample` and `downsample` work in chunks of at most `max_block` input samples.

use core::f64::consts::PI;

/// Half-length parameter per stage: a stage has `4 * K + 1` taps of which about half are zero.
const K0: usize = 16;
const K_LATER: usize = 8;
const BETA: f64 = 7.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..60 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

/// Odd taps `h[1], h[3], ... h[2K-1]` of the half-band filter (`h[0]` is exactly 0.5),
/// normalised so that the DC gain is exactly 1.
fn halfband_odd_taps(k: usize) -> Vec<f64> {
    let half = 2 * k;
    let w = |i: f64| bessel_i0(BETA * (1.0 - (i / half as f64).powi(2)).max(0.0).sqrt()) / bessel_i0(BETA);
    let mut h: Vec<f64> = (0..k)
        .map(|j| {
            let i = (2 * j + 1) as f64;
            (PI * i / 2.0).sin() / (PI * i) * w(i)
        })
        .collect();
    // sum over all taps = 0.5 + 2 * sum(h_odd) = 1
    let s: f64 = h.iter().sum();
    for v in &mut h {
        *v *= 0.25 / s;
    }
    h
}

#[derive(Clone, Debug)]
struct Stage {
    k: usize,
    /// `h[2j + 1]` for j in 0..k.
    odd: Vec<f32>,
    /// Upsampler history (input rate), power-of-two ring.
    up: Vec<f32>,
    up_pos: usize,
    /// Downsampler history (output rate of the up path = this stage's high rate).
    down: Vec<f32>,
    down_pos: usize,
}

impl Stage {
    fn new(k: usize) -> Self {
        let odd: Vec<f32> = halfband_odd_taps(k).iter().map(|&v| v as f32).collect();
        let up_len = (2 * k + 1).next_power_of_two();
        let down_len = (4 * k + 1).next_power_of_two();
        Stage { k, odd, up: vec![0.0; up_len], up_pos: 0, down: vec![0.0; down_len], down_pos: 0 }
    }

    fn reset(&mut self) {
        self.up.fill(0.0);
        self.down.fill(0.0);
    }

    /// One input sample in, two out. `y[2n] = x[n-K]`, `y[2n+1] = sum_q g[q] x[n-q]`, `g[q] = 2 h[2q-2K+1]`.
    #[inline]
    fn up(&mut self, x: f32) -> (f32, f32) {
        let mask = self.up.len() - 1;
        self.up_pos = (self.up_pos + 1) & mask;
        self.up[self.up_pos] = x;
        let k = self.k;
        let at = |d: usize| self.up[self.up_pos.wrapping_sub(d) & mask];
        let even = at(k);
        // g[q] pairs q and 2K-1-q (symmetry): odd[|2q-2K+1| / 2 ...]; use h[2j+1] for j = |q - K + 0.5| - 0.5
        let mut acc = 0.0f32;
        for j in 0..k {
            // q = K + j and q = K - 1 - j share the tap h[2j+1]
            acc += self.odd[j] * (at(k + j) + at(k - 1 - j));
        }
        (even, 2.0 * acc)
    }

    /// Two high-rate samples in (`a` at the even phase, `b` after it), one low-rate sample out.
    #[inline]
    fn down(&mut self, a: f32, b: f32) -> f32 {
        let mask = self.down.len() - 1;
        self.down_pos = (self.down_pos + 1) & mask;
        self.down[self.down_pos] = a;
        let k = self.k;
        let at = |d: usize| self.down[self.down_pos.wrapping_sub(d) & mask];
        // f[t] = 0.5 s[t-2K] + sum_j h[2j+1] (s[t-2K-(2j+1)] + s[t-2K+(2j+1)])
        let mut acc = 0.5 * at(2 * k);
        for j in 0..k {
            acc += self.odd[j] * (at(2 * k + 2 * j + 1) + at(2 * k - 2 * j - 1));
        }
        self.down_pos = (self.down_pos + 1) & mask;
        self.down[self.down_pos] = b;
        acc
    }
}

#[derive(Clone, Debug)]
pub struct Oversampler {
    factor: usize,
    max_block: usize,
    stages: Vec<Stage>,
    a: Vec<f32>,
    b: Vec<f32>,
}

impl Oversampler {
    /// `factor` is 1, 2, 4, 8 or 16 (1 passes the signal through). `max_block` is the longest
    /// chunk of input samples handled in one go (longer inputs are processed in pieces).
    pub fn new(factor: usize, max_block: usize) -> Self {
        assert!(matches!(factor, 1 | 2 | 4 | 8 | 16), "factor must be 1, 2, 4, 8 or 16");
        let max_block = max_block.max(1);
        let n = factor.trailing_zeros() as usize;
        let stages = (0..n).map(|i| Stage::new(if i == 0 { K0 } else { K_LATER })).collect();
        Oversampler { factor, max_block, stages, a: vec![0.0; max_block * factor], b: vec![0.0; max_block * factor] }
    }

    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Delay of `upsample` followed by `downsample`, in samples at the host rate.
    pub fn latency(&self) -> usize {
        self.stages.iter().enumerate().map(|(i, s)| 4 * s.k / (2usize << i)).sum()
    }

    pub fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
    }

    /// `out.len()` must be `input.len() * factor`.
    pub fn upsample(&mut self, input: &[f32], out: &mut [f32]) {
        assert_eq!(out.len(), input.len() * self.factor, "output must be factor times longer");
        if self.factor == 1 {
            out.copy_from_slice(input);
            return;
        }
        let f = self.factor;
        for (inp, o) in input.chunks(self.max_block).zip(out.chunks_mut(self.max_block * f)) {
            let mut len = inp.len();
            self.a[..len].copy_from_slice(inp);
            for stage in self.stages.iter_mut() {
                for i in 0..len {
                    let (e, od) = stage.up(self.a[i]);
                    self.b[2 * i] = e;
                    self.b[2 * i + 1] = od;
                }
                len *= 2;
                core::mem::swap(&mut self.a, &mut self.b);
            }
            o.copy_from_slice(&self.a[..len]);
        }
    }

    /// `input.len()` must be `out.len() * factor`.
    pub fn downsample(&mut self, input: &[f32], out: &mut [f32]) {
        assert_eq!(input.len(), out.len() * self.factor, "input must be factor times longer");
        if self.factor == 1 {
            out.copy_from_slice(input);
            return;
        }
        let f = self.factor;
        for (inp, o) in input.chunks(self.max_block * f).zip(out.chunks_mut(self.max_block)) {
            let mut len = inp.len();
            self.a[..len].copy_from_slice(inp);
            for stage in self.stages.iter_mut().rev() {
                for i in 0..len / 2 {
                    self.b[i] = stage.down(self.a[2 * i], self.a[2 * i + 1]);
                }
                len /= 2;
                core::mem::swap(&mut self.a, &mut self.b);
            }
            o.copy_from_slice(&self.a[..len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::Fft;

    fn tone(bin: usize, n: usize) -> Vec<f32> {
        (0..n).map(|t| (2.0 * PI * bin as f64 * t as f64 / n as f64).sin() as f32 * 0.5).collect()
    }

    /// Magnitude spectrum in dB relative to a full-scale-0.5 tone bin.
    fn spectrum_db(x: &[f32]) -> Vec<f32> {
        let n = x.len();
        let mut f = Fft::new(n);
        let (mut re, mut im) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
        f.forward(x, &mut re, &mut im);
        (0..=n / 2).map(|k| 20.0 * ((re[k] * re[k] + im[k] * im[k]).sqrt() / (n as f32 / 4.0)).max(1e-12).log10()).collect()
    }

    #[test]
    fn taps_have_unit_dc_gain_and_the_stated_latency() {
        for k in [K0, K_LATER] {
            let h = halfband_odd_taps(k);
            assert!((0.5 + 2.0 * h.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            assert!((h[0] - 1.0 / PI).abs() < 0.02 && h.windows(2).all(|w| w[0].abs() > w[1].abs()));
        }
        let lat: Vec<usize> = [1usize, 2, 4, 8, 16].iter().map(|&f| Oversampler::new(f, 64).latency()).collect();
        assert_eq!(lat, [0, 32, 40, 44, 46]);
    }

    #[test]
    fn up_then_down_is_a_delay_in_the_passband() {
        for factor in [2usize, 4, 8, 16] {
            let mut os = Oversampler::new(factor, 100);
            let n = 4096;
            let x = tone(300, n); // 0.073 of the host rate
            let mut hi = vec![0.0f32; n * factor];
            let mut y = vec![0.0f32; n];
            // feed in uneven pieces: the chunking must not change anything
            let mut at = 0;
            for piece in [1usize, 99, 100, 1000, 2896] {
                os.upsample(&x[at..at + piece], &mut hi[at * factor..(at + piece) * factor]);
                os.downsample(&hi[at * factor..(at + piece) * factor], &mut y[at..at + piece]);
                at += piece;
            }
            assert_eq!(at, n);
            let l = os.latency();
            let worst = (n / 2..n).map(|t| (y[t] - x[t - l]).abs()).fold(0.0f32, f32::max);
            assert!(worst < 2e-3, "factor {factor}: {worst}");
        }
    }

    #[test]
    fn images_are_suppressed_when_upsampling() {
        // A tone at 0.2 of the host rate has its first image at 0.8: it must be far below the tone.
        let n = 8192;
        let tone_bin = 1638; // whole periods per block, so the block can be analysed directly
        for factor in [2usize, 4, 8] {
            let mut os = Oversampler::new(factor, n);
            let x = tone(tone_bin, n);
            let mut hi = vec![0.0f32; n * factor];
            os.upsample(&x, &mut hi); // start-up
            os.upsample(&x, &mut hi); // the signal is periodic: this block is steady state
            let spec = spectrum_db(&hi);
            let tone_db = spec[tone_bin - 1..=tone_bin + 1].iter().cloned().fold(f32::MIN, f32::max);
            let loudest_other = spec
                .iter()
                .enumerate()
                .filter(|(k, _)| (*k as i64 - tone_bin as i64).abs() > 4)
                .map(|(_, v)| *v)
                .fold(f32::MIN, f32::max);
            println!("factor {factor}: tone {tone_db:.1} dB, strongest other bin {loudest_other:.1} dB");
            assert!((tone_db - 0.0).abs() < 0.1, "unit gain: {tone_db}");
            assert!(tone_db - loudest_other > 55.0, "factor {factor}: {tone_db} vs {loudest_other}");
        }
    }

    #[test]
    fn aliasing_is_suppressed_when_downsampling() {
        // At 2x, a tone at 0.4 of the high rate (0.8 of the host rate) would alias to 0.2.
        let n = 8192;
        let mut os = Oversampler::new(2, n);
        let x = tone(3277, n); // 0.4 * 8192 = 3276.8 -> bin 3277 of the high rate
        let mut y = vec![0.0f32; n / 2];
        // run twice so that the filter has settled
        os.downsample(&x, &mut y);
        os.downsample(&x, &mut y);
        let rms_in = (x.iter().map(|v| (v * v) as f64).sum::<f64>() / x.len() as f64).sqrt();
        let rms_out = (y[512..].iter().map(|v| (v * v) as f64).sum::<f64>() / (y.len() - 512) as f64).sqrt();
        let db = 20.0 * (rms_out / rms_in).log10();
        println!("alias attenuation {db:.1} dB");
        assert!(db < -60.0, "{db}");
        // a tone well inside the band passes at unit gain
        let x = tone(400, n);
        os.downsample(&x, &mut y);
        os.downsample(&x, &mut y);
        let rms_out = (y[512..].iter().map(|v| (v * v) as f64).sum::<f64>() / (y.len() - 512) as f64).sqrt();
        assert!((rms_out / rms_in - 1.0).abs() < 0.01, "{}", rms_out / rms_in);
    }

    #[test]
    fn factor_one_is_a_copy_and_reset_clears() {
        let mut os = Oversampler::new(1, 8);
        let x = [1.0f32, 2.0, 3.0];
        let mut y = [0.0f32; 3];
        os.upsample(&x, &mut y);
        assert_eq!(x, y);
        assert_eq!(os.latency(), 0);
        let mut os = Oversampler::new(4, 16);
        let mut hi = vec![0.0f32; 64];
        os.upsample(&[1.0; 16], &mut hi);
        os.reset();
        os.upsample(&[0.0; 16], &mut hi);
        assert!(hi.iter().all(|&v| v == 0.0));
    }
}
