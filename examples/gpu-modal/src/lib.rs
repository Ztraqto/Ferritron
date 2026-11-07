//! A modal synthesizer: every note rings 128 resonators (the modes of a
//! slightly stiff string or bar), 16 voices, 2,048 resonators in all. They
//! are one `IirNode` (design doc 9.2 / 9.4), which measures the CPU and the
//! GPU and runs where it is faster. The plugin asks for the "max" GPU
//! policy (latency allowed); users can override it with
//! `FERRITRON_GPU_POLICY`.
//!
//! In real time the GPU bank delays its output by two blocks, and the plugin
//! reports that latency to the host. When the host renders offline, the
//! plugin uses a bank that waits for the GPU instead: no latency, no gaps. Releasing a note shortens its decay: the bank keeps
//! the filter state across coefficient changes, so this does not click.

use ferritron::NoteKind;
use ferritron::prelude::*;
use ferritron_gpu::graph::{Node, NodeSetup, Policy};
use ferritron_gpu::{Biquad, IirNode, Lane};

const VOICES: usize = 16;
const MODES: usize = 128;
/// Mallet: a short burst of windowed noise, in samples.
const MALLET: usize = 96;
/// Decay after a note is released, in seconds.
const DAMPED: f64 = 0.15;
/// Inharmonicity of the modes: f_k = f0 · k · sqrt(1 + B·k²).
const STIFFNESS: f64 = 0.0004;

#[derive(Clone, Copy, Default)]
struct Voice {
    key: u8,
    held: bool,
    started: u64,
    velocity: f32,
    /// Mallet samples still to play, and where in the current block they start.
    mallet_left: usize,
    mallet_at: usize,
    seed: u32,
}

/// Set in tests to compare the CPU path with the GPU path.
#[cfg(test)]
static FORCE_CPU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Default)]
struct GpuModal {
    bank: Option<IirNode>,
    voices: [Voice; VOICES],
    /// Coefficients each voice started with, for damping on release.
    struck: Vec<Biquad>,
    inputs: Vec<Vec<f32>>,
    sample_rate: f64,
    clock: u64,
}

/// A resonator at `freq` that decays by 60 dB in `t60` seconds and rings
/// with peak amplitude about `amp` after a unit impulse.
fn mode(freq: f64, t60: f64, amp: f64, rate: f64) -> Biquad {
    if freq >= rate * 0.45 {
        return Biquad { b0: 0.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }; // above the audio band
    }
    let q = (std::f64::consts::PI * freq * t60 / 6.91).max(0.5);
    let mut b = Biquad::bandpass(freq, q, rate);
    // A band-pass rings with amplitude about 2·b0 after an impulse.
    let gain = (amp / (2.0 * b.b0 as f64)) as f32;
    b.b0 *= gain;
    b.b1 *= gain;
    b.b2 *= gain;
    b
}

impl GpuModal {
    fn note_on(&mut self, key: u8, velocity: f32, offset: usize, decay: f64, brightness: f64) {
        // Reuse a free voice, or steal the oldest one.
        let slot = (0..VOICES)
            .find(|&v| !self.voices[v].held && self.voices[v].started == 0)
            .unwrap_or_else(|| (0..VOICES).min_by_key(|&v| self.voices[v].started).unwrap_or(0));
        self.clock += 1;
        self.voices[slot] = Voice {
            key,
            held: true,
            started: self.clock,
            velocity,
            mallet_left: MALLET,
            mallet_at: offset,
            seed: 0x9e37_79b9 ^ (key as u32) << 8 ^ slot as u32,
        };
        let f0 = 440.0 * 2f64.powf((key as f64 - 69.0) / 12.0);
        let rolloff = 2.0 - 1.8 * brightness;
        let Some(bank) = self.bank.as_mut() else { return };
        for k in 0..MODES {
            let n = (k + 1) as f64;
            let freq = f0 * n * (1.0 + STIFFNESS * n * n).sqrt();
            let c = mode(freq, decay / (1.0 + 0.03 * k as f64), 0.15 * n.powf(-rolloff), self.sample_rate);
            self.struck[slot * MODES + k] = c;
            bank.set_lane(slot * MODES + k, &[c]);
        }
    }

    fn note_off(&mut self, key: u8) {
        let rate = self.sample_rate;
        for slot in 0..VOICES {
            let voice = &mut self.voices[slot];
            if !voice.held || voice.key != key {
                continue;
            }
            voice.held = false;
            let Some(bank) = self.bank.as_mut() else { return };
            // Same numerators (so the sound continues smoothly), shorter decay.
            for k in 0..MODES {
                let mut c = self.struck[slot * MODES + k];
                if c.b0 == 0.0 {
                    continue;
                }
                let freq_ratio = c.a1 as f64; // keep the pole angle, shrink the radius
                let r_old = (c.a2 as f64).sqrt();
                let r_new = 10f64.powf(-3.0 / (DAMPED * rate)).min(r_old);
                let cos = -freq_ratio / (2.0 * r_old);
                c.a1 = (-2.0 * r_new * cos) as f32;
                c.a2 = (r_new * r_new) as f32;
                bank.set_lane(slot * MODES + k, &[c]);
            }
        }
    }
}

#[ferritron::plugin]
impl Process for GpuModal {
    fn activate(&mut self, setup: &Setup) {
        self.sample_rate = setup.sample_rate;
        self.inputs = vec![vec![0.0; setup.max_block_size]; VOICES];
        let silent = Biquad { b0: 0.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
        self.struck = vec![silent; VOICES * MODES];
        let lanes: Vec<Lane> =
            (0..VOICES * MODES).map(|l| Lane { input: l / MODES, output: 0, sections: vec![silent] }).collect();
        #[cfg(test)]
        let force_cpu = FORCE_CPU.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let force_cpu = false;
        let policy = if force_cpu { Policy::Off } else { Policy::resolve(Policy::Max) };
        let node = NodeSetup { sample_rate: setup.sample_rate, max_block: setup.max_block_size, offline: setup.offline, policy };
        self.bank = Some(IirNode::new(&lanes, &node));
    }

    fn reset(&mut self) {
        self.voices = Default::default();
    }

    fn latency(&self) -> u32 {
        self.bank.as_ref().map_or(0, IirNode::latency)
    }

    fn process(&mut self, ctx: &mut Context) {
        let n = ctx.io.frames();
        let (decay, brightness) = (ctx.params.decay.value() as f64, ctx.params.brightness.value() as f64 / 100.0);
        // Coefficients change per block; mallets start at their exact sample.
        for note in ctx.notes {
            match note.kind {
                NoteKind::On => self.note_on(note.key, note.velocity, note.offset as usize, decay, brightness),
                NoteKind::Off => self.note_off(note.key),
            }
        }
        for (voice, input) in self.voices.iter_mut().zip(&mut self.inputs) {
            let input = &mut input[..n];
            input.fill(0.0);
            for sample in input.iter_mut().skip(voice.mallet_at) {
                if voice.mallet_left == 0 {
                    break;
                }
                // Raised-cosine window over white noise.
                let phase = (MALLET - voice.mallet_left) as f32 / MALLET as f32;
                let window = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
                voice.seed ^= voice.seed << 13;
                voice.seed ^= voice.seed >> 17;
                voice.seed ^= voice.seed << 5;
                let noise = voice.seed as f32 / u32::MAX as f32 * 2.0 - 1.0;
                *sample = noise * window * voice.velocity;
                voice.mallet_left -= 1;
            }
            voice.mallet_at = 0;
        }

        let Some(bank) = self.bank.as_mut() else { return };
        let inputs: [&[f32]; VOICES] = std::array::from_fn(|v| &self.inputs[v][..n]);
        let volume = db_to_gain(ctx.params.volume.value());
        let out = ctx.io.channel_mut(0);
        bank.process(&inputs, &mut [out]);
        out.iter_mut().for_each(|s| *s *= volume);
        for ch in 1..ctx.io.num_channels() {
            ctx.io.copy_channel(0, ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    const BLOCK: usize = 256;

    /// One test at a time: they share FORCE_CPU, and a real-time GPU test
    /// running next to an offline bounce would find the GPU busy.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Plays one note at real-time pace; returns the output and the latency.
    fn play(blocks: usize, release_at: usize) -> (Vec<f32>, usize) {
        render(Harness::<GpuModal>::new(48_000.0, BLOCK), blocks, release_at, true)
    }

    fn render(mut h: Harness<GpuModal>, blocks: usize, release_at: usize, paced: bool) -> (Vec<f32>, usize) {
        h.set_param("volume", 0.0);
        let latency = h.latency() as usize;
        let mut out = Vec::new();
        for block in 0..blocks {
            if block == 0 {
                h.note_on(10, 57, 0.9); // A3
            }
            if block == release_at {
                h.note_off(0, 57);
            }
            let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
            assert!(h.process(&mut [&mut left, &mut right]));
            assert_eq!(left, right);
            out.extend_from_slice(&left);
            if paced {
                std::thread::sleep(std::time::Duration::from_micros((BLOCK as u64 * 1_000_000) / 48_000));
            }
        }
        (out, latency)
    }

    #[test]
    fn an_offline_bounce_matches_the_cpu_without_latency() {
        let _serial = serial();
        // As fast as possible: real-time GPU processing would leave gaps.
        let (offline, latency) = render(Harness::<GpuModal>::offline(48_000.0, BLOCK), 80, 50, false);
        assert_eq!(latency, 0, "offline rendering adds no latency");
        FORCE_CPU.store(true, std::sync::atomic::Ordering::Relaxed);
        let (cpu, _) = render(Harness::<GpuModal>::new(48_000.0, BLOCK), 80, 50, false);
        FORCE_CPU.store(false, std::sync::atomic::Ordering::Relaxed);
        let peak = cpu.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let worst = offline.iter().zip(&cpu).map(|(g, c)| (g - c).abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.01 && worst < 1e-3 * peak, "offline GPU differs from the CPU by {worst} (peak {peak})");
    }

    fn energy(s: &[f32]) -> f32 {
        s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32
    }

    #[test]
    fn gpu_and_cpu_play_the_same_note() {
        let _serial = serial();
        FORCE_CPU.store(true, std::sync::atomic::Ordering::Relaxed);
        let (cpu, cpu_latency) = play(80, usize::MAX);
        FORCE_CPU.store(false, std::sync::atomic::Ordering::Relaxed);
        let (gpu, latency) = play(80, usize::MAX);
        assert_eq!(cpu_latency, 0);
        assert!(energy(&cpu) > 1e-4, "the note is audible");
        assert!(cpu[..10].iter().all(|&s| s == 0.0) && cpu[10..20].iter().any(|&s| s != 0.0), "starts at its offset");
        if latency == 0 {
            return eprintln!("skipping the GPU comparison: no GPU");
        }
        assert_eq!(latency, 2 * BLOCK, "the GPU bank reports its delay");
        let peak = cpu.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        let worst = gpu[latency..].iter().zip(&cpu).map(|(g, c)| (g - c).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3 * peak, "GPU output differs from the CPU by {worst} (peak {peak})");
    }

    #[test]
    fn releasing_a_note_damps_it_without_a_click() {
        let _serial = serial();
        let (held, latency) = play(120, usize::MAX);
        let (released, _) = play(120, 40);
        let at = 40 * BLOCK + latency;
        let tail = at + 48_00; // 100 ms after the release
        assert!(energy(&released[tail..]) < energy(&held[tail..]) * 0.01, "release damps the note");
        // No click: the release keeps the waveform continuous.
        let jump = released[at - 64..at + 64].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        let normal = held[at - 64..at + 64].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(jump <= normal * 1.5 + 1e-6, "release step {jump} vs. {normal} while held");
    }
}
