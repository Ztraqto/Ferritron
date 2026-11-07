//! `cargo run --release -p ferritron-bench`
//!
//! Measures how much `ctx.parallel_for` speeds up per-voice work, and what a
//! parallel dispatch costs, by driving a test plugin through
//! `ferritron::testing::Harness` (the same path hosts use).

use ferritron::testing::Harness;
use ferritron_bench::{BLOCK, SAMPLE_RATE, VOICES, Voices};
use std::time::Instant;

/// Renders `blocks` blocks; returns (seconds per block, rendered audio).
fn render(harmonics: f64, blocks: usize) -> (f64, Vec<f32>) {
    let mut h = Harness::<Voices>::new(SAMPLE_RATE, BLOCK);
    h.set_param("harmonics", harmonics);
    let mut all = Vec::with_capacity(blocks * BLOCK);
    let mut buf = vec![0.0f32; BLOCK];
    // Warm up (worker threads, caches).
    for _ in 0..20 {
        h.process(&mut [&mut buf]);
    }
    let start = Instant::now();
    for _ in 0..blocks {
        assert!(h.process(&mut [&mut buf]));
        all.extend_from_slice(&buf);
    }
    (start.elapsed().as_secs_f64() / blocks as f64, all)
}

fn main() {
    let block_seconds = BLOCK as f64 / SAMPLE_RATE;
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("Ferritron parallel benchmark: {VOICES} voices, {BLOCK}-sample blocks at {SAMPLE_RATE} Hz, {cores} logical cores");
    println!("(real-time budget per block: {:.0} us)\n", block_seconds * 1e6);
    println!("{:<34} {:>14} {:>14} {:>9}", "work per voice", "serial / block", "parallel", "speedup");

    for (label, harmonics, blocks) in [("heavy (48 harmonics)", 48.0, 400), ("light (1 harmonic)", 1.0, 4000)] {
        // SAFETY: single-threaded at this point; the variable is read when the pool starts.
        unsafe { std::env::set_var("FERRITRON_WORKERS", "0") };
        let (serial, serial_audio) = render(harmonics, blocks);
        unsafe { std::env::remove_var("FERRITRON_WORKERS") };
        let (parallel, parallel_audio) = render(harmonics, blocks);
        assert_eq!(serial_audio, parallel_audio, "parallel output must match serial output exactly");
        println!(
            "{label:<34} {:>11.1} us {:>11.1} us {:>8.2}x",
            serial * 1e6,
            parallel * 1e6,
            serial / parallel
        );
    }
    println!("\nOutputs of the serial and parallel runs are identical.");
    gpu_convolution(2);
    gpu_convolution(10);
    gpu_filter_bank();
    offline_bounce();
    placement();
    fusion();
    batching();
    round_trip();
}

/// Verification item V2 (design doc 9.3): what one GPU round trip costs,
/// with almost no work in it: the floor that persistent kernels would remove.
fn round_trip() {
    use ferritron_gpu::{Biquad, IirBank, Lane};
    let lanes = [Lane { input: 0, output: 0, sections: vec![Biquad::IDENTITY] }];
    let Ok(mut bank) = IirBank::new(&lanes, 1) else { return println!("
GPU unavailable") };
    let input = [0.0f32];
    let mut out = [0.0f32];
    let mut times: Vec<f64> = (0..2000)
        .map(|_| {
            let start = Instant::now();
            bank.process(&[&input], &mut [&mut out]);
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!(
        "
GPU round trip with no real work (V2), {} runs: median {:.1} us, 99% {:.1} us, max {:.1} us ({} us at 64 samples is the whole budget)",
        times.len(),
        at(0.5),
        at(0.99),
        at(1.0),
        (64.0 / SAMPLE_RATE * 1e6) as u32
    );
}

/// Batching across instances (design doc 9.2): the same plugin on 32 tracks,
/// each with a 256-filter bank, one GPU submission per block for all.
fn batching() {
    use ferritron_gpu::{BatchJob, Biquad, IirBank, Lane, RealtimeIirBank};
    let instances = 32;
    let lanes: Vec<Lane> = (0..256)
        .map(|i| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(100.0 + 9.0 * i as f64, 80.0, SAMPLE_RATE)] })
        .collect();
    println!("
Batching across instances: {instances} instances x 256 filters, {BLOCK}-sample blocks:");
    let Ok(mut banks) = (0..instances).map(|_| IirBank::new(&lanes, BLOCK)).collect::<Result<Vec<_>, _>>() else {
        return println!("  GPU unavailable");
    };
    let input = vec![vec![0.0f32; BLOCK]];
    let mut outs = vec![vec![vec![0.0f32; BLOCK]]; instances];
    let rounds = 100;
    let start = Instant::now();
    for _ in 0..rounds {
        for (bank, out) in banks.iter_mut().zip(outs.iter_mut()) {
            bank.process(&[&input[0][..]], &mut [&mut out[0][..]]);
        }
    }
    let one_by_one = start.elapsed().as_secs_f64() / rounds as f64 * 1e6;
    let start = Instant::now();
    for _ in 0..rounds {
        let mut jobs: Vec<BatchJob<'_, Vec<f32>, Vec<f32>>> = banks
            .iter_mut()
            .zip(outs.iter_mut())
            .map(|(bank, out)| BatchJob { bank, inputs: &input, outputs: &mut out[..], frames: BLOCK })
            .collect();
        IirBank::process_many(&mut jobs);
    }
    let batched = start.elapsed().as_secs_f64() / rounds as f64 * 1e6;
    let budget = BLOCK as f64 / SAMPLE_RATE * 1e6;
    println!("  one submission per instance: {one_by_one:>7.1} us per block ({:.0}% of the budget)", one_by_one / budget * 100.0);
    println!("  one submission for all:      {batched:>7.1} us per block ({:.0}% of the budget)", batched / budget * 100.0);

    // Real time: 32 RealtimeIirBanks share the GPU thread, which batches them.
    let mut rt: Vec<RealtimeIirBank> = (0..instances).map(|_| RealtimeIirBank::new(&lanes, BLOCK).expect("GPU")).collect();
    let block_time = std::time::Duration::from_secs_f64(BLOCK as f64 / SAMPLE_RATE);
    let mut buf = vec![0.0f32; BLOCK];
    let (mut gpu, blocks) = (0u64, 300);
    for b in 0..blocks {
        let begun = Instant::now();
        for bank in rt.iter_mut() {
            buf.fill(if b % 50 == 0 { 1.0 } else { 0.0 });
            bank.process(&[&input[0][..]], &mut [&mut buf[..]]);
        }
        gpu += rt[0].last_gpu_micros();
        std::thread::sleep(block_time.saturating_sub(begun.elapsed()));
    }
    let missed: u64 = rt.iter().map(|b| b.missed_blocks()).sum();
    let fallback: u64 = rt.iter().map(|b| b.fallback_blocks()).sum();
    println!(
        "  real time, batched on one GPU thread: {:.1} us of GPU time per block for all {instances}; missed {missed}, CPU fallback {fallback} of {}",
        gpu as f64 / blocks as f64,
        blocks * instances
    );
}

/// Node fusion (design doc 9.7): three filter banks in series, as separate
/// GPU banks (one round trip each) and as one fused chain (one round trip).
fn fusion() {
    use ferritron_gpu::{Biquad, IirBank, Lane, RealtimeIirBank};
    let rate = SAMPLE_RATE;
    let modes: Vec<Lane> = (0..1024)
        .map(|i| Lane { input: 0, output: i % 16, sections: vec![Biquad::bandpass(80.0 + 2.5 * i as f64, 200.0, rate)] })
        .collect();
    let eq: Vec<Lane> = (0..16)
        .map(|c| Lane { input: c, output: c, sections: vec![Biquad::peaking(800.0, 1.0, 4.0, rate), Biquad::highpass(60.0, 0.7, rate)] })
        .collect();
    let mixdown: Vec<Lane> = (0..16).map(|c| Lane { input: c, output: c % 2, sections: vec![Biquad::lowpass(9000.0, 0.7, rate)] }).collect();
    let stages: [&[Lane]; 3] = [&modes, &eq, &mixdown];
    println!("
Node fusion: 3 IIR banks in series (1,024 resonators -> 16-channel EQ -> stereo), {BLOCK}-sample blocks:");

    let Ok(mut fused) = IirBank::chain(&stages, BLOCK) else { return println!("  GPU unavailable") };
    let mut separate: Vec<IirBank> = stages.iter().map(|s| IirBank::new(s, BLOCK).expect("GPU")).collect();
    let input = vec![0.0f32; BLOCK];
    let mut a = vec![vec![0.0f32; BLOCK]; 16];
    let mut b = vec![vec![0.0f32; BLOCK]; 16];
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let blocks = 300;
    let start = Instant::now();
    for _ in 0..blocks {
        fused.process(&[&input], &mut out.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
    }
    let fused_us = start.elapsed().as_secs_f64() / blocks as f64 * 1e6;
    let start = Instant::now();
    for _ in 0..blocks {
        separate[0].process(&[&input], &mut a.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
        separate[1].process(&a.iter().map(|c| &c[..]).collect::<Vec<_>>(), &mut b.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
        separate[2].process(&b.iter().map(|c| &c[..]).collect::<Vec<_>>(), &mut out.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
    }
    let separate_us = start.elapsed().as_secs_f64() / blocks as f64 * 1e6;
    let rt_latency = RealtimeIirBank::chain(&stages, BLOCK).map_or(0, |b| b.latency());
    println!("  separate banks: {separate_us:>7.1} us per block (3 round trips), real-time latency {} samples", 3 * rt_latency);
    println!("  fused chain:    {fused_us:>7.1} us per block (1 round trip),  real-time latency {rt_latency} samples");

    // As nodes of a graph, where prepare() fuses them: the light chain above
    // (its small stages stay on the CPU anyway), and a heavy one whose
    // stages each go to the GPU on their own.
    use ferritron_gpu::IirNode;
    use ferritron_gpu::graph::{Graph, NodeSetup, Policy};
    let wide = |count: usize, outputs: usize| -> Vec<Lane> {
        (0..count)
            .map(|i| Lane { input: i % 16, output: i % outputs, sections: vec![Biquad::peaking(200.0 + 7.0 * i as f64, 2.0, 1.0, rate)] })
            .collect()
    };
    let (heavy_eq, heavy_mix) = (wide(1024, 16), wide(1024, 2));
    let heavy: [&[Lane]; 3] = [&modes, &heavy_eq, &heavy_mix];
    for (name, stages) in [("light", stages), ("heavy", heavy)] {
        println!("  graph of 3 IirNodes, {name} ({} + {} + {} filters):", stages[0].len(), stages[1].len(), stages[2].len());
        for fusion in [false, true] {
            for offline in [true, false] {
                let setup = NodeSetup { sample_rate: rate, max_block: BLOCK, offline, policy: Policy::Max };
                let mut g = Graph::new(1, 2);
                let ids: Vec<_> = stages.iter().map(|s| g.add(IirNode::new(s, &setup))).collect();
                g.connect(Graph::INPUT, 0, ids[0], 0).expect("valid");
                g.connect_all(ids[0], ids[1]).expect("valid");
                g.connect_all(ids[1], ids[2]).expect("valid");
                g.connect_all(ids[2], Graph::OUTPUT).expect("valid");
                g.set_fusion(fusion);
                g.prepare(BLOCK).expect("valid graph");
                if offline {
                    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
                    let start = Instant::now();
                    for _ in 0..blocks {
                        g.process(&[&input], &mut [&mut l, &mut r]);
                    }
                    let us = start.elapsed().as_secs_f64() / blocks as f64 * 1e6;
                    print!("    fusion {:<3}: offline {us:>7.1} us per block", if fusion { "on" } else { "off" });
                } else {
                    println!(", real-time latency {:>4} samples ({} nodes fused)", g.latency(), g.fused_nodes());
                }
            }
        }
    }
}

/// Automatic placement (design doc 9.2): which device an IirNode picks for
/// banks of different sizes, and what it measured (us per block).
fn placement() {
    use ferritron_gpu::graph::{Device, NodeSetup, Policy};
    use ferritron_gpu::{Biquad, IirNode, Lane};
    println!("
Automatic placement of an IIR bank (policy max), {BLOCK}-sample blocks:");
    println!("  {:>8}  {:<26} {:<26}", "filters", "real time: CPU / GPU", "offline: CPU / GPU");
    for count in [8, 64, 256, 1024, 4096] {
        let lanes: Vec<Lane> = (0..count)
            .map(|i| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(100.0 + i as f64, 100.0, SAMPLE_RATE)] })
            .collect();
        let row = |offline| {
            let node = IirNode::new(&lanes, &NodeSetup { sample_rate: SAMPLE_RATE, max_block: BLOCK, offline, policy: Policy::Max });
            let us = |d| node.measurements().iter().find(|m| m.device == d).map_or("-".into(), |m| format!("{:.1}", m.micros));
            let pick = if node.device() == Device::Gpu { "GPU" } else { "CPU" };
            format!("{} / {} -> {pick}", us(Device::Cpu), us(Device::Gpu))
        };
        println!("  {count:>8}  {:<26} {:<26}", row(false), row(true));
    }
}

/// A DAW bounce: 60 s through the 2 s reverb, blocks as fast as `process`
/// returns. Real-time mode drops tails it cannot wait for; offline mode waits.
fn offline_bounce() {
    use ferritron_gpu::{Convolver, Mode};
    let taps = 2 * SAMPLE_RATE as usize;
    let ir: Vec<f32> = (0..taps).map(|i| ((i * 7919 % 1000) as f32 / 500.0 - 1.0) * (-(i as f32) / (taps as f32 / 5.0)).exp()).collect();
    let frames = 60 * SAMPLE_RATE as usize;
    println!("
Offline bounce of 60 s through the 2 s reverb, {BLOCK}-sample blocks, as fast as possible:");
    for mode in [Mode::Realtime, Mode::Offline] {
        let Ok(mut conv) = Convolver::with_mode(&ir, BLOCK, mode) else { return println!("  GPU unavailable") };
        let mut buf = vec![0.0f32; BLOCK];
        let start = Instant::now();
        for i in 0..frames / BLOCK {
            buf.iter_mut().enumerate().for_each(|(j, s)| *s = ((i * BLOCK + j) % 97) as f32 / 97.0 - 0.5);
            conv.process(&mut buf);
        }
        let seconds = start.elapsed().as_secs_f64();
        let grid_blocks = frames / BLOCK; // grid size equals BLOCK here
        println!(
            "  {:<9} {:>6.2} s ({:>5.1}x real time), tails missing in {} of {grid_blocks} blocks",
            format!("{mode:?}:"),
            seconds,
            60.0 / seconds,
            conv.missed_blocks()
        );
    }
}

/// Modal synthesis: 2,000 resonators (high-Q band-pass biquads) summed into
/// one output, as a real-time plugin (256-sample blocks) and as an offline
/// render of 10 s. CPU: transposed direct form II on one core.
fn gpu_filter_bank() {
    use ferritron_gpu::{Biquad, IirBank, Lane, RealtimeIirBank};
    let modes = 2000;
    let lanes: Vec<Lane> = (0..modes)
        .map(|i| {
            let freq = 80.0 * 1.0018f64.powi(i as i32); // 80 Hz .. 2.9 kHz
            Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(freq, 200.0 + (i % 7) as f64 * 50.0, SAMPLE_RATE)] }
        })
        .collect();
    println!("\nFilter bank: {modes} resonators summed (modal synthesis), {BLOCK}-sample blocks:");
    let input: Vec<f32> = (0..BLOCK * 400).map(|i| if i % 12_000 == 0 { 1.0 } else { 0.0 }).collect();

    // CPU: every resonator, sample by sample.
    let mut state = vec![[0.0f32; 2]; modes];
    let mut out = vec![0.0f32; BLOCK];
    let rounds = 50;
    let start = Instant::now();
    for block in input.chunks(BLOCK).take(rounds) {
        out.fill(0.0);
        for (lane, s) in lanes.iter().zip(&mut state) {
            let c = lane.sections[0];
            for (o, &x) in out.iter_mut().zip(block) {
                let y = c.b0 * x + s[0];
                s[0] = c.b1 * x - c.a1 * y + s[1];
                s[1] = c.b2 * x - c.a2 * y;
                *o += y;
            }
        }
    }
    std::hint::black_box(&out);
    let cpu = start.elapsed().as_secs_f64() / rounds as f64;

    let mut rt = match RealtimeIirBank::new(&lanes, BLOCK) {
        Ok(b) => b,
        Err(e) => return println!("  GPU unavailable: {e}"),
    };
    let block_time = std::time::Duration::from_secs_f64(BLOCK as f64 / SAMPLE_RATE);
    let (mut audio_thread, mut gpu_total, blocks) = (0.0, 0u64, 400);
    for block in input.chunks(BLOCK).take(blocks) {
        let begun = Instant::now();
        rt.process(&[block], &mut [&mut out[..]]);
        audio_thread += begun.elapsed().as_secs_f64();
        std::thread::sleep(block_time.saturating_sub(begun.elapsed()));
        gpu_total += rt.last_gpu_micros();
    }
    let budget = block_time.as_secs_f64();
    println!("  CPU, 1 core:                  {:>8.1} us per block ({:.0}% of the budget)", cpu * 1e6, cpu / budget * 100.0);
    println!("  Real-time bank, audio thread: {:>8.1} us per block", audio_thread / blocks as f64 * 1e6);
    println!(
        "  GPU ({}): {:.1} us per block, latency {} samples, missed {} of {blocks} blocks",
        rt.adapter(),
        gpu_total as f64 / blocks as f64,
        rt.latency(),
        rt.missed_blocks()
    );

    // Offline: 10 s at once in 8192-sample blocks.
    let frames = 10 * SAMPLE_RATE as usize;
    let long = 8192;
    let mut bank = IirBank::new(&lanes, long).expect("the GPU worked a moment ago");
    let input: Vec<f32> = (0..frames).map(|i| if i % 12_000 == 0 { 1.0 } else { 0.0 }).collect();
    let mut out = vec![0.0f32; long];
    let start = Instant::now();
    for block in input.chunks(long) {
        bank.process(&[block], &mut [&mut out[..block.len()]]);
    }
    let gpu = start.elapsed().as_secs_f64();
    let cpu_total = cpu * (frames / BLOCK) as f64;
    println!("  Offline render of 10 s: CPU {:.2} s (estimated), GPU {:.3} s ({:.0}x faster)", cpu_total, gpu, cpu_total / gpu);
}

/// An impulse response of `seconds` at 48 kHz, 256-sample blocks: direct
/// convolution on one CPU core vs. CPU head + GPU tail, played in real time.
fn gpu_convolution(seconds: usize) {
    let taps = seconds * SAMPLE_RATE as usize;
    let ir: Vec<f32> = (0..taps).map(|i| ((i * 7919 % 1000) as f32 / 500.0 - 1.0) * (-(i as f32) / (taps as f32 / 5.0)).exp()).collect();
    println!("\nConvolution with a {seconds} s impulse response ({taps} taps), {BLOCK}-sample blocks:");

    // CPU: direct convolution of one block against the whole IR.
    let history = vec![0.1f32; taps + BLOCK];
    let start = Instant::now();
    let rounds = 4;
    let mut sink = 0.0;
    for _ in 0..rounds {
        for t in 0..BLOCK {
            let mut acc = 0.0;
            for (k, h) in ir.iter().enumerate() {
                acc += h * history[taps + t - k];
            }
            sink += acc;
        }
    }
    let cpu = start.elapsed().as_secs_f64() / rounds as f64;
    std::hint::black_box(sink);

    let begun = Instant::now();
    let mut conv = match ferritron_gpu::Convolver::new(&ir, BLOCK) {
        Ok(c) => c,
        Err(e) => return println!("  GPU unavailable: {e}"),
    };
    let setup = begun.elapsed();
    let block_time = std::time::Duration::from_secs_f64(BLOCK as f64 / SAMPLE_RATE);
    let (mut audio_thread, mut gpu_total, blocks) = (0.0, 0u64, 400);
    let mut buf = vec![0.0f32; BLOCK];
    for i in 0..blocks {
        buf.iter_mut().enumerate().for_each(|(j, s)| *s = ((i * BLOCK + j) % 97) as f32 / 97.0 - 0.5);
        let begun = Instant::now();
        conv.process(&mut buf);
        audio_thread += begun.elapsed().as_secs_f64();
        std::thread::sleep(block_time.saturating_sub(begun.elapsed())); // real-time pacing
        gpu_total += conv.last_gpu_micros();
    }
    let budget = block_time.as_secs_f64();
    println!("  CPU direct, 1 core:           {:>8.1} us per block ({:.0}% of the {:.0} us budget)", cpu * 1e6, cpu / budget * 100.0, budget * 1e6);
    println!("  CPU head on the audio thread: {:>8.1} us per block", audio_thread / blocks as f64 * 1e6);
    println!("  GPU tail ({}): {:.1} us per block (setup {:.0} ms)", conv.adapter(), gpu_total as f64 / blocks as f64, setup.as_secs_f64() * 1e3);
    println!("  Missed GPU deadlines: {} of {blocks} blocks", conv.missed_blocks());
}
