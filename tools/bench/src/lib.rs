//! The benchmark's test plugin: heavy additive voices rendered with
//! `ctx.parallel_for`. Also built as a DLL, to test that plugins in different
//! libraries share one Ferritron Runtime.

use ferritron::prelude::*;

pub const VOICES: usize = 32;
pub const BLOCK: usize = 256;
pub const SAMPLE_RATE: f64 = 48_000.0;

/// One additive voice: `harmonics` sines summed per sample (deliberately heavy).
#[derive(Clone)]
struct Voice {
    phase: f32,
    step: f32,
    out: Vec<f32>,
}

#[derive(Default)]
pub struct Voices {
    voices: Vec<Voice>,
}

#[ferritron::plugin]
impl Process for Voices {
    fn activate(&mut self, setup: &Setup) {
        self.voices = (0..VOICES)
            .map(|v| Voice { phase: 0.0, step: (110.0 + 7.0 * v as f32) / setup.sample_rate as f32, out: vec![0.0; setup.max_block_size] })
            .collect();
    }

    fn process(&mut self, ctx: &mut Context) {
        let frames = ctx.io.frames();
        let harmonics = ctx.params.harmonics.value() as usize;
        ctx.parallel_for(&mut self.voices, |voice| {
            for sample in &mut voice.out[..frames] {
                let mut sum = 0.0;
                for h in 1..=harmonics {
                    sum += (voice.phase * std::f32::consts::TAU * h as f32).sin() / h as f32;
                }
                *sample = sum;
                voice.phase = (voice.phase + voice.step).fract();
            }
        });
        let out = ctx.io.channel_mut(0);
        out.fill(0.0);
        for voice in &self.voices {
            for (o, v) in out.iter_mut().zip(&voice.out) {
                *o += v / VOICES as f32;
            }
        }
    }
}

