//! A test plugin for the Ferritron 1.3.1 features. Every feature gets a few
//! parameters or lines here and a test that drives it through the host path.

use ferritron::prelude::*;
use ferritron::{MidiKind, Speaker};

#[derive(Default)]
struct Multiband {
    latency: u32,
    /// CC 7 (volume) from MIDI, 0..=1.
    volume: f32,
    /// Index of the LFE channel of the main bus, if it has one.
    lfe: Option<usize>,
}

#[ferritron::plugin]
impl Process for Multiband {
    fn activate(&mut self, setup: &Setup) {
        self.lfe = setup.layout.speakers().iter().position(|s| *s == Speaker::Lfe);
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn process(&mut self, ctx: &mut Context) {
        // Linear phase costs 5120 samples; switching modes tells the host (P9).
        let want = if ctx.params().mode.index() == 1 { 5120 } else { 0 };
        if want != self.latency {
            self.latency = want;
            ctx.request_latency(want);
        }
        let mut gain = db_to_gain(ctx.params().input_gain.value() + ctx.params().output_gain.value());
        // Test hook for the host's transport: while the host plays, the gain follows tempo / 120.
        if ctx.transport().playing {
            gain *= (ctx.transport().tempo_or(120.0) / 120.0) as f32;
        }
        for e in ctx.midi() {
            if let MidiKind::Cc { number: 7, value } = e.kind {
                self.volume = f32::from(value) / 127.0;
            }
        }
        let volume = if self.volume > 0.0 { self.volume } else { 1.0 };
        ctx.io().for_each_sample(|s| *s *= gain * volume);
        // Test hook: the sidechain's second channel (the first, when mono) is added to channel 0.
        if let Some(sc) = ctx.sidechain() {
            let mut ch = ctx.io().channels_mut();
            for (o, s) in ch[0].iter_mut().zip(sc.channel(1)) {
                *o += *s;
            }
        }
        // Test hook: the LFE channel is muted, wherever the host puts it.
        if let Some(i) = self.lfe.filter(|i| *i < ctx.io().num_channels()) {
            ctx.io().channel_mut(i).fill(0.0);
        }
    }

    // `level`: below -90 dB is silence ("-INF"); the knob is linear in dB above that,
    // with the bottom 5 % of travel reserved for the silence zone.
    fn format_param_text(param: ParamRef, plain: f64, out: &mut dyn std::fmt::Write) -> bool {
        if param.key != "level" {
            return false;
        }
        let _ = if plain <= -90.0 { out.write_str("-INF dB") } else { write!(out, "{plain:+.2} dB") };
        true
    }

    fn parse_param_text(param: ParamRef, text: &str) -> ParsedText {
        if param.key != "level" {
            return ParsedText::Default;
        }
        let t = text.trim().trim_end_matches("dB").trim();
        if t.eq_ignore_ascii_case("-inf") {
            return ParsedText::Value(-96.0);
        }
        // "2x" is an amplitude ratio.
        if let Some(r) = t.strip_suffix('x').and_then(|n| n.trim().parse::<f64>().ok()).filter(|r| *r > 0.0) {
            return ParsedText::Value(20.0 * r.log10());
        }
        t.parse().map_or(ParsedText::Reject, ParsedText::Value)
    }

    fn param_normalize(_: ParamRef, plain: f64) -> f64 {
        if plain <= -90.0 { 0.0 } else { 0.05 + 0.95 * (plain + 90.0) / 102.0 }
    }

    fn param_denormalize(_: ParamRef, n: f64) -> f64 {
        if n < 0.05 { -96.0 } else { -90.0 + (n - 0.05) / 0.95 * 102.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    #[test]
    fn log_scale_and_custom_mapping_and_text() {
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        // Log: the middle of the knob is the geometric mean of 10 and 30000.
        h.set_normalized("freq", 0.5);
        assert!((h.param("freq") - (10.0f64 * 30000.0).sqrt()).abs() < 1e-6);
        h.set_param("freq", 1000.0);
        assert!((h.normalized("freq") - (100.0f64.ln() / 3000.0f64.ln())).abs() < 1e-9);
        // Round trips through the host path stay inside a tight error.
        for v in [10.0, 20.0, 440.0, 1000.0, 12_345.0, 30_000.0] {
            h.set_param("freq", v);
            let n = h.normalized("freq");
            h.set_normalized("freq", n);
            assert!((h.param("freq") / v - 1.0).abs() < 1e-9, "{v}");
        }
        // Custom text and mapping: "-INF dB" at the bottom, signed dB above.
        h.set_param("level", -96.0);
        assert_eq!((h.text("level"), h.normalized("level")), ("-INF dB".to_string(), 0.0));
        h.set_param("level", 12.0);
        assert_eq!((h.text("level"), h.normalized("level")), ("+12.00 dB".to_string(), 1.0));
        h.set_normalized("level", 0.5);
        assert!((h.param("level") - (-90.0 + 0.45 / 0.95 * 102.0)).abs() < 1e-9);
        // Typed text: accepted forms change the value, rejected ones do not.
        let plain = |h: &Harness<Multiband>, t: &str| h.parse_text("level", t).map(|n| <Params as ferritron::ParamSet>::infos()[4].denormalize(n));
        assert_eq!(plain(&h, "-inf"), Some(-96.0));
        assert!((plain(&h, "2x").unwrap() - 6.0206).abs() < 1e-3);
        assert!((plain(&h, "-6 dB").unwrap() + 6.0).abs() < 1e-6);
        assert_eq!(h.parse_text("level", "banana"), None, "rejected: the value stays");
        // Parameters without `text = "custom"` keep the default display and parsing.
        h.set_param("input_gain", 3.0);
        assert_eq!(h.text("input_gain"), "3.00");
        assert!(h.parse_text("input_gain", "6 dB").is_some());
    }

    #[test]
    fn sidechain_midi_and_wide_layouts() {
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        h.set_param("output_gain", 0.0);
        // Sidechain connected, stereo: channel 1 is added to channel 0.
        let (mut l, mut r) = ([1.0f32; 4], [1.0f32; 4]);
        h.process_with_sidechain(&mut [&mut l, &mut r], &[&[0.0; 4], &[0.5; 4]]);
        assert_eq!(l, [1.5; 4]);
        // Mono sidechain reads as L = R.
        let mut l = [1.0f32; 4];
        h.process_with_sidechain(&mut [&mut l], &[&[0.25; 4]]);
        assert_eq!(l, [1.25; 4]);
        // Not connected: nothing added.
        let mut l = [1.0f32; 4];
        h.process(&mut [&mut l]);
        assert_eq!(l, [1.0; 4]);

        // MIDI in an effect: a CC 7 reaches the plugin and scales the block.
        h.midi(2, 0, MidiKind::Cc { number: 7, value: 64 });
        let mut l = [1.0f32; 4];
        h.process(&mut [&mut l]);
        assert!(l.iter().all(|v| (v - 64.0 / 127.0).abs() < 1e-6), "{l:?}");

        // 5.1 with the speakers a host reports; the plugin mutes the LFE wherever it is.
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        h.set_param("output_gain", 0.0);
        h.set_layout(&Speaker::default_layout(6).speakers().to_vec());
        h.reactivate(48_000.0, 64);
        let mut ch = vec![[1.0f32; 4]; 6];
        let mut refs: Vec<&mut [f32]> = ch.iter_mut().map(|c| &mut c[..]).collect();
        assert!(h.process(&mut refs));
        assert_eq!(ch[3], [0.0; 4], "LFE muted");
        assert_eq!(ch[2], [1.0; 4]);
        // 32 channels go through.
        let mut ch = vec![[1.0f32; 4]; 32];
        let mut refs: Vec<&mut [f32]> = ch.iter_mut().map(|c| &mut c[..]).collect();
        assert!(h.process(&mut refs));
        assert_eq!(ch[31], [1.0; 4]);
    }

    #[test]
    fn a_latency_change_while_running_reaches_the_host() {
        use ferritron::abi::REQUEST_LATENCY;
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        let run = |h: &mut Harness<Multiband>| h.process(&mut [&mut [0.0f32; 16]]);
        run(&mut h);
        assert_eq!((h.latency(), h.take_main_requests()), (0, 0), "nothing asked yet");
        h.set_param("mode", 1.0);
        run(&mut h);
        assert_eq!(h.latency(), 5120, "the host reads the new latency");
        assert_eq!(h.take_main_requests() & REQUEST_LATENCY, REQUEST_LATENCY);
        assert_eq!(h.take_main_requests(), 0, "taking clears the request");
        run(&mut h);
        assert_eq!(h.take_main_requests(), 0, "no change, no request");
        h.set_param("mode", 0.0);
        run(&mut h);
        assert_eq!((h.latency(), h.take_main_requests() & REQUEST_LATENCY), (0, REQUEST_LATENCY));
    }

    #[test]
    fn many_values_switch_together_without_automation() {
        use ferritron::abi::REQUEST_VALUES;
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        h.take_main_requests();
        // A preset: input_gain (0), input_pan (1), output_gain (2), mode (5, after freq and level).
        h.instance().load_values(&[(0, 6.0), (1, 0.5), (2, -3.0), (5, 1.0)]);
        // The host reads the preset at once (audit A1); the audio thread applies it at its next block.
        assert_eq!(h.param("input_gain"), 6.0);
        assert_eq!(h.take_main_requests() & REQUEST_VALUES, REQUEST_VALUES, "the host is told the values changed");
        h.instance().load_values(&[(0, 6.0), (1, 0.5), (2, -3.0), (5, 1.0)]);
        let mut l = [1.0f32; 8];
        h.process(&mut [&mut l]);
        assert_eq!((h.param("input_gain"), h.param("input_pan"), h.param("output_gain"), h.param("mode")), (6.0, 0.5, -3.0, 1.0));
        assert!((l[0] - db_to_gain(3.0)).abs() < 1e-5, "the block ran with the new values: {}", l[0]);
        // Out of range values are clamped like any other write; unknown indexes are ignored.
        h.instance().load_values(&[(0, 1000.0), (99, 1.0)]);
        h.process(&mut [&mut l]);
        assert_eq!(h.param("input_gain"), 36.0);
        // By key.
        h.instance().load_values_by_key(&[("input_gain", -12.0), ("nope", 1.0)]);
        h.process(&mut [&mut l]);
        assert_eq!(h.param("input_gain"), -12.0);
    }

    #[test]
    fn the_host_transport_reaches_the_plugin() {
        let mut h = Harness::<Multiband>::new(48_000.0, 64);
        h.set_param("output_gain", 0.0);
        let run = |h: &mut Harness<Multiband>| {
            let mut l = [1.0f32; 8];
            h.process(&mut [&mut l]);
            l[0]
        };
        // A host that says nothing: no tempo, not playing.
        assert_eq!(run(&mut h), 1.0);
        h.set_transport(ferritron::Transport { tempo: Some(240.0), playing: true, ..Default::default() });
        assert_eq!(run(&mut h), 2.0);
        h.set_transport(ferritron::Transport { tempo: None, playing: true, ..Default::default() });
        assert_eq!(run(&mut h), 1.0, "no tempo: the plugin falls back to 120");
        h.set_transport(ferritron::Transport::default());
        assert_eq!(run(&mut h), 1.0);
    }

    #[test]
    fn explicit_ids_initial_values_and_class_id() {
        let h = Harness::<Multiband>::new(48_000.0, 64);
        // Host order: the plugin's own parameters, then Bypass (reserved ID).
        let ids: Vec<u32> = (0..h.host_param_count()).map(|i| h.host_param(i).unwrap().id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4, 696, 0x4000_0000]);
        // A new instance starts at `initial`; the host's default is still `default`.
        assert_eq!(h.param("output_gain"), -1.0);
        let out = h.host_param(2).unwrap();
        assert_eq!(h.host_param(2).unwrap().id, 2);
        assert_eq!(out.default_normalized, 0.5, "default = 0 dB of -36..36");
        // A saved state wins over `initial`.
        let mut h2 = Harness::<Multiband>::new(48_000.0, 64);
        h2.set_param("output_gain", 6.0);
        let saved = h2.save_state();
        let mut h3 = Harness::<Multiband>::new(48_000.0, 64);
        assert!(h3.load_state(&saved));
        assert_eq!(h3.param("output_gain"), 6.0);
        // The VST3 class ID is the one in the project file.
        // (2.0: on Windows the bytes are in COM order so that hosts show the GUID as written.)
        let first = <Multiband as ferritron::__private::PluginMeta>::INFO.class_id[..4].to_vec();
        assert_eq!(first, if cfg!(windows) { vec![0x2A, 0x4C, 0x1E, 0x6F] } else { vec![0x6F, 0x1E, 0x4C, 0x2A] });
        let info = <Multiband as ferritron::__private::PluginMeta>::INFO;
        assert_eq!(info.vst3_subcategories, "Fx|Distortion|Tools");
        assert_eq!(info.clap_features, "audio-effect,stereo,distortion,multi-effects");
    }
}

/// A minimal LV2 host: loads the plugin through `lv2_descriptor`, connects the ports
/// the Turtle file lists (`lv2_port_plan`) and runs a block with sidechain, MIDI and transport.
#[cfg(test)]
mod lv2_host {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};
    use std::sync::Mutex;

    #[repr(C)]
    struct Feature {
        uri: *const c_char,
        data: *mut c_void,
    }
    #[repr(C)]
    struct Descriptor {
        uri: *const c_char,
        instantiate: unsafe extern "C" fn(*const Descriptor, f64, *const c_char, *const *const Feature) -> *mut c_void,
        connect_port: unsafe extern "C" fn(*mut c_void, u32, *mut c_void),
        activate: Option<unsafe extern "C" fn(*mut c_void)>,
        run: unsafe extern "C" fn(*mut c_void, u32),
        deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
        cleanup: unsafe extern "C" fn(*mut c_void),
        extension_data: unsafe extern "C" fn(*const c_char) -> *const c_void,
    }
    #[repr(C)]
    struct UridMap {
        handle: *mut c_void,
        map: unsafe extern "C" fn(*mut c_void, *const c_char) -> u32,
    }
    unsafe extern "C" {
        fn lv2_descriptor(index: u32) -> *const Descriptor;
    }

    static URIS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn map(_: *mut c_void, uri: *const c_char) -> u32 {
        let uri = unsafe { CStr::from_ptr(uri) }.to_string_lossy().into_owned();
        let mut uris = URIS.lock().unwrap();
        let at = uris.iter().position(|u| *u == uri).unwrap_or_else(|| {
            uris.push(uri);
            uris.len() - 1
        });
        at as u32 + 1
    }
    fn urid(uri: &str) -> u32 {
        let c = std::ffi::CString::new(uri).unwrap();
        unsafe { map(std::ptr::null_mut(), c.as_ptr()) }
    }

    /// An atom sequence holding `events` (frame, type URID, data).
    fn sequence(events: &[(i64, u32, Vec<u8>)]) -> Vec<u64> {
        let mut body: Vec<u8> = Vec::new();
        for (frame, kind, data) in events {
            body.extend(frame.to_le_bytes());
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(kind.to_le_bytes());
            body.extend(data);
            while body.len() % 8 != 0 {
                body.push(0);
            }
        }
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend((8 + body.len() as u32).to_le_bytes()); // atom size: unit + pad + events
        bytes.extend(urid("http://lv2plug.in/ns/ext/atom#Sequence").to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(body);
        bytes.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
    }

    fn property(key: &str, float: f32) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend(urid(key).to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p.extend(4u32.to_le_bytes());
        p.extend(urid("http://lv2plug.in/ns/ext/atom#Float").to_le_bytes());
        p.extend(float.to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p
    }

    #[test]
    fn the_lv2_ports_carry_sidechain_midi_and_transport() {
        let m = ferritron_manifest::parse(include_str!("../multiband-test.ferritroject")).unwrap();
        let plan = m.lv2_ports();
        use ferritron_manifest::Lv2Port::*;
        assert_eq!(plan, [In(0), In(1), Sidechain(0), Sidechain(1), Out(0), Out(1), Midi, Time, Latency, Freewheel, MidiOut]);
        let base = <Params as ferritron::ParamSet>::infos().len() as u32 + 1; // + Bypass

        let desc = unsafe { &*lv2_descriptor(0) };
        let map_feature = UridMap { handle: std::ptr::null_mut(), map };
        let uri = std::ffi::CString::new("http://lv2plug.in/ns/ext/urid#map").unwrap();
        let feature = Feature { uri: uri.as_ptr(), data: (&raw const map_feature).cast_mut().cast() };
        let features = [&feature as *const Feature, std::ptr::null()];
        let handle = unsafe { (desc.instantiate)(desc, 48_000.0, std::ptr::null(), features.as_ptr()) };
        assert!(!handle.is_null());

        // Control ports: plain defaults (output gain 0 dB).
        let mut controls: Vec<f32> = <Params as ferritron::ParamSet>::infos().iter().map(|i| i.default as f32).collect();
        controls.push(0.0); // Bypass
        for (i, c) in controls.iter_mut().enumerate() {
            unsafe { (desc.connect_port)(handle, i as u32, (c as *mut f32).cast()) };
        }
        let frames = 8usize;
        let (mut in_l, mut in_r) = (vec![1.0f32; frames], vec![1.0f32; frames]);
        let (sc_l, sc_r) = (vec![0.0f32; frames], vec![0.25f32; frames]);
        let (mut out_l, mut out_r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        let cc = sequence(&[(0, urid("http://lv2plug.in/ns/ext/midi#MidiEvent"), vec![0xB0, 7, 64])]);
        let mut object = Vec::new();
        object.extend(0u32.to_le_bytes());
        object.extend(urid("http://lv2plug.in/ns/ext/time#Position").to_le_bytes());
        object.extend(property("http://lv2plug.in/ns/ext/time#speed", 1.0));
        object.extend(property("http://lv2plug.in/ns/ext/time#beatsPerMinute", 240.0));
        let time = sequence(&[(0, urid("http://lv2plug.in/ns/ext/atom#Object"), object)]);
        let (mut latency, mut freewheel) = (-1.0f32, 0.0f32);
        let ports: [*mut c_void; 10] = [
            in_l.as_mut_ptr().cast(),
            in_r.as_mut_ptr().cast(),
            sc_l.as_ptr().cast_mut().cast(),
            sc_r.as_ptr().cast_mut().cast(),
            out_l.as_mut_ptr().cast(),
            out_r.as_mut_ptr().cast(),
            cc.as_ptr().cast_mut().cast(),
            time.as_ptr().cast_mut().cast(),
            (&raw mut latency).cast(),
            (&raw mut freewheel).cast(),
        ];
        for (i, p) in ports.iter().enumerate() {
            unsafe { (desc.connect_port)(handle, base + i as u32, *p) };
        }
        unsafe {
            (desc.activate.unwrap())(handle);
            (desc.run)(handle, frames as u32);
        }
        // gain 1 x (tempo 240 / 120) x CC7 (64 / 127), then the sidechain's channel 1 added to channel 0.
        let volume = 64.0 / 127.0;
        assert!((out_l[0] - (2.0 * volume + 0.25)).abs() < 1e-5, "{out_l:?}");
        assert!((out_r[0] - 2.0 * volume).abs() < 1e-5, "{out_r:?}");
        assert_eq!(latency, 0.0, "the latency port was written");
        unsafe { (desc.cleanup)(handle) };
    }
}

#[cfg(all(test, feature = "vst2-test"))]
mod vst2_host;

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<Multiband>(include_str!("../../../tests/fixtures/1.4.0/multiband-test.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}

/// Bench S4 (`docs/performance.md`): the multiband test plugin, stereo, 48 kHz, 512 samples,
/// one parameter automated every 64 samples.
/// `cargo test --release -p multiband-test bench_s4 -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn bench_s4() {
    let mut h = ferritron::testing::Harness::<Multiband>::new(48_000.0, 512);
    let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    let t = ferritron::testing::time_blocks(2000, |b| {
        for (i, (x, y)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let v = (((b * 512 + i) % 97) as f32 / 97.0 - 0.5) * 0.5;
            (*x, *y) = (v, v);
        }
        for i in (0..512u32).step_by(64) {
            h.automate(i, "input_gain", ((b as u32 * 8 + i / 64) % 24) as f64 - 12.0);
        }
        assert!(h.process(&mut [&mut l, &mut r]));
    });
    println!("BENCH s4 512 {t}");
}
