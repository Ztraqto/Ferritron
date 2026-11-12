//! A small polyphonic sine synthesizer: the instrument example.
//!
//! It tells hosts it plays 16 voices (`Process::VOICES`), so hosts offer per-note expressions
//! (VST3 Note Expression, CLAP note expressions): tuning bends one note, volume changes its level.

use ferritron::{ExpressionKind, NoteKind, VoiceEvent, VoiceEventKind};
use ferritron::prelude::*;

const VOICES: usize = 16;

#[derive(Clone, Copy, Default)]
struct Voice {
    key: u8,
    /// The host's note ID (-1: none).
    note_id: i32,
    /// The note's frequency without tuning, in cycles per sample.
    base: f32,
    /// The host's per-note volume (linear; 1 = unchanged).
    gain: f32,
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
    fn note_on(&mut self, key: u8, note_id: i32, velocity: f32, sample_rate: f32) {
        // Reuse a free voice, or steal the oldest one.
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            (0..VOICES).min_by_key(|&i| self.voices[i].started).unwrap_or(0)
        });
        let frequency = 440.0 * 2f32.powf((key as f32 - 69.0) / 12.0);
        self.clock += 1;
        self.voices[slot] = Voice {
            key,
            note_id,
            base: frequency / sample_rate,
            gain: 1.0,
            active: true,
            releasing: false,
            phase: 0.0,
            step: frequency / sample_rate,
            level: 0.0,
            velocity,
            started: self.clock,
        };
    }

    /// A host's note expression for the voices it names (by note ID, else by key).
    fn expression(&mut self, e: &VoiceEvent) {
        let VoiceEventKind::Expression { kind, value } = e.kind else { return };
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let named = if e.note_id >= 0 { v.note_id == e.note_id } else { e.key < 0 || e.key as u8 == v.key };
            if !named {
                continue;
            }
            match kind {
                ExpressionKind::Tuning => v.step = v.base * 2f32.powf(value / 12.0),
                ExpressionKind::Volume => v.gain = value.clamp(0.0, 4.0),
                _ => {}
            }
        }
    }

    fn note_off(&mut self, key: u8) {
        for voice in self.voices.iter_mut().filter(|v| v.active && v.key == key) {
            voice.releasing = true;
        }
    }
}

#[ferritron::plugin]
impl Process for SineSynth {
    const VOICES: u32 = VOICES as u32;

    fn reset(&mut self) {
        self.voices = Default::default();
    }

    fn process(&mut self, ctx: &mut Context) {
        let sample_rate = ctx.sample_rate() as f32;
        let volume = db_to_gain(ctx.params().volume.value());
        // Linear ramps: level change per sample.
        let attack = 1.0 / (ctx.params().attack.value() * 0.001 * sample_rate).max(1.0);
        let release = 1.0 / (ctx.params().release.value() * 0.001 * sample_rate).max(1.0);

        let notes = ctx.notes();
        let voice_events = ctx.voice_events();
        let (mut next, mut next_voice) = (0, 0);
        let out = ctx.io().channel_mut(0);
        for (i, sample) in out.iter_mut().enumerate() {
            while let Some(note) = notes.get(next).filter(|n| n.offset as usize == i) {
                match note.kind {
                    NoteKind::On => self.note_on(note.key, note.note_id, note.velocity, sample_rate),
                    NoteKind::Off => self.note_off(note.key),
                }
                next += 1;
            }
            while let Some(e) = voice_events.get(next_voice).filter(|e| e.offset as usize == i) {
                self.expression(e);
                next_voice += 1;
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
                sum += (voice.phase * std::f32::consts::TAU).sin() * voice.level * voice.velocity * voice.gain;
                voice.phase = (voice.phase + voice.step).fract();
            }
            *sample = sum * volume;
        }
        for ch in 1..ctx.io().num_channels() {
            ctx.io().copy_channel(0, ch);
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

    #[test]
    fn a_tuning_expression_bends_one_note() {
        let mut h = Harness::<SineSynth>::new(48_000.0, 48_000);
        h.set_param("volume", 0.0);
        h.set_param("attack", 0.0);
        let (mut l, mut r) = (vec![0.0; 48_000], vec![0.0; 48_000]);
        // Note 7 is A4 (440 Hz); the host tunes it up an octave from the start.
        let events = [
            ferritron::abi::Event::note(0, true, 69, 0, 1.0, 7),
            ferritron::abi::Event::note_expression(0, 7, -1, -1, ExpressionKind::Tuning as u32, 12.0),
        ];
        assert!(h.process_events(&mut [&mut l, &mut r], &events));
        let crossings = l.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((876..=882).contains(&crossings), "~880 Hz, got {crossings} cycles");
    }
}

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<SineSynth>(include_str!("../../../tests/fixtures/1.4.0/sine-synth.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}

/// Bench S3 (`docs/performance.md`): 16 voices held, stereo, 48 kHz, 512 samples.
/// `cargo test --release -p sine-synth bench_s3 -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn bench_s3() {
    let mut h = ferritron::testing::Harness::<SineSynth>::new(48_000.0, 512);
    for k in 0..16u8 {
        h.note_on(0, 48 + k, 0.5);
    }
    let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    let t = ferritron::testing::time_blocks(2000, |_| {
        l.fill(0.0);
        r.fill(0.0);
        assert!(h.process(&mut [&mut l, &mut r]));
    });
    println!("BENCH s3 512 {t}");
}
