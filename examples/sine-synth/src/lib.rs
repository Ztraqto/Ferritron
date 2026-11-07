//! A small polyphonic sine synthesizer: the instrument example.

use ferritron::NoteKind;
use ferritron::prelude::*;

const VOICES: usize = 16;

#[derive(Clone, Copy, Default)]
struct Voice {
    key: u8,
    active: bool,
    releasing: bool,
    phase: f32,
    step: f32,
    level: f32,
    velocity: f32,
    started: u64,
}

#[derive(Default)]
struct SineSynth {
    voices: [Voice; VOICES],
    clock: u64,
}

impl SineSynth {
    fn note_on(&mut self, key: u8, velocity: f32, sample_rate: f32) {
        // Reuse a free voice, or steal the oldest one.
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            (0..VOICES).min_by_key(|&i| self.voices[i].started).unwrap_or(0)
        });
        let frequency = 440.0 * 2f32.powf((key as f32 - 69.0) / 12.0);
        self.clock += 1;
        self.voices[slot] = Voice {
            key,
            active: true,
            releasing: false,
            phase: 0.0,
            step: frequency / sample_rate,
            level: 0.0,
            velocity,
            started: self.clock,
        };
    }

    fn note_off(&mut self, key: u8) {
        for voice in self.voices.iter_mut().filter(|v| v.active && v.key == key) {
            voice.releasing = true;
        }
    }
}

#[ferritron::plugin]
impl Process for SineSynth {
    fn reset(&mut self) {
        self.voices = Default::default();
    }

    fn process(&mut self, ctx: &mut Context) {
        let sample_rate = ctx.sample_rate as f32;
        let volume = db_to_gain(ctx.params.volume.value());
        // Linear ramps: level change per sample.
        let attack = 1.0 / (ctx.params.attack.value() * 0.001 * sample_rate).max(1.0);
        let release = 1.0 / (ctx.params.release.value() * 0.001 * sample_rate).max(1.0);

        let notes = ctx.notes;
        let mut next = 0;
        let out = ctx.io.channel_mut(0);
        for (i, sample) in out.iter_mut().enumerate() {
            while let Some(note) = notes.get(next).filter(|n| n.offset as usize == i) {
                match note.kind {
                    NoteKind::On => self.note_on(note.key, note.velocity, sample_rate),
                    NoteKind::Off => self.note_off(note.key),
                }
                next += 1;
            }
            let mut sum = 0.0;
            for voice in self.voices.iter_mut().filter(|v| v.active) {
                if voice.releasing {
                    voice.level -= release;
                    if voice.level <= 0.0 {
                        voice.active = false;
                        continue;
                    }
                } else {
                    voice.level = (voice.level + attack).min(1.0);
                }
                sum += (voice.phase * std::f32::consts::TAU).sin() * voice.level * voice.velocity;
                voice.phase = (voice.phase + voice.step).fract();
            }
            *sample = sum * volume;
        }
        for ch in 1..ctx.io.num_channels() {
            ctx.io.copy_channel(0, ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    fn render(h: &mut Harness<SineSynth>, frames: usize) -> Vec<f32> {
        let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
        assert!(h.process(&mut [&mut left, &mut right]));
        assert_eq!(left, right);
        left
    }

    #[test]
    fn plays_the_right_pitch_and_releases() {
        let mut h = Harness::<SineSynth>::new(48_000.0, 48_000);
        h.set_param("volume", 0.0);
        assert!(render(&mut h, 256).iter().all(|&s| s == 0.0), "silent before any note");

        h.note_on(100, 69, 1.0); // A4 = 440 Hz
        let out = render(&mut h, 48_000);
        assert!(out[..100].iter().all(|&s| s == 0.0), "note starts exactly at its offset");
        let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((436..=442).contains(&crossings), "~440 Hz, got {crossings} cycles");
        assert!(out.iter().map(|s| s.abs()).fold(0.0, f32::max) > 0.9);

        h.note_off(0, 69);
        let tail = render(&mut h, 48_000); // default release: 200 ms
        assert!(tail[..100].iter().any(|&s| s != 0.0), "release fades out");
        assert!(tail[20_000..].iter().all(|&s| s == 0.0), "silent after the release");
    }
}
