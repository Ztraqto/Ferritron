use ferritron::prelude::*;

#[derive(Default)]
struct Gain;

#[ferritron::plugin]
impl Process for Gain {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
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
        h.automate(2, "gain", 0.0);
        let (mut left, mut right) = ([1.0f32; 4], [1.0f32; 4]);
        assert!(h.process(&mut [&mut left, &mut right]));
        assert!((left[0] - 0.501).abs() < 1e-3 && left[2] == 1.0, "{left:?}");
        assert_eq!(left, right);

        h.set_param("gain", -60.0);
        h.set_bypass(true);
        let mut buf = [0.25f32; 4];
        assert!(h.process(&mut [&mut buf]));
        assert_eq!(buf, [0.25; 4], "bypass passes the input through");

        let saved = h.save_state();
        let mut other = Harness::<Gain>::new(48_000.0, 512);
        assert!(other.load_state(&saved));
        assert_eq!(other.param("gain"), -60.0);
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
