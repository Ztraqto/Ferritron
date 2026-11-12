//! S8 micro-benchmarks (X7 of the 2.0 plan): ns per sample for every part.
//!
//! `cargo test --release -p ferritron-dsp s8 -- --ignored --nocapture`
//!
//! The numbers depend on the machine and are only meaningful from a release build. The
//! test checks nothing but that the work was done (finite results); it prints a table.

use crate::fastmath;
use crate::*;
use std::hint::black_box;
use std::time::Instant;

const BLOCK: usize = 512;

/// Best of 5 runs, in ns per sample.
fn measure(mut f: impl FnMut()) -> f64 {
    let reps = 400;
    f(); // warm-up
    (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..reps {
                f();
            }
            t.elapsed().as_secs_f64() * 1e9 / (reps * BLOCK) as f64
        })
        .fold(f64::MAX, f64::min)
}

fn signal(n: usize) -> Vec<f32> {
    let mut s = 0x1234_5678u32;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1 << 24) as f32 * 1.8 - 0.9
        })
        .collect()
}

#[test]
#[ignore]
fn s8_micro_benchmarks() {
    let input = signal(BLOCK);
    let mut buf = input.clone();
    let mut rows: Vec<(String, f64)> = Vec::new();
    let mut add = |name: &str, v: f64| rows.push((name.to_string(), v));

    // smoothing
    for (n, kind) in [("Smoother linear", SmoothKind::Linear), ("Smoother log", SmoothKind::Log), ("Smoother exp", SmoothKind::Exp)] {
        let mut s = Smoother::new(kind, 1.0);
        s.set_time_constant(100.0);
        let mut flip = false;
        add(n, measure(|| {
            flip = !flip;
            s.set_target(if flip { 2.0 } else { 1.0 }, BLOCK as u32);
            s.fill(black_box(&mut buf));
        }));
    }

    // filters
    let c = design::peaking(48_000.0, 1000.0, 1.0, 6.0);
    let mut bq = Biquad::new(&c);
    add("Biquad (1 filter)", measure(|| {
        buf.copy_from_slice(&input);
        bq.process_block(black_box(&mut buf));
    }));
    let mut x4 = BiquadX4::new(&[c; 4]);
    let mut b4: Vec<Vec<f32>> = (0..4).map(|_| input.clone()).collect();
    add("BiquadX4 (4 filters, per filter-sample)", measure(|| {
        let [a, b, c2, d] = &mut b4[..] else { unreachable!() };
        x4.process_block(black_box([a, b, c2, d]));
    }) / 4.0);
    let mut x8 = BiquadX8::new(&[c; 8]);
    let mut inter = signal(BLOCK * 8);
    add(if x8.uses_avx2() { "BiquadX8 AVX2 (8 filters, per filter-sample)" } else { "BiquadX8 portable (per filter-sample)" }, measure(|| {
        x8.process_interleaved(black_box(&mut inter));
    }) / 8.0);
    let mut x8p = BiquadX8::with_isa(&[c; 8], Isa::Scalar);
    add("BiquadX8 portable path (per filter-sample)", measure(|| {
        x8p.process_interleaved(black_box(&mut inter));
    }) / 8.0);

    // delay, sliding, hold, fade
    let mut dl = DelayLine::new(2048);
    add("DelayLine integer", measure(|| {
        for s in buf.iter_mut() {
            *s = dl.process(*s, 1000);
        }
        black_box(&buf);
    }));
    add("DelayLine fractional", measure(|| {
        for s in buf.iter_mut() {
            *s = dl.process_frac(*s, 1000.37);
        }
        black_box(&buf);
    }));
    let mut sm = SlidingMax::new(64);
    add("SlidingMax (window 64)", measure(|| {
        for s in buf.iter_mut() {
            *s = sm.push(*s);
        }
        black_box(&buf);
    }));
    let mut ph = PeakHold::new(100, 4800.0);
    add("PeakHold", measure(|| {
        for s in buf.iter_mut() {
            *s = ph.process(*s);
        }
        black_box(&buf);
    }));
    let mut la = Lookahead::new(64);
    add("Lookahead", measure(|| {
        for s in buf.iter_mut() {
            *s = la.process(*s, s.abs()).1;
        }
        black_box(&buf);
    }));
    let mut out = vec![0.0f32; BLOCK];
    for (n, law) in [("Crossfade linear (at rest)", FadeLaw::Linear), ("Crossfade equal power (at rest)", FadeLaw::EqualPower)] {
        let mut cf = Crossfade::new(law, 0.3);
        add(n, measure(|| cf.process_block(black_box(&input), black_box(&input), &mut out)));
    }
    let mut cf = Crossfade::new(FadeLaw::Linear, 0.0);
    let mut up = false;
    add("Crossfade linear (ramping)", measure(|| {
        up = !up;
        cf.set_target(if up { 1.0 } else { 0.0 }, BLOCK as u32);
        cf.process_block(black_box(&input), black_box(&input), &mut out);
    }));

    // math, tables, shapers, dither
    macro_rules! per_sample {
        ($name:expr, $f:expr) => {
            add($name, measure(|| {
                for (o, &x) in out.iter_mut().zip(input.iter()) {
                    *o = $f(black_box(x));
                }
                black_box(&out);
            }));
        };
    }
    per_sample!("fastmath::log2", |x: f32| fastmath::log2(x.abs() + 0.01));
    per_sample!("std f32::log2 (reference)", |x: f32| (x.abs() + 0.01).log2());
    per_sample!("fastmath::exp2", |x: f32| fastmath::exp2(x * 8.0));
    per_sample!("std f32::exp2 (reference)", |x: f32| (x * 8.0).exp2());
    per_sample!("fastmath::db_to_gain", |x: f32| fastmath::db_to_gain(x * 40.0));
    per_sample!("fastmath::gain_to_db", |x: f32| fastmath::gain_to_db(x.abs() + 0.01));
    per_sample!("fastmath::tanh", fastmath::tanh);
    per_sample!("std f32::tanh (reference)", |x: f32| (x * 3.0).tanh());
    let lut = Lut::new(-1.0, 1.0, 1024, |x| x.sin());
    per_sample!("Lut (1024)", |x: f32| lut.get(x));
    for (n, k) in [
        ("Shaper tanh (block, 4 lanes)", ShaperKind::Tanh),
        ("Shaper atan (block, 4 lanes)", ShaperKind::Atan),
        ("Shaper poly2 (block, 4 lanes)", ShaperKind::Poly2),
        ("Shaper poly3 (block, 4 lanes)", ShaperKind::Poly3),
        ("Shaper algebraic (block, 4 lanes)", ShaperKind::Algebraic),
    ] {
        let sh = Shaper::new(k);
        add(n, measure(|| {
            buf.copy_from_slice(&input);
            sh.process_block(black_box(&mut buf));
        }));
    }
    let sh = Shaper::new(ShaperKind::Tanh);
    per_sample!("Shaper tanh (scalar)", |x: f32| sh.shape(x));
    let mut dither = Dither::new(1, 16);
    per_sample!("Dither TPDF quantize", |x: f32| dither.quantize(x));

    // frequency domain
    for n in [256usize, 1024, 4096] {
        let mut f = Fft::new(n);
        let x = signal(n);
        let (mut re, mut im) = (vec![0.0f32; n / 2 + 1], vec![0.0f32; n / 2 + 1]);
        let mut y = vec![0.0f32; n];
        let t = measure(|| {
            f.forward(black_box(&x), &mut re, &mut im);
            f.inverse(&re, &im, &mut y);
        }) * BLOCK as f64 / n as f64;
        add(&format!("Fft {n} forward+inverse"), t / 2.0);
    }
    for (block, ir_len) in [(128usize, 2048usize), (512, 48_000)] {
        let ir = signal(ir_len);
        let mut c = PartitionedConvolver::new(block, &ir);
        let x = signal(block);
        let mut y = vec![0.0f32; block];
        let t = measure(|| c.process(black_box(&x), &mut y)) * BLOCK as f64 / block as f64;
        add(&format!("PartitionedConvolver block {block}, IR {ir_len}"), t);
    }
    for factor in [2usize, 4, 8, 16] {
        let mut os = Oversampler::new(factor, BLOCK);
        let mut hi = vec![0.0f32; BLOCK * factor];
        let mut back = vec![0.0f32; BLOCK];
        add(&format!("Oversampler {factor}x up+down (per host sample)"), measure(|| {
            os.upsample(black_box(&input), &mut hi);
            os.downsample(&hi, &mut back);
        }));
    }
    black_box((&buf, &out, &b4, &inter));

    println!("\nS8 micro-benchmarks, {BLOCK}-sample blocks, ISA in use: {:?}", isa());
    for (name, v) in &rows {
        assert!(v.is_finite() && *v > 0.0, "{name}");
        println!("{name:<52} {v:>9.3} ns/sample");
    }
}
