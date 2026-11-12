use ferritron::prelude::*;

#[derive(Default)]
struct Gain {
    /// Per-sample gain in dB of one piece (allocated in `activate`).
    ramp: Vec<f32>,
}

#[ferritron::plugin]
impl Process for Gain {
    fn activate(&mut self, setup: &Setup) {
        self.ramp = vec![0.0; setup.max_block_size];
    }

    fn process(&mut self, ctx: &mut Context) {
        let frames = ctx.io().frames();
        // `smooth = "20ms"` in the project file: the dB value glides to the host's value.
        let ramp = &mut self.ramp[..frames];
        if ctx.ramp(&ctx.params().gain, ramp) {
            for c in 0..ctx.io().num_channels() {
                for (s, db) in ctx.io().channel_mut(c).iter_mut().zip(ramp.iter()) {
                    *s *= db_to_gain(*db);
                }
            }
        } else {
            let gain = db_to_gain(ctx.params().gain.value());
            ctx.io().for_each_sample(|s| *s *= gain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    #[test]
    fn applies_gain_automation_and_bypass() {
        let mut h = Harness::<Gain>::new(48_000.0, 512);
        h.set_param("gain", -6.0);
        // 2.0: changes apply at the start of their control period (32 samples).
        h.automate(32, "gain", 0.0);
        let (mut left, mut right) = ([1.0f32; 64], [1.0f32; 64]);
        assert!(h.process(&mut [&mut left, &mut right]));
        assert!((left[0] - 0.501).abs() < 1e-3 && (left[31] - 0.501).abs() < 1e-3, "{left:?}");
        // From the control period at 32 the gain glides up (20 ms) instead of jumping.
        assert!(left[32] > 0.501 && left[32] < 0.6 && left[63] > left[32], "{left:?}");
        assert_eq!(left, right);

        h.set_param("gain", -60.0);
        h.set_bypass(true);
        // Bypass crossfades over 10 ms, then passes the input through untouched.
        let mut buf = [0.25f32; 512];
        for _ in 0..4 {
            buf.fill(0.25);
            assert!(h.process(&mut [&mut buf]));
        }
        assert_eq!(buf, [0.25; 512], "bypass passes the input through");

        let saved = h.save_state();
        let mut other = Harness::<Gain>::new(48_000.0, 512);
        assert!(other.load_state(&saved));
        assert_eq!(other.param("gain"), -60.0);
    }

    /// The harness drives the plugin by index and ID as hosts do (R5).
    #[test]
    fn host_order_flags_modes_and_memory() {
        use ferritron::testing::AutomationMode;
        let mut h = Harness::<Gain>::new(48_000.0, 512);
        // The hosts' order: `gain`, then Bypass and AI Access.
        assert_eq!(h.host_param_count(), 3);
        h.set_param_index(0, -12.0);
        assert_eq!(h.param_index(0), -12.0);
        assert_eq!(h.param("gain"), -12.0);
        assert_eq!(h.text_index(0), h.text("gain"));
        let id = h.host_param(0).unwrap().id;
        h.set_param_id(id, 3.0);
        assert_eq!(h.param_id(id), 3.0);
        assert_ne!(h.flags(1), h.flags(0), "Bypass is flagged as such");
        assert_eq!(h.host_latency(), 0);

        // A bulk change switches at the next block and the smoother jumps.
        h.load_values(&[("gain", -6.0)]);
        let mut buf = [1.0f32; 64];
        h.process(&mut [&mut buf]);
        assert!((buf[0] - 0.501).abs() < 1e-3, "{}", buf[0]);

        // The same automation point, applied per sample instead of per control period.
        h.set_automation_mode(AutomationMode::Sample, 0);
        h.load_values(&[("gain", 0.0)]);
        h.process(&mut [&mut [1.0f32; 64]]);
        let point = h.param_event(5, "gain", -60.0);
        let mut buf = [1.0f32; 64];
        h.process_events(&mut [&mut buf], &[point]);
        assert_eq!(buf[4], 1.0, "unchanged before the point");
        assert!(buf[6] < 1.0 && buf[6] > 0.9, "the glide starts at the point's sample, not at 32: {:?}", &buf[..8]);

        let m = h.memory_report();
        println!("{m:?}, framework {} bytes", m.framework());
        assert!(m.smoothing > 0, "activation allocated the smoother");
        assert_eq!(m.bypass, 0, "no latency, no bypass delay; the crossfade buffer is shared (T5)");
        assert!(m.framework() < 64 * 1024, "{m:?}");
    }

    #[test]
    fn reported_latency_matches_the_delay() {
        // `cargo ferritron check gain` runs this test: a gain has no delay and reports none.
        let m = ferritron::testing::check_latency::<Gain>().unwrap().expect("a gain answers an impulse");
        assert_eq!((m.measured, m.reported), (0, 0));
    }

    #[test]
    fn dense_automation_does_not_zipper() {
        let mut h = Harness::<Gain>::new(48_000.0, 512);
        let (mut l, mut r) = (vec![1.0f32; 512], vec![1.0f32; 512]);
        h.process(&mut [&mut l, &mut r]);
        // Jumps between 0 dB and -40 dB every 64 samples.
        for i in (0..512).step_by(64) {
            h.automate(i as u32, "gain", if (i / 64) % 2 == 0 { -40.0 } else { 0.0 });
        }
        let mut seen = Vec::new();
        for _ in 0..4 {
            l.fill(1.0);
            r.fill(1.0);
            assert!(h.process(&mut [&mut l, &mut r]));
            seen.extend_from_slice(&l);
        }
        let biggest_step = seen.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(biggest_step < 0.01, "a jump of {biggest_step} between neighbouring samples");
    }

    /// An AI client reaches the plugin through the MCP hub, only while the
    /// user's "AI Access" switch is on.
    #[test]
    fn an_ai_client_reaches_the_plugin_through_the_mcp_hub() {
        use ferritron_mcp::Hub;
        use serde_json::json;
        use std::time::{Duration, Instant};

        // A private hub for this test: its own port and token.
        let token_path = std::env::temp_dir().join(format!("ferritron-mcp-test-{}", std::process::id())).join("token");
        let token = ferritron_mcp::ensure_token(&token_path).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // SAFETY: set before any plugin instance's agent reads them.
        unsafe {
            std::env::set_var("FERRITRON_MCP_PORT", port.to_string());
            std::env::set_var("FERRITRON_MCP_TOKEN", &token_path);
        }
        let hub = Hub::start(port, token).unwrap();
        let list = || hub.call_tool("list_plugins", &json!({})).unwrap();
        let mut h = Harness::<Gain>::new(48_000.0, 256);
        let play = |h: &mut Harness<Gain>, blocks: usize| {
            for _ in 0..blocks {
                let (mut l, mut r) = (vec![0.5f32; 256], vec![0.5f32; 256]);
                h.process(&mut [&mut l, &mut r]);
            }
        };
        let wait_for = |want_some: bool| {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                let found = list().as_array().unwrap().iter().any(|p| p["plugin"] == "Ferritron Gain");
                if found == want_some {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        };

        // Off by default: nothing to reach.
        play(&mut h, 10);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!list().as_array().unwrap().iter().any(|p| p["plugin"] == "Ferritron Gain"), "listed while AI access is off");

        h.set_param("ferritron.mcp", 1.0);
        assert!(wait_for(true), "the plugin never reached the hub");
        let entry = list().as_array().unwrap().iter().find(|p| p["plugin"] == "Ferritron Gain").unwrap().clone();
        assert_eq!(entry["daw_pid"], std::process::id());
        let handle = entry["handle"].as_str().unwrap().to_string();

        let params = hub.call_tool("get_parameters", &json!({ "handle": handle })).unwrap();
        let keys: Vec<&str> = params["parameters"].as_array().unwrap().iter().filter_map(|p| p["key"].as_str()).collect();
        assert!(keys.contains(&"gain") && !keys.contains(&"ferritron.mcp"), "the AI cannot see its own switch: {keys:?}");

        let set = hub.call_tool("set_parameter", &json!({ "handle": handle, "parameter": "gain", "value": -6.0 })).unwrap();
        assert_eq!(set["value"], -6.0);
        assert_eq!(h.param("gain"), -6.0);
        assert!(hub.call_tool("set_parameter", &json!({ "handle": handle, "parameter": "ferritron.mcp", "value": 0.0 })).is_err());

        // Audio before and after the plugin: 0.5 in, -6 dB applied.
        play(&mut h, 40);
        let capture = hub.call_tool("capture_audio", &json!({ "handle": handle, "seconds": 0.1 })).unwrap();
        let peak = |side: &str| capture[side]["levels"][0]["peak_db"].as_f64().unwrap();
        assert!((peak("before") - -6.02).abs() < 0.1, "{capture}");
        assert!((peak("after") - -12.04).abs() < 0.1, "{capture}");
        assert!(std::path::Path::new(capture["after"]["wav"].as_str().unwrap()).is_file());

        // Off again: the plugin leaves the hub.
        h.set_param("ferritron.mcp", 0.0);
        assert!(wait_for(false), "still reachable after AI access was turned off");
    }
}

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<Gain>(include_str!("../../../tests/fixtures/1.4.0/gain.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}

/// Bench S5 (`docs/performance.md`): 64 instances in one process (no editors), each one block
/// of stereo at 48 kHz, 512 samples: time for all 64, and the framework's memory per instance.
/// `cargo test --release -p gain bench_s5 -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn bench_s5() {
    use ferritron::testing::Harness;
    let mut all: Vec<Harness<Gain>> = (0..64).map(|_| Harness::<Gain>::new(48_000.0, 512)).collect();
    for (i, h) in all.iter_mut().enumerate() {
        h.set_param("gain", -(i as f64 % 12.0));
    }
    let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    let t = ferritron::testing::time_blocks(500, |b| {
        for h in all.iter_mut() {
            for (i, (x, y)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                let v = (((b * 512 + i) % 97) as f32 / 97.0 - 0.5) * 0.5;
                (*x, *y) = (v, v);
            }
            assert!(h.process(&mut [&mut l, &mut r]));
        }
    });
    println!("BENCH s5 512 {t} (64 instances)");
    println!("BENCH s5 memory {}", s5_memory(&all[0]));
}

#[cfg(test)]
fn s5_memory(h: &ferritron::testing::Harness<Gain>) -> String {
    let m = h.memory_report();
    format!("framework {} bytes per instance ({m:?})", m.framework())
}

/// X8: denormal input costs no more than normal input (FTZ/DAZ on the audio thread).
/// `cargo test --release -p gain denormal_input -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn denormal_input_is_not_slower() {
    let mut h = ferritron::testing::Harness::<Gain>::new(48_000.0, 512);
    h.set_param("gain", -6.0);
    let mut time = |value: f32| {
        let (mut l, mut r) = (vec![value; 512], vec![value; 512]);
        ferritron::testing::time_blocks(5000, |_| {
            l.fill(value);
            r.fill(value);
            h.process(&mut [&mut l, &mut r]);
        })
        .mean_us
    };
    let (normal, denormal) = (time(0.25), time(1.0e-40));
    println!("BENCH denormals: normal {normal:.3} us, denormal {denormal:.3} us ({:.2}x)", denormal / normal);
    assert!(denormal < normal * 1.5, "denormal input {denormal:.3} us vs normal {normal:.3} us");
}
