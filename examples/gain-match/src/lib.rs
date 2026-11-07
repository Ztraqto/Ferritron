//! Gain match over Ferritron DirectLink (design doc, Phase 7): put one
//! instance before an effect ("Before") and one after it ("After"), in the
//! same group. "Before" publishes the level it measures; "After" brings the
//! effect's output back to that level, so the effect can be judged without
//! a loudness change fooling the ear.
//!
//! The pair meets on the DirectLink channel of its group, which works
//! across plugin libraries and processes. Mode and group are parameters, so
//! after reopening the project the two find each other again by themselves.

use ferritron::directlink::{DirectLink, Port};
use ferritron::prelude::*;

const GROUPS: usize = 16;
const NAMESPACE: &str = "dev.ferritron.examples.gain-match";
/// Level averaging time, seconds.
const AVERAGE: f32 = 0.3;
/// A reference older than this is gone (its plugin stopped or was removed).
const STALE: std::time::Duration = std::time::Duration::from_millis(500);
/// The correction stays within ±24 dB.
const LIMIT: f32 = 15.85;

#[derive(Default)]
struct GainMatch {
    /// The "level" port of each group's channel (None if shared memory failed).
    ports: Vec<Option<Port>>,
    /// Mean square of the input, averaged.
    level: f32,
    gain: f32,
    sample_rate: f32,
}

#[ferritron::plugin]
impl Process for GainMatch {
    fn activate(&mut self, setup: &Setup) {
        // Every group's channel is opened here, so switching groups while
        // playing needs no allocation.
        self.ports =
            (1..=GROUPS).map(|g| DirectLink::open(NAMESPACE, &format!("group {g}")).ok().and_then(|link| link.port("level"))).collect();
        self.sample_rate = setup.sample_rate as f32;
        self.reset();
    }

    fn reset(&mut self) {
        self.level = 0.0;
        self.gain = 1.0;
    }

    fn process(&mut self, ctx: &mut Context) {
        let n = ctx.io.frames();
        if n == 0 {
            return;
        }
        let channels = ctx.io.num_channels();
        let mut sum = 0.0;
        for ch in 0..channels {
            sum += ctx.io.channel_mut(ch).iter().map(|s| s * s).sum::<f32>();
        }
        let ms = sum / (n * channels.max(1)) as f32;
        let a = 1.0 - (-(n as f32) / (AVERAGE * self.sample_rate)).exp();
        self.level += (ms - self.level) * a;

        let group = (ctx.params.group.value() as usize).clamp(1, GROUPS) - 1;
        let Some(Some(port)) = self.ports.get(group) else { return };
        if ctx.params.mode.index() == 0 {
            // Before: tell the group.
            port.publish(&[self.level]);
            return;
        }
        // After: match the group's level, if there is a fresh one.
        let mut reference = [0.0f32];
        let target = match port.read(&mut reference) {
            Some(r) if r.age < STALE && r.len == 1 && self.level > 1e-12 && reference[0] > 1e-12 => {
                (reference[0] / self.level).sqrt().clamp(1.0 / LIMIT, LIMIT)
            }
            _ => 1.0,
        };
        let step = 1.0 - (-1.0 / (0.05 * self.sample_rate)).exp();
        for i in 0..n {
            self.gain += (target - self.gain) * step;
            for ch in 0..channels {
                ctx.io.channel_mut(ch)[i] *= self.gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    fn sine(amplitude: f32, start: usize) -> Vec<f32> {
        (start..start + 256).map(|i| amplitude * (i as f32 * 0.05).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Runs a Before/After pair; returns the After output's RMS at the end.
    fn pair(before_group: f64, after: &mut Harness<GainMatch>, after_input: f32) -> f32 {
        let mut before = Harness::<GainMatch>::new(48_000.0, 256);
        before.set_param("mode", 0.0);
        before.set_param("group", before_group);
        let mut last = 0.0;
        for b in 0..600 {
            let mut pre = sine(0.5, b * 256);
            before.process(&mut [&mut pre]);
            let mut post = sine(after_input, b * 256);
            after.process(&mut [&mut post]);
            last = rms(&post);
        }
        last
    }

    #[test]
    fn the_after_instance_matches_the_level_before_the_effect() {
        let mut after = Harness::<GainMatch>::new(48_000.0, 256);
        after.set_param("mode", 1.0);
        after.set_param("group", 11.0);
        // The "effect" lost 12 dB; gain match brings it back.
        let out = pair(11.0, &mut after, 0.125);
        let target = rms(&sine(0.5, 0));
        assert!((20.0 * (out / target).log10()).abs() < 0.2, "{out} vs {target}");
    }

    #[test]
    fn another_group_is_not_matched() {
        let mut after = Harness::<GainMatch>::new(48_000.0, 256);
        after.set_param("mode", 1.0);
        after.set_param("group", 13.0);
        let out = pair(12.0, &mut after, 0.125);
        // Unchanged: the same block of input (the last one).
        assert!((out - rms(&sine(0.125, 599 * 256))).abs() < 1e-4, "passes through without a partner: {out}");
    }

    #[test]
    fn a_reopened_project_finds_its_partner_again() {
        let mut after = Harness::<GainMatch>::new(48_000.0, 256);
        after.set_param("mode", 1.0);
        after.set_param("group", 14.0);
        let state = after.save_state();
        // Reopen: a new instance from the saved state.
        let mut reopened = Harness::<GainMatch>::new(48_000.0, 256);
        assert!(reopened.load_state(&state));
        let out = pair(14.0, &mut reopened, 0.125);
        assert!((20.0 * (out / rms(&sine(0.5, 0))).log10()).abs() < 0.2);
    }
}
