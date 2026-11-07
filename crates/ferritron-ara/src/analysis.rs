//! Content analysis for ARA: finds the notes of a monophonic source (a voice,
//! a bass line, a lead). A plugin that analyses better overrides
//! [`AraRender::analyze_notes`](crate::AraRender::analyze_notes) and may call
//! [`detect_notes`] for the cases it does not handle itself.

use crate::Source;

/// A note the analysis found; times are in seconds from the start of the source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    /// Average frequency in Hz, or 0 for a sound without a defined pitch.
    pub frequency: f32,
    /// MIDI note number (60 = middle C), or `None` without a defined pitch.
    pub pitch: Option<i32>,
    /// 0 (weak) to 1 (strong), on a scale close to dB.
    pub volume: f32,
    pub start: f64,
    /// Time from `start` to the musical anchor of the note (its attack).
    pub attack: f64,
    /// Time from `start` to the release point.
    pub duration: f64,
    /// Time from `start` to the end of the whole sound.
    pub signal: f64,
}

const WINDOW: usize = 1024;
const INTEGRATION: usize = 512;
const HOP: usize = 256;
const MIN_HZ: f64 = 55.0;
const MAX_HZ: f64 = 1_100.0;
const THRESHOLD: f32 = 0.15;
/// About -50 dBFS.
const SILENCE: f32 = 0.003;
/// Notes shorter than this many frames are noise.
const MIN_FRAMES: usize = 3;
/// A frame further than this from the running pitch (semitones) is another note.
const SPLIT: f64 = 0.8;

/// Monophonic pitch tracking (YIN) and segmentation into notes.
pub fn detect_notes(source: &Source<'_>) -> Vec<Note> {
    if source.is_empty() || source.channels() == 0 || source.sample_rate() <= 0.0 {
        return Vec::new();
    }
    // Mono, boxcar-decimated to about 12 kHz (all the pitch search needs).
    let factor = ((source.sample_rate() / 12_000.0).round() as usize).max(1);
    let rate = source.sample_rate() / factor as f64;
    let scale = 1.0 / (source.channels() * factor) as f32;
    let x: Vec<f32> = (0..source.len() / factor)
        .map(|i| {
            let mut sum = 0.0;
            for c in 0..source.channels() {
                sum += source.channel(c)[i * factor..(i + 1) * factor].iter().sum::<f32>();
            }
            sum * scale
        })
        .collect();

    let tau_min = ((rate / MAX_HZ) as usize).max(2);
    let tau_max = (rate / MIN_HZ) as usize;
    if tau_max + INTEGRATION > WINDOW || x.len() < WINDOW {
        return Vec::new();
    }
    let frames = (x.len() - WINDOW) / HOP + 1;
    let mut difference = vec![0.0f32; tau_max + 1];
    let mut normalized = vec![0.0f32; tau_max + 1];
    // (frequency in Hz or 0, rms)
    let mut track: Vec<(f64, f32)> = Vec::with_capacity(frames);
    for f in 0..frames {
        let frame = &x[f * HOP..f * HOP + WINDOW];
        let rms = (frame[..INTEGRATION].iter().map(|s| s * s).sum::<f32>() / INTEGRATION as f32).sqrt();
        if rms < SILENCE {
            track.push((0.0, rms));
            continue;
        }
        for tau in 1..=tau_max {
            let mut d = 0.0f32;
            for j in 0..INTEGRATION {
                let e = frame[j] - frame[j + tau];
                d += e * e;
            }
            difference[tau] = d;
        }
        let mut running = 0.0f32;
        normalized[0] = 1.0;
        for tau in 1..=tau_max {
            running += difference[tau];
            normalized[tau] = if running > 0.0 { difference[tau] * tau as f32 / running } else { 1.0 };
        }
        // First dip below the threshold, followed down to its minimum.
        let mut found = None;
        let mut tau = tau_min;
        while tau < tau_max {
            if normalized[tau] < THRESHOLD {
                while tau + 1 < tau_max && normalized[tau + 1] < normalized[tau] {
                    tau += 1;
                }
                found = Some(tau);
                break;
            }
            tau += 1;
        }
        let Some(tau) = found else {
            track.push((0.0, rms));
            continue;
        };
        // Parabolic interpolation around the minimum.
        let (a, b, c) = (normalized[tau - 1], normalized[tau], normalized[(tau + 1).min(tau_max)]);
        let denominator = a - 2.0 * b + c;
        let shift = if denominator.abs() > 1e-9 { 0.5 * (a - c) / denominator } else { 0.0 };
        track.push((rate / (tau as f64 + shift as f64), rms));
    }

    let midi = |hz: f64| 69.0 + 12.0 * (hz / 440.0).log2();
    let seconds = |frame: usize| (frame * HOP + WINDOW / 2) as f64 * factor as f64 / source.sample_rate();
    let frame_time = HOP as f64 * factor as f64 / source.sample_rate();
    let mut notes = Vec::new();
    let mut run: Vec<(f64, f32)> = Vec::new();
    let mut run_start = 0;
    // A frame away from the running pitch waits: one is a blip, two start the next note.
    let mut pending: Option<(f64, f32)> = None;
    let flush = |run: &mut Vec<(f64, f32)>, run_start: usize, notes: &mut Vec<Note>| {
        if run.len() >= MIN_FRAMES {
            let mut hz: Vec<f64> = run.iter().map(|r| r.0).collect();
            hz.sort_by(|a, b| a.total_cmp(b));
            let frequency = hz[hz.len() / 2];
            let rms = run.iter().fold(0.0f32, |m, r| m.max(r.1));
            let duration = run.len() as f64 * frame_time;
            notes.push(Note {
                frequency: frequency as f32,
                pitch: Some(midi(frequency).round() as i32),
                volume: ((20.0 * rms.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0),
                start: (seconds(run_start) - frame_time / 2.0).max(0.0),
                attack: 0.0,
                duration,
                signal: duration,
            });
        }
        run.clear();
    };
    for (f, &(hz, rms)) in track.iter().enumerate() {
        if hz <= 0.0 {
            flush(&mut run, run_start, &mut notes);
            pending = None;
        } else if run.is_empty() {
            run_start = f;
            run.push((hz, rms));
            pending = None;
        } else {
            let mean = midi(run.iter().map(|r| r.0).sum::<f64>() / run.len() as f64);
            if (midi(hz) - mean).abs() <= SPLIT {
                run.extend(pending.take());
                run.push((hz, rms));
            } else if let Some(first) = pending.take() {
                flush(&mut run, run_start, &mut notes);
                run_start = f - 1;
                run.push(first);
                run.push((hz, rms));
            } else {
                pending = Some((hz, rms));
            }
        }
    }
    flush(&mut run, run_start, &mut notes);
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(out: &mut Vec<f32>, freq: f64, rate: f64, seconds: f64) {
        let n = (seconds * rate) as usize;
        let start = out.len();
        out.extend((0..n).map(|i| 0.5 * (2.0 * std::f64::consts::PI * freq * (start + i) as f64 / rate).sin() as f32));
    }

    #[test]
    fn finds_the_notes_of_a_melody() {
        let rate = 44_100.0;
        let mut x = Vec::new();
        tone(&mut x, 440.0, rate, 0.5);
        x.extend(std::iter::repeat_n(0.0, (0.2 * rate) as usize));
        tone(&mut x, 659.26, rate, 0.5);
        tone(&mut x, 261.63, rate, 0.5); // follows without a gap
        let ptrs = [x.as_ptr()];
        let source = Source { channels: &ptrs, len: x.len(), sample_rate: rate };
        let notes = detect_notes(&source);
        let pitches: Vec<_> = notes.iter().map(|n| n.pitch).collect();
        assert_eq!(pitches, [Some(69), Some(76), Some(60)], "{notes:?}");
        assert!((notes[0].start - 0.0).abs() < 0.08, "{}", notes[0].start);
        assert!((notes[1].start - 0.7).abs() < 0.08, "{}", notes[1].start);
        assert!((notes[2].start - 1.2).abs() < 0.1, "{}", notes[2].start);
        assert!((notes[0].frequency - 440.0).abs() < 3.0);
        assert!((notes[0].duration - 0.5).abs() < 0.1);
    }

    #[test]
    fn silence_has_no_notes() {
        let x = vec![0.0f32; 50_000];
        let ptrs = [x.as_ptr()];
        let source = Source { channels: &ptrs, len: x.len(), sample_rate: 44_100.0 };
        assert!(detect_notes(&source).is_empty());
    }
}
