//! A delay of `actual` samples that tells the host `reported` samples. With the two equal it is
//! correct; set apart it is the mistake `cargo ferritron check` finds. The tests are the
//! recipe for any plugin: one test that calls `ferritron::testing::check_latency`.

use ferritron::prelude::*;

const RING: usize = 4096;

#[derive(Default)]
struct LatencyCheck {
    rings: [Vec<f32>; 2],
    pos: usize,
    reported: u32,
}

#[ferritron::plugin]
impl Process for LatencyCheck {
    fn activate(&mut self, _setup: &Setup) {
        self.rings = [vec![0.0; RING], vec![0.0; RING]];
        self.pos = 0;
    }

    fn tail(&self) -> Tail {
        Tail::Samples(RING as u32)
    }

    /// What `request_latency` last said, read at every `activate` (the built-in bypass delays
    /// the dry signal by it).
    fn latency(&self) -> u32 {
        self.reported
    }

    fn process(&mut self, ctx: &mut Context) {
        let (reported, actual, mute) = {
            let p = ctx.params();
            (p.reported.value() as u32, p.actual.value() as usize, p.mute.value())
        };
        if reported != self.reported {
            self.reported = reported;
            ctx.request_latency(reported);
        }
        let frames = ctx.io().frames();
        for c in 0..ctx.io().num_channels().min(2) {
            let ring = &mut self.rings[c];
            for (i, s) in ctx.io().channel_mut(c).iter_mut().enumerate() {
                let w = (self.pos + i) & (RING - 1);
                ring[w] = *s;
                *s = if mute { 0.0 } else { ring[(w + RING - actual) & (RING - 1)] };
            }
        }
        self.pos = (self.pos + frames) & (RING - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::{Harness, LatencyMeasurement, check_latency};

    fn set(h: &mut Harness<LatencyCheck>, reported: f64, actual: f64) {
        h.set_param("reported", reported);
        h.set_param("actual", actual);
    }

    /// T5 rework of the bypass: the delayed input and the plugin's output line up. Turning
    /// bypass on changes nothing in the output at any sample, through the crossfade. Turning it
    /// off fades back to a plugin that was not called while bypassed (its own delay still holds
    /// old input), so the output is exact again once the 10 ms fade (480 samples) is over.
    #[test]
    fn bypass_with_latency_is_seamless_through_crossfades() {
        let mut h = Harness::<LatencyCheck>::new(48_000.0, 256);
        set(&mut h, 64.0, 64.0);
        let mut n = 0usize;
        let signal = |i: usize| ((i as f32) * 0.013).sin();
        let mut run = |h: &mut Harness<LatencyCheck>, n: &mut usize| -> Vec<f32> {
            let mut l: Vec<f32> = (0..256).map(|i| signal(*n + i)).collect();
            let mut r = l.clone();
            assert!(h.process(&mut [&mut l, &mut r]));
            *n += 256;
            l
        };
        run(&mut h, &mut n);
        // The host takes the new latency (activates again: the plugin's and the bypass's delays
        // start empty), then lets both fill.
        assert_eq!(h.host_latency(), 64);
        for _ in 0..4 {
            run(&mut h, &mut n);
        }
        for toggle in [true, false, true, false] {
            h.set_bypass(toggle);
            let toggled = n;
            for _ in 0..4 {
                let start = n;
                let out = run(&mut h, &mut n);
                for (i, &o) in out.iter().enumerate() {
                    if !toggle && start + i < toggled + 480 {
                        continue;
                    }
                    let want = signal(start + i - 64);
                    assert!((o - want).abs() < 1e-5, "bypass {toggle}, sample {}: {o} vs {want}", start + i);
                }
            }
        }
    }

    #[test]
    fn latency_matches_when_reported_equals_the_delay() {
        // The recipe: this is the whole test a plugin needs.
        assert_eq!(check_latency::<LatencyCheck>().unwrap(), Some(LatencyMeasurement { measured: 64, reported: 64 }));
        for n in [0.0, 1.0, 511.0, 1500.0] {
            let mut h = Harness::<LatencyCheck>::new(48_000.0, 512);
            set(&mut h, n, n);
            let m = h.measure_latency().unwrap();
            assert_eq!((m.measured, m.reported, m.matches()), (n as u32, n as u32, true), "n = {n}");
        }
    }

    #[test]
    fn latency_check_finds_a_reported_value_that_is_off() {
        let mut h = Harness::<LatencyCheck>::new(48_000.0, 512);
        set(&mut h, 64.0, 71.0);
        let m = h.measure_latency().unwrap();
        assert_eq!((m.measured, m.reported, m.matches()), (71, 64, false));
        // reporting 0 while delaying 300 is the typical mistake (the host does not compensate)
        let mut h = Harness::<LatencyCheck>::new(48_000.0, 512);
        set(&mut h, 0.0, 300.0);
        assert_eq!(h.measure_latency().unwrap().measured, 300);
    }

    #[test]
    fn a_silent_answer_is_not_a_measurement() {
        let mut h = Harness::<LatencyCheck>::new(48_000.0, 512);
        h.set_param("mute", 1.0);
        assert_eq!(h.measure_latency(), None);
    }

    /// The test `cargo ferritron check latency-check` runs. `LATENCY_CHECK_SKEW=7` (used by the
    /// tests of ferritron-cli) makes the plugin delay 7 samples more than it reports; the plain
    /// run is the correct plugin.
    #[test]
    fn latency_check_fixture() {
        match std::env::var("LATENCY_CHECK_SKEW").ok().and_then(|v| v.parse::<f64>().ok()) {
            None => {
                check_latency::<LatencyCheck>().unwrap();
            }
            Some(skew) => {
                let mut h = Harness::<LatencyCheck>::new(48_000.0, 512);
                set(&mut h, 64.0, 64.0 + skew);
                let m = h.measure_latency().unwrap();
                println!("FERRITRON-CHECK latency measured={} reported={}", m.measured, m.reported);
            }
        }
    }
}
