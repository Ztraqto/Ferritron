//! Look-ahead for limiters and compressors with a fixed reported latency.
//!
//! The audio is always delayed by `max` samples, whatever the current look-ahead is, so
//! the latency a plugin reports to the host never changes when the user turns the
//! look-ahead knob. The detector sees the signal `n <= max` samples before the audio
//! reaches the output: for the audio sample leaving now, [`Lookahead::process`] returns the
//! largest detector value among that sample and the `n` samples after it.

use crate::delay::DelayLine;
use crate::sliding::SlidingMax;

#[derive(Clone, Debug)]
pub struct Lookahead {
    max: usize,
    n: usize,
    audio: DelayLine,
    detector: DelayLine,
    peak: SlidingMax,
}

impl Lookahead {
    /// Allocates for a look-ahead of up to `max` samples (do this in `activate`).
    pub fn new(max: usize) -> Self {
        Lookahead {
            max,
            n: max,
            audio: DelayLine::new(max),
            detector: DelayLine::new(max),
            peak: SlidingMax::new(max + 1),
        }
    }

    /// The latency to report to the host: the largest look-ahead, never less.
    pub fn latency(&self) -> usize {
        self.max
    }

    pub fn lookahead(&self) -> usize {
        self.n
    }

    /// Sets the current look-ahead (clamped to `0..=max`). The reported latency stays `max`.
    /// The detector jumps to the new alignment at once; no allocation.
    pub fn set_lookahead(&mut self, n: usize) {
        self.n = n.min(self.max);
        self.peak.set_window(self.n + 1);
    }

    pub fn clear(&mut self) {
        self.audio.clear();
        self.detector.clear();
        self.peak.reset();
    }

    /// Feeds one audio sample and its (non-negative) detector value. Returns the audio delayed
    /// by `latency()` samples and the largest detector value over the look-ahead window of it.
    #[inline]
    pub fn process(&mut self, audio: f32, level: f32) -> (f32, f32) {
        self.audio.push(audio);
        self.detector.push(level);
        let peak = self.peak.push(self.detector.read(self.max - self.n));
        (self.audio.read(self.max), peak)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_is_delayed_by_max_whatever_the_lookahead() {
        for n in [0usize, 3, 10] {
            let mut l = Lookahead::new(10);
            l.set_lookahead(n);
            assert_eq!(l.latency(), 10);
            let mut out = Vec::new();
            for t in 0..40 {
                let x = if t == 5 { 1.0 } else { 0.0 };
                out.push(l.process(x, x.abs()).0);
            }
            let at = out.iter().position(|&v| v == 1.0).unwrap();
            assert_eq!(at, 15, "n={n}");
        }
    }

    #[test]
    fn detector_sees_the_peak_n_samples_early() {
        let mut l = Lookahead::new(16);
        l.set_lookahead(4);
        let mut peaks = Vec::new();
        for t in 0..60 {
            let x = if t == 20 { 1.0 } else { 0.0 };
            peaks.push(l.process(x, x).1);
        }
        // The impulse leaves the delay at t = 36. The peak must already be seen 4 samples earlier
        // (t = 32..=36) and not before; after the impulse has left, it is gone.
        let first = peaks.iter().position(|&v| v == 1.0).unwrap();
        let last = peaks.iter().rposition(|&v| v == 1.0).unwrap();
        assert_eq!((first, last), (32, 36));
    }

    #[test]
    fn full_lookahead_and_zero_lookahead() {
        let mut l = Lookahead::new(8);
        assert_eq!(l.lookahead(), 8);
        let mut peaks = Vec::new();
        for t in 0..30 {
            peaks.push(l.process(0.0, if t == 10 { 1.0 } else { 0.0 }).1);
        }
        assert_eq!(peaks.iter().position(|&v| v == 1.0), Some(10));
        assert_eq!(peaks.iter().rposition(|&v| v == 1.0), Some(18));
        l.clear();
        l.set_lookahead(0);
        let mut peaks = Vec::new();
        for t in 0..30 {
            peaks.push(l.process(0.0, if t == 10 { 1.0 } else { 0.0 }).1);
        }
        // zero look-ahead: the peak is seen exactly when the audio sample leaves
        assert_eq!(peaks.iter().position(|&v| v == 1.0), Some(18));
        assert_eq!(peaks.iter().rposition(|&v| v == 1.0), Some(18));
        l.set_lookahead(1000);
        assert_eq!(l.lookahead(), 8);
    }
}
