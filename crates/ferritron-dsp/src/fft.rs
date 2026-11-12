//! Real FFT of power-of-two length, written here (no dependencies), and a uniformly
//! partitioned convolver built on it. Plans are created in `new` (allocation, twiddle
//! computation in `f64`); `forward` / `inverse` / `process` allocate nothing.
//!
//! Spectrum layout: `n / 2 + 1` bins in separate `re` / `im` slices (`im[0]` and `im[n/2]`
//! are zero). `forward` is unscaled; `inverse` divides by `n`, so `inverse(forward(x)) == x`.

use core::f64::consts::PI;

/// Real FFT plan. Uses an `n / 2` point complex FFT plus a split step.
#[derive(Clone, Debug)]
pub struct Fft {
    n: usize,
    /// Complex FFT size, `n / 2`.
    m: usize,
    rev: Vec<u32>,
    /// `exp(-2 pi i k / m)` for `k < m / 2`.
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
    /// `exp(-2 pi i k / n)` for `k <= m`.
    sp_re: Vec<f32>,
    sp_im: Vec<f32>,
    z_re: Vec<f32>,
    z_im: Vec<f32>,
}

impl Fft {
    /// `n` must be a power of two, at least 4.
    pub fn new(n: usize) -> Self {
        assert!(n >= 4 && n.is_power_of_two(), "Fft length must be a power of two >= 4");
        let m = n / 2;
        let bits = m.trailing_zeros();
        let rev = (0..m as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        let tw: Vec<(f32, f32)> =
            (0..m / 2).map(|k| { let a = -2.0 * PI * k as f64 / m as f64; (a.cos() as f32, a.sin() as f32) }).collect();
        let sp: Vec<(f32, f32)> =
            (0..=m).map(|k| { let a = -2.0 * PI * k as f64 / n as f64; (a.cos() as f32, a.sin() as f32) }).collect();
        Fft {
            n,
            m,
            rev,
            tw_re: tw.iter().map(|t| t.0).collect(),
            tw_im: tw.iter().map(|t| t.1).collect(),
            sp_re: sp.iter().map(|t| t.0).collect(),
            sp_im: sp.iter().map(|t| t.1).collect(),
            z_re: vec![0.0; m],
            z_im: vec![0.0; m],
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Number of spectrum bins, `n / 2 + 1`.
    pub fn bins(&self) -> usize {
        self.m + 1
    }

    /// In-place complex FFT of the scratch (`inverse` = conjugate twiddles, unscaled).
    fn cfft(&mut self, inverse: bool) {
        let m = self.m;
        let (re, im) = (&mut self.z_re, &mut self.z_im);
        for i in 0..m {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let sign = if inverse { -1.0f32 } else { 1.0 };
        let mut len = 2;
        while len <= m {
            let half = len / 2;
            let step = m / len;
            let mut start = 0;
            while start < m {
                for k in 0..half {
                    let (wr, wi) = (self.tw_re[k * step], sign * self.tw_im[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
                start += len;
            }
            len *= 2;
        }
    }

    /// Spectrum of `input` (`n` samples) into `re` / `im` (`n / 2 + 1` bins each).
    pub fn forward(&mut self, input: &[f32], re: &mut [f32], im: &mut [f32]) {
        let (n, m) = (self.n, self.m);
        assert!(input.len() >= n && re.len() > m && im.len() > m);
        for k in 0..m {
            self.z_re[k] = input[2 * k];
            self.z_im[k] = input[2 * k + 1];
        }
        self.cfft(false);
        for k in 0..=m {
            let (zr, zi) = (self.z_re[k % m], self.z_im[k % m]);
            let (cr, ci) = (self.z_re[(m - k) % m], -self.z_im[(m - k) % m]); // conj(Z[m-k])
            let (er, ei) = (0.5 * (zr + cr), 0.5 * (zi + ci));
            // odd part: (Z[k] - conj(Z[m-k])) / (2i)
            let (dr, di) = (zr - cr, zi - ci);
            let (or_, oi) = (0.5 * di, -0.5 * dr);
            let (wr, wi) = (self.sp_re[k], self.sp_im[k]);
            re[k] = er + or_ * wr - oi * wi;
            im[k] = ei + or_ * wi + oi * wr;
        }
        im[0] = 0.0;
        im[m] = 0.0;
    }

    /// Time signal (`n` samples) from a spectrum of `n / 2 + 1` bins; scaled so that
    /// `inverse(forward(x)) == x`. Imaginary parts of bins 0 and `n / 2` are ignored.
    pub fn inverse(&mut self, re: &[f32], im: &[f32], out: &mut [f32]) {
        let (n, m) = (self.n, self.m);
        assert!(out.len() >= n && re.len() > m && im.len() > m);
        for k in 0..m {
            let (xr, xi) = (re[k], if k == 0 { 0.0 } else { im[k] });
            let (yr, yi) = (re[m - k], if k == 0 { 0.0 } else { -im[m - k] }); // conj(X[m-k])
            let (er, ei) = (0.5 * (xr + yr), 0.5 * (xi + yi));
            // odd part: (X[k] - conj(X[m-k])) / (2 W^k), W^k = sp[k]; 1 / W^k = conj(W^k)
            let (dr, di) = (0.5 * (xr - yr), 0.5 * (xi - yi));
            let (wr, wi) = (self.sp_re[k], -self.sp_im[k]);
            let (or_, oi) = (dr * wr - di * wi, dr * wi + di * wr);
            // Z[k] = E + i O
            self.z_re[k] = er - oi;
            self.z_im[k] = ei + or_;
        }
        self.cfft(true);
        let s = 1.0 / m as f32;
        for k in 0..m {
            out[2 * k] = self.z_re[k] * s;
            out[2 * k + 1] = self.z_im[k] * s;
        }
    }
}

/// Uniformly partitioned convolution (overlap-save with a frequency-domain delay line).
///
/// The impulse response is cut into blocks of `block` samples. Every call to
/// [`PartitionedConvolver::process`] takes exactly `block` input samples and returns the
/// `block` output samples of the full convolution, aligned with the input (no added delay
/// beyond the block you hand in). Cost per block: two FFTs of size `2 * block` plus one
/// complex multiply-add per partition and bin.
#[derive(Clone, Debug)]
pub struct PartitionedConvolver {
    block: usize,
    fft: Fft,
    bins: usize,
    parts: usize,
    h_re: Vec<f32>,
    h_im: Vec<f32>,
    /// Ring of input spectra, `parts` of them, newest at `pos`.
    x_re: Vec<f32>,
    x_im: Vec<f32>,
    pos: usize,
    window: Vec<f32>,
    acc_re: Vec<f32>,
    acc_im: Vec<f32>,
    time: Vec<f32>,
}

impl PartitionedConvolver {
    /// `block` is a power of two (at least 2); `ir` must not be empty. Allocates everything.
    pub fn new(block: usize, ir: &[f32]) -> Self {
        assert!(block >= 2 && block.is_power_of_two(), "block must be a power of two >= 2");
        assert!(!ir.is_empty(), "empty impulse response");
        let mut fft = Fft::new(2 * block);
        let bins = fft.bins();
        let parts = ir.len().div_ceil(block);
        let mut h_re = vec![0.0; parts * bins];
        let mut h_im = vec![0.0; parts * bins];
        let mut buf = vec![0.0f32; 2 * block];
        for p in 0..parts {
            buf.fill(0.0);
            let seg = &ir[p * block..ir.len().min((p + 1) * block)];
            buf[..seg.len()].copy_from_slice(seg);
            fft.forward(&buf, &mut h_re[p * bins..(p + 1) * bins], &mut h_im[p * bins..(p + 1) * bins]);
        }
        PartitionedConvolver {
            block,
            fft,
            bins,
            parts,
            h_re,
            h_im,
            x_re: vec![0.0; parts * bins],
            x_im: vec![0.0; parts * bins],
            pos: 0,
            window: vec![0.0; 2 * block],
            acc_re: vec![0.0; bins],
            acc_im: vec![0.0; bins],
            time: vec![0.0; 2 * block],
        }
    }

    pub fn block_size(&self) -> usize {
        self.block
    }

    pub fn partitions(&self) -> usize {
        self.parts
    }

    /// Forgets the input history.
    pub fn reset(&mut self) {
        self.x_re.fill(0.0);
        self.x_im.fill(0.0);
        self.window.fill(0.0);
    }

    /// Convolves one block. Both slices must be exactly `block_size()` long.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let b = self.block;
        assert!(input.len() == b && output.len() == b, "process() takes exactly one block");
        // window = [previous block | this block]
        self.window.copy_within(b.., 0);
        self.window[b..].copy_from_slice(input);
        self.pos = (self.pos + self.parts - 1) % self.parts;
        let bins = self.bins;
        let at = self.pos * bins;
        self.fft.forward(&self.window, &mut self.x_re[at..at + bins], &mut self.x_im[at..at + bins]);
        self.acc_re.fill(0.0);
        self.acc_im.fill(0.0);
        for p in 0..self.parts {
            let slot = (self.pos + p) % self.parts * bins;
            let (xr, xi) = (&self.x_re[slot..slot + bins], &self.x_im[slot..slot + bins]);
            let (hr, hi) = (&self.h_re[p * bins..(p + 1) * bins], &self.h_im[p * bins..(p + 1) * bins]);
            for k in 0..bins {
                self.acc_re[k] += xr[k] * hr[k] - xi[k] * hi[k];
                self.acc_im[k] += xr[k] * hi[k] + xi[k] * hr[k];
            }
        }
        self.fft.inverse(&self.acc_re, &self.acc_im, &mut self.time);
        output.copy_from_slice(&self.time[b..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn naive_dft(x: &[f32]) -> (Vec<f64>, Vec<f64>) {
        let n = x.len();
        let mut re = vec![0.0; n / 2 + 1];
        let mut im = vec![0.0; n / 2 + 1];
        for k in 0..=n / 2 {
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * PI * (k * t % n) as f64 / n as f64;
                re[k] += v as f64 * a.cos();
                im[k] += v as f64 * a.sin();
            }
        }
        (re, im)
    }

    #[test]
    fn matches_a_direct_dft() {
        let mut rng = Rng(99);
        for n in [4usize, 8, 16, 64, 256] {
            let x: Vec<f32> = (0..n).map(|_| rng.next()).collect();
            let (wr, wi) = naive_dft(&x);
            let mut f = Fft::new(n);
            let (mut re, mut im) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
            f.forward(&x, &mut re, &mut im);
            for k in 0..=n / 2 {
                assert!((re[k] as f64 - wr[k]).abs() < 2e-4 * (n as f64).sqrt(), "n={n} k={k} re {} vs {}", re[k], wr[k]);
                assert!((im[k] as f64 - wi[k]).abs() < 2e-4 * (n as f64).sqrt(), "n={n} k={k} im");
            }
            assert_eq!((im[0], im[n / 2]), (0.0, 0.0));
        }
    }

    #[test]
    fn known_signals() {
        let n = 1024;
        let mut f = Fft::new(n);
        let (mut re, mut im) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
        // impulse: flat spectrum of 1
        let mut x = vec![0.0f32; n];
        x[0] = 1.0;
        f.forward(&x, &mut re, &mut im);
        assert!(re.iter().all(|&v| (v - 1.0).abs() < 1e-6) && im.iter().all(|&v| v.abs() < 1e-6));
        // constant: all energy in bin 0
        f.forward(&vec![0.5f32; n], &mut re, &mut im);
        assert!((re[0] - 512.0).abs() < 1e-3);
        assert!(re[1..].iter().chain(&im[1..]).all(|&v| v.abs() < 1e-3));
        // cosine on bin 37: n/2 in that bin; sine gives -i n/2
        let c: Vec<f32> = (0..n).map(|t| (2.0 * PI * 37.0 * t as f64 / n as f64).cos() as f32).collect();
        f.forward(&c, &mut re, &mut im);
        assert!((re[37] - n as f32 / 2.0).abs() < 1e-2 && im[37].abs() < 1e-2);
        assert!(re.iter().enumerate().all(|(k, &v)| k == 37 || v.abs() < 1e-2));
        let s: Vec<f32> = (0..n).map(|t| (2.0 * PI * 5.0 * t as f64 / n as f64).sin() as f32).collect();
        f.forward(&s, &mut re, &mut im);
        assert!(re[5].abs() < 1e-2 && (im[5] + n as f32 / 2.0).abs() < 1e-2);
        // Nyquist: alternating signs
        let alt: Vec<f32> = (0..n).map(|t| if t % 2 == 0 { 1.0 } else { -1.0 }).collect();
        f.forward(&alt, &mut re, &mut im);
        assert!((re[n / 2] - n as f32).abs() < 1e-3);
        // Parseval
        let mut rng = Rng(5);
        let x: Vec<f32> = (0..n).map(|_| rng.next()).collect();
        f.forward(&x, &mut re, &mut im);
        let time: f64 = x.iter().map(|&v| (v * v) as f64).sum();
        let mut freq = (re[0] * re[0]) as f64 + (re[n / 2] * re[n / 2]) as f64;
        for k in 1..n / 2 {
            freq += 2.0 * (re[k] * re[k] + im[k] * im[k]) as f64;
        }
        assert!((freq / n as f64 - time).abs() / time < 1e-4);
    }

    #[test]
    fn inverse_round_trip() {
        let mut rng = Rng(1234);
        for n in [4usize, 16, 128, 1024, 8192] {
            let x: Vec<f32> = (0..n).map(|_| rng.next()).collect();
            let mut f = Fft::new(n);
            let (mut re, mut im) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
            let mut y = vec![0.0; n];
            f.forward(&x, &mut re, &mut im);
            f.inverse(&re, &im, &mut y);
            let worst = x.iter().zip(&y).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 2e-5, "n={n}: {worst}");
        }
    }

    fn direct(x: &[f32], h: &[f32]) -> Vec<f32> {
        (0..x.len()).map(|n| (0..h.len().min(n + 1)).map(|k| h[k] as f64 * x[n - k] as f64).sum::<f64>() as f32).collect()
    }

    #[test]
    fn convolver_matches_direct_convolution() {
        let mut rng = Rng(77);
        for (block, ir_len) in [(8usize, 1usize), (8, 8), (8, 9), (16, 100), (64, 1000), (32, 31)] {
            let h: Vec<f32> = (0..ir_len).map(|_| rng.next() * 0.3).collect();
            let blocks = 12;
            let x: Vec<f32> = (0..block * blocks).map(|_| rng.next()).collect();
            let want = direct(&x, &h);
            let mut c = PartitionedConvolver::new(block, &h);
            assert_eq!(c.partitions(), ir_len.div_ceil(block));
            let mut got = vec![0.0f32; x.len()];
            for (i, o) in x.chunks(block).zip(got.chunks_mut(block)) {
                c.process(i, o);
            }
            let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-4, "block {block}, ir {ir_len}: {worst}");
        }
    }

    #[test]
    fn convolver_impulse_response_and_reset() {
        let h: Vec<f32> = (0..50).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut c = PartitionedConvolver::new(16, &h);
        let mut out = vec![0.0f32; 16 * 5];
        let mut imp = vec![0.0f32; 16 * 5];
        imp[0] = 1.0;
        for (i, o) in imp.chunks(16).zip(out.chunks_mut(16)) {
            c.process(i, o);
        }
        for n in 0..80 {
            let want = if n < 50 { h[n] } else { 0.0 };
            assert!((out[n] - want).abs() < 1e-5, "n={n}");
        }
        c.reset();
        let mut o = vec![0.0f32; 16];
        c.process(&[0.0; 16], &mut o);
        assert!(o.iter().all(|&v| v.abs() < 1e-7), "no tail after reset");
        assert_eq!(c.block_size(), 16);
    }
}
