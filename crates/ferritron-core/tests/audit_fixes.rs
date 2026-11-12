//! Tests for the fixes of the 2.0.0 audit (docs/issues-2.0.0*.md). Each test names the item it covers.
//! The plugin below records what `process` saw; tests read it back with `take_calls`.

use ferritron_core::__private::PluginMeta;
use ferritron_core::abi::Event;
use ferritron_core::testing::{AutomationMode, Harness};
use ferritron_core::*;
use std::cell::RefCell;

struct Params {
    amount: FloatParam,
    smooth: FloatParam,
}

static INFOS: [ParamInfo; 2] = [
    ParamInfo { id: 7, key: "amount", name: "Amount", unit: "", kind: ParamKind::Float { min: 0.0, max: 4.0 }, default: 2.0, scale: ParamScale::Linear, text: None },
    ParamInfo { id: 8, key: "smooth", name: "Smooth", unit: "", kind: ParamKind::Float { min: 0.0, max: 4.0 }, default: 1.0, scale: ParamScale::Linear, text: None },
];

impl Default for Params {
    fn default() -> Self {
        Self { amount: FloatParam::new(2.0), smooth: FloatParam::new(1.0) }
    }
}

impl ParamSet for Params {
    fn infos() -> &'static [ParamInfo] {
        &INFOS
    }
    fn cell(&self, index: usize) -> &ParamCell {
        if index == 0 { self.amount.cell() } else { self.smooth.cell() }
    }
    fn index_of(id: u32) -> Option<usize> {
        match id {
            7 => Some(0),
            8 => Some(1),
            _ => None,
        }
    }
    /// `smooth` has a 20 ms linear ramp; `amount` none.
    fn smoothing(index: usize) -> Option<Smoothing> {
        (index == 1).then_some(Smoothing { seconds: 0.02, kind: 0 })
    }
}

/// What one `process` call saw.
#[derive(Clone, Debug)]
struct Call {
    frames: usize,
    offset: u32,
    notes: Vec<(u32, NoteKind, u8)>,
    position: Option<i64>,
    beats: Option<f64>,
    amount: f32,
    smooth: f32,
    smooth_changed: bool,
}

thread_local! {
    // Per test thread, so tests running in parallel do not see each other's calls.
    static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
}

fn take_calls() -> Vec<Call> {
    CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

#[derive(Default)]
struct Probe;

impl Process for Probe {
    type Params = Params;
    fn process(&mut self, ctx: &mut Context<'_, Params>) {
        let call = Call {
            frames: ctx.io().frames(),
            offset: ctx.block_offset(),
            notes: ctx.notes().iter().map(|n| (n.offset, n.kind, n.key)).collect(),
            position: ctx.transport().position_samples,
            beats: ctx.transport().position_beats,
            amount: ctx.params().amount.value(),
            smooth: ctx.params().smooth.value(),
            smooth_changed: ctx.changed(&ctx.params().smooth),
        };
        CALLS.with(|c| c.borrow_mut().push(call));
        // The sidechain goes to the output, so a test can see that it was processed.
        if let Some(sc) = ctx.sidechain() {
            let src: Vec<f32> = sc.channel(0).to_vec();
            ctx.io().channel_mut(0).copy_from_slice(&src);
        }
    }
}

const INFO: PluginInfo = PluginInfo {
    id: "test.audit-fixes",
    name: "Audit Probe",
    vendor: "",
    version: "1",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [0; 16],
    isolation: isolation::Isolation::Guarded,
    process: isolation::ProcessOptions::DEFAULT,
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig { min_channels: 1, max_channels: 2, sidechain: Some(1), midi_input: true, passthrough: false, host_cc: false },
};

impl PluginMeta for Probe {
    const INFO: &'static PluginInfo = &INFO;
    fn localized() -> &'static i18n::Localized {
        static L: std::sync::OnceLock<i18n::Localized> = std::sync::OnceLock::new();
        L.get_or_init(|| i18n::Localized::new(INFO.id, Params::infos(), &[]))
    }
}

/// One mono block of `frames` samples of `value`; returns the output.
fn block(h: &mut Harness<Probe>, frames: usize, value: f32) -> Vec<f32> {
    let mut buf = vec![value; frames];
    assert!(h.process(&mut [&mut buf]), "process failed");
    buf
}

/// Puts the instance to sleep (Tail::Auto: 2 s of silence).
fn put_to_sleep(h: &mut Harness<Probe>) {
    for _ in 0..300 {
        block(h, 512, 0.0);
    }
    assert!(h.sleeping(), "the instance should sleep after 3 s of silence");
    take_calls();
}

/// A4: in "sample" mode, a parameter change at offset == frames must not break the block,
/// a note at that offset arrives on the last sample, and the change applies after the block.
#[test]
fn a4_sample_mode_events_at_the_end_of_the_block() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    h.set_automation_mode(AutomationMode::Sample, 0);
    let events = [h.param_event(8, "amount", 1.0), Event::note(8, true, 60, 0, 1.0, -1)];
    let mut buf = [0.5f32; 8];
    assert!(h.process_events(&mut [&mut buf], &events), "the block must not fail");
    let calls = take_calls();
    let notes: Vec<_> = calls.iter().flat_map(|c| c.notes.clone()).collect();
    assert_eq!(notes, vec![(7, NoteKind::On, 60)], "a note at offset == frames goes to the last sample");
    assert!(calls.iter().all(|c| c.amount == 2.0), "a change at offset == frames applies after the block");
    block(&mut h, 8, 0.5);
    assert_eq!(take_calls()[0].amount, 1.0);
}

/// A2: a sleeping instance wakes up when the sidechain has sound.
#[test]
fn a2_sidechain_wakes_a_sleeping_instance() {
    let mut h = Harness::<Probe>::new(48_000.0, 512);
    put_to_sleep(&mut h);
    let sidechain = [0.8f32; 512];
    let mut buf = [0.0f32; 512];
    assert!(h.process_with_sidechain(&mut [&mut buf], &[&sidechain]));
    assert_eq!(take_calls().len(), 1, "process must run for a loud sidechain");
    assert_eq!(buf[0], 0.8);
}

/// A3: when a block is split, each piece sees the transport at its own first sample.
#[test]
fn a3_transport_advances_with_each_piece() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    h.set_automation_mode(AutomationMode::Sample, 0);
    h.set_transport(Transport { tempo: Some(120.0), playing: true, position_samples: Some(1000), position_beats: Some(4.0), ..Default::default() });
    let events = [h.param_event(32, "amount", 1.0)];
    let mut buf = [0.5f32; 64];
    assert!(h.process_events(&mut [&mut buf], &events));
    let calls = take_calls();
    assert_eq!(calls.iter().map(|c| c.frames).collect::<Vec<_>>(), vec![32, 32]);
    assert_eq!(calls[0].position, Some(1000));
    assert_eq!(calls[1].position, Some(1032));
    assert_eq!(calls.iter().map(|c| c.offset).collect::<Vec<_>>(), vec![0, 32]);
    // 32 samples at 48 kHz and 120 BPM (2 beats per second).
    assert!((calls[1].beats.unwrap() - (4.0 + 32.0 / 24_000.0)).abs() < 1e-9, "{:?}", calls[1].beats);
}

/// A3: while the host is stopped, the position does not move inside the block.
#[test]
fn a3_transport_stays_while_stopped() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    h.set_automation_mode(AutomationMode::Sample, 0);
    h.set_transport(Transport { tempo: Some(120.0), playing: false, position_samples: Some(1000), position_beats: Some(4.0), ..Default::default() });
    let events = [h.param_event(32, "amount", 1.0)];
    let mut buf = [0.5f32; 64];
    assert!(h.process_events(&mut [&mut buf], &events));
    let calls = take_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.position == Some(1000) && c.beats == Some(4.0)));
}

/// A1: values from `load_values` (a preset) are what the host reads and saves at once,
/// before the next `process`.
#[test]
fn a1_loaded_values_are_visible_before_the_next_block() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    h.load_values(&[("amount", 3.0)]);
    assert_eq!(h.param("amount"), 3.0, "get_param");
    assert_eq!(h.instance().value("amount"), Some(3.0), "InstanceHandle::value");
    let state = h.save_state();
    let mut other = Harness::<Probe>::new(48_000.0, 64);
    assert!(other.load_state(&state));
    assert_eq!(other.param("amount"), 3.0, "a state saved before the next block holds the preset");
    block(&mut h, 64, 0.5);
    assert_eq!(h.param("amount"), 3.0);
    assert_eq!(take_calls()[0].amount, 3.0);
}

/// B5: `ctx.changed` is true for every piece whose smoothed value differs from the previous
/// piece's, and false once the value stands still.
#[test]
fn b5_changed_follows_the_smoothed_value() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    block(&mut h, 64, 0.5);
    take_calls();
    h.set_param("smooth", 3.0);
    for _ in 0..30 {
        block(&mut h, 64, 0.5); // 1920 samples: well past the 960-sample ramp
    }
    let calls = take_calls();
    assert!(calls[0].smooth_changed, "the first piece after the change");
    assert!(calls.iter().filter(|c| c.smooth != calls[0].smooth).count() > 2, "the value should move over several pieces");
    for pair in calls.windows(2) {
        if pair[1].smooth != pair[0].smooth {
            assert!(pair[1].smooth_changed, "value moved {} -> {} but changed() was false", pair[0].smooth, pair[1].smooth);
        }
    }
    let n = calls.len();
    assert_eq!(calls[n - 1].smooth, 3.0);
    assert_eq!(calls[n - 2].smooth, 3.0);
    assert!(!calls[n - 1].smooth_changed, "nothing moved since the previous piece");
}

/// B11: loading a state that does not hold a parameter (saved before it existed) gives that
/// parameter its default.
#[test]
fn b11_a_parameter_missing_from_the_state_gets_its_default() {
    let h = Harness::<Probe>::new(48_000.0, 64);
    let full = h.save_state();
    // Header: "FRTN", version, count (12 bytes); then 12-byte entries (id u32, value f64).
    let count = u32::from_le_bytes(full[8..12].try_into().unwrap()) as usize;
    let mut old = full[..12].to_vec();
    let mut kept = 0u32;
    for entry in full[12..12 + count * 12].chunks(12) {
        if u32::from_le_bytes(entry[..4].try_into().unwrap()) != 8 {
            old.extend_from_slice(entry);
            kept += 1;
        }
    }
    old[8..12].copy_from_slice(&kept.to_le_bytes());
    let mut other = Harness::<Probe>::new(48_000.0, 64);
    other.set_param("smooth", 3.5);
    assert!(other.load_state(&old));
    assert_eq!(other.param("smooth"), 1.0);
}

/// C9: a sleeping instance that gets the same value again does not count a parameter move.
#[test]
fn c9_resent_values_while_asleep_move_nothing() {
    let mut h = Harness::<Probe>::new(48_000.0, 512);
    put_to_sleep(&mut h);
    let before = h.instance().params_revision();
    for _ in 0..10 {
        let events = [h.param_event(0, "amount", 2.0)]; // the value it already has
        let mut buf = [0.0f32; 512];
        assert!(h.process_events(&mut [&mut buf], &events));
    }
    assert_eq!(h.instance().params_revision(), before);
}

/// F1 / A5: `reset` clears the smoothing, so a parameter in the middle of its ramp jumps to its target.
#[test]
fn reset_clears_the_smoothing() {
    let mut h = Harness::<Probe>::new(48_000.0, 64);
    block(&mut h, 64, 0.5);
    take_calls();
    h.set_param("smooth", 3.0);
    block(&mut h, 64, 0.5);
    assert!(take_calls()[0].smooth < 3.0, "the value is still ramping");
    h.reset();
    block(&mut h, 64, 0.5);
    assert_eq!(take_calls()[0].smooth, 3.0);
}

/// A1: a panic in a plugin's own `scale = "custom"` (here `denormalize`) must not unwind into
/// the host. A sleeping instance gets a parameter event: the wake-up check and the apply both
/// convert the value, and both keep the old value instead of panicking.
struct ScaledParams {
    warp: FloatParam,
}

fn keep_value(v: f64) -> f64 {
    v
}

fn explode(_: f64) -> f64 {
    panic!("custom scale: denormalize failed")
}

static SCALED_INFOS: [ParamInfo; 1] = [ParamInfo {
    id: 9,
    key: "warp",
    name: "Warp",
    unit: "",
    kind: ParamKind::Float { min: 0.0, max: 1.0 },
    default: 0.5,
    scale: ParamScale::Custom { normalize: keep_value, denormalize: explode },
    text: None,
}];

impl Default for ScaledParams {
    fn default() -> Self {
        Self { warp: FloatParam::new(0.5) }
    }
}

impl ParamSet for ScaledParams {
    fn infos() -> &'static [ParamInfo] {
        &SCALED_INFOS
    }
    fn cell(&self, _index: usize) -> &ParamCell {
        self.warp.cell()
    }
    fn index_of(id: u32) -> Option<usize> {
        (id == 9).then_some(0)
    }
}

#[derive(Default)]
struct Scaled;

impl Process for Scaled {
    type Params = ScaledParams;
    fn process(&mut self, _ctx: &mut Context<'_, ScaledParams>) {}
}

const SCALED_INFO: PluginInfo = PluginInfo {
    id: "test.audit-scaled",
    name: "Audit Scaled",
    vendor: "",
    version: "1",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [0; 16],
    isolation: isolation::Isolation::Guarded,
    process: isolation::ProcessOptions::DEFAULT,
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig { min_channels: 1, max_channels: 2, sidechain: None, midi_input: false, passthrough: false, host_cc: false },
};

impl PluginMeta for Scaled {
    const INFO: &'static PluginInfo = &SCALED_INFO;
    fn localized() -> &'static i18n::Localized {
        static L: std::sync::OnceLock<i18n::Localized> = std::sync::OnceLock::new();
        L.get_or_init(|| i18n::Localized::new(SCALED_INFO.id, ScaledParams::infos(), &[]))
    }
}

#[test]
fn a1_a_panicking_custom_scale_stays_inside_the_plugin_call() {
    let mut h = Harness::<Scaled>::new(48_000.0, 512);
    for _ in 0..300 {
        let mut buf = [0.0f32; 512];
        assert!(h.process(&mut [&mut buf]));
    }
    assert!(h.sleeping(), "the instance should sleep after 3 s of silence");
    let events = [Event::param(0, 9, 0.8)];
    let mut buf = [0.0f32; 512];
    assert!(h.process_events(&mut [&mut buf], &events), "process must return normally");
}

thread_local! {
    // Per test thread, like CALLS.
    static RESETS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A plugin that counts its `reset` calls.
#[derive(Default)]
struct Resetting;

impl Process for Resetting {
    type Params = Params;
    fn process(&mut self, _ctx: &mut Context<'_, Params>) {}
    fn reset(&mut self) {
        RESETS.with(|r| r.set(r.get() + 1));
    }
}

const RESET_INFO: PluginInfo = PluginInfo {
    id: "test.audit-reset",
    name: "Audit Reset",
    vendor: "",
    version: "1",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [0; 16],
    isolation: isolation::Isolation::Guarded,
    process: isolation::ProcessOptions::DEFAULT,
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig { min_channels: 1, max_channels: 2, sidechain: None, midi_input: true, passthrough: false, host_cc: false },
};

impl PluginMeta for Resetting {
    const INFO: &'static PluginInfo = &RESET_INFO;
    fn localized() -> &'static i18n::Localized {
        static L: std::sync::OnceLock<i18n::Localized> = std::sync::OnceLock::new();
        L.get_or_init(|| i18n::Localized::new(RESET_INFO.id, Params::infos(), &[]))
    }
}

/// B2: a note held across a fully bypassed span does not come back after the bypass is
/// turned off: the plugin is reset before it runs again.
#[test]
fn b2_unbypass_resets_the_plugin() {
    let mut h = Harness::<Resetting>::new(48_000.0, 512);
    h.note_on(0, 60, 1.0);
    let mut buf = [0.5f32; 512];
    assert!(h.process(&mut [&mut buf]));
    h.set_bypass(true);
    for _ in 0..4 {
        let mut buf = [0.5f32; 512];
        assert!(h.process(&mut [&mut buf]));
    }
    h.note_off(0, 60); // the plugin is not called while bypassed, so this note-off is not heard
    let mut buf = [0.5f32; 512];
    assert!(h.process(&mut [&mut buf]));
    let before = RESETS.with(|r| r.get());
    h.set_bypass(false);
    let mut buf = [0.5f32; 512];
    assert!(h.process(&mut [&mut buf]));
    assert!(RESETS.with(|r| r.get()) > before, "the plugin must be reset when the bypass is turned off");
}

/// C4: All Notes Off (CC 123) releases the notes held on its channel, so an instrument whose
/// note-off never came can sleep again.
#[test]
fn c4_all_notes_off_lets_the_plugin_sleep() {
    let mut h = Harness::<Probe>::new(48_000.0, 512);
    h.note_on(0, 60, 1.0);
    block(&mut h, 512, 0.5);
    h.midi(0, 0, MidiKind::Cc { number: 123, value: 0 });
    block(&mut h, 512, 0.0);
    for _ in 0..300 {
        block(&mut h, 512, 0.0);
    }
    assert!(h.sleeping(), "All Notes Off must release the held note so the instance can sleep");
}

/// A plugin with voices that leaves the voice end to Ferritron.
#[derive(Default)]
struct Voiced;

impl Process for Voiced {
    type Params = Params;
    const VOICES: u32 = 4;
    fn process(&mut self, _ctx: &mut Context<'_, Params>) {}
}

const VOICED_INFO: PluginInfo = PluginInfo {
    id: "test.audit-voiced",
    name: "Audit Voiced",
    vendor: "",
    version: "1",
    url: "",
    email: "",
    kind: PluginKind::Instrument,
    class_id: [0; 16],
    isolation: isolation::Isolation::Guarded,
    process: isolation::ProcessOptions::DEFAULT,
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig { min_channels: 1, max_channels: 2, sidechain: None, midi_input: true, passthrough: false, host_cc: false },
};

impl PluginMeta for Voiced {
    const INFO: &'static PluginInfo = &VOICED_INFO;
    fn localized() -> &'static i18n::Localized {
        static L: std::sync::OnceLock<i18n::Localized> = std::sync::OnceLock::new();
        L.get_or_init(|| i18n::Localized::new(VOICED_INFO.id, Params::infos(), &[]))
    }
}

/// A5: a released voice keeps its modulation until `end`, and `end` drops it.
#[test]
fn a5_a_released_voice_keeps_its_offsets_until_end() {
    let mut m = VoiceMods::new();
    let v = VoiceId::new(0, 60);
    m.note_on(v);
    assert!(m.set(v, 2, 0.5));
    m.note_off(v);
    assert!(!m.is_active(v));
    assert_eq!(m.active_voices(), 0);
    assert_eq!(m.amount(v, 2), 0.5, "the release tail keeps its modulation");
    m.end(v);
    assert_eq!(m.amount(v, 2), 0.0);
}

/// A5: released voices give their slots to new notes; a full table of held notes still refuses.
#[test]
fn a5_released_voices_are_reused() {
    let mut m = VoiceMods::new();
    for k in 0..ferritron_core::voice::MAX_VOICES as u8 {
        assert!(m.note_on(VoiceId::new(0, k)));
    }
    assert!(!m.note_on(VoiceId::new(1, 0)), "all voices held");
    for k in 0..ferritron_core::voice::MAX_VOICES as u8 {
        m.note_off(VoiceId::new(0, k));
    }
    assert_eq!(m.active_voices(), 0);
    assert!(m.note_on(VoiceId::new(1, 0)), "a released voice gives up its slot");
}

/// A5: a host's per-note modulation that names only a note ID reaches that note, not the
/// voices that have no note ID.
#[test]
fn a5_a_note_id_alone_reaches_only_its_note() {
    let mut m = VoiceMods::new();
    let (with_id, without_id) = (VoiceId::new(0, 60), VoiceId::new(0, 62));
    m.feed_note(&NoteEvent { offset: 0, kind: NoteKind::On, key: 60, channel: 0, velocity: 1.0, note_id: 5 });
    m.feed_note(&NoteEvent { offset: 0, kind: NoteKind::On, key: 62, channel: 0, velocity: 1.0, note_id: -1 });
    m.feed_voice(&VoiceEvent { offset: 0, note_id: 5, key: -1, channel: -1, kind: VoiceEventKind::Mod { index: 2, amount: 0.25 } });
    assert_eq!(m.amount(with_id, 2), 0.25);
    assert_eq!(m.amount(without_id, 2), 0.0);
}

/// A5 / D4: with `REPORTS_VOICE_END = false`, a note-off makes Ferritron report the end of the
/// voice (one `EVENT_NOTE_END` at the note-off, with its note ID, channel and key).
#[test]
fn a5_a_note_off_reports_the_end_of_its_voice() {
    let mut h = Harness::<Voiced>::new(48_000.0, 512);
    h.note_on(3, 60, 1.0);
    let mut buf = [0.0f32; 512];
    assert!(h.process(&mut [&mut buf]));
    assert!(h.voice_ends().is_empty(), "no voice has ended yet");
    h.note_off(5, 60);
    let mut buf = [0.0f32; 512];
    assert!(h.process(&mut [&mut buf]));
    assert_eq!(h.voice_ends(), vec![(5, -1, 0, 60)]);
}

/// A plugin that sends one MIDI message at offset 0 of every piece it is given.
#[derive(Default)]
struct MidiOutProbe;

impl Process for MidiOutProbe {
    type Params = Params;
    fn process(&mut self, ctx: &mut Context<'_, Params>) {
        ctx.midi_out(0, 0, MidiKind::Cc { number: 1, value: 1 });
    }
}

const MIDI_OUT_INFO: PluginInfo = PluginInfo {
    id: "test.audit-midi-out",
    name: "Audit MIDI Out",
    vendor: "",
    version: "1",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [0; 16],
    isolation: isolation::Isolation::Guarded,
    process: isolation::ProcessOptions::DEFAULT,
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig { min_channels: 1, max_channels: 2, sidechain: None, midi_input: true, passthrough: false, host_cc: false },
};

impl PluginMeta for MidiOutProbe {
    const INFO: &'static PluginInfo = &MIDI_OUT_INFO;
    fn localized() -> &'static i18n::Localized {
        static L: std::sync::OnceLock<i18n::Localized> = std::sync::OnceLock::new();
        L.get_or_init(|| i18n::Localized::new(MIDI_OUT_INFO.id, Params::infos(), &[]))
    }
}

/// A7: a MIDI event sent from a piece is placed in the host's block. In "sample" mode a parameter
/// change at offset 32 splits the 64-sample block; the second piece's offset 0 is block offset 32.
#[test]
fn a7_midi_out_offsets_are_block_offsets() {
    let mut h = Harness::<MidiOutProbe>::new(48_000.0, 64);
    h.set_automation_mode(AutomationMode::Sample, 0);
    let events = [h.param_event(32, "amount", 1.0)];
    let mut buf = [0.5f32; 64];
    assert!(h.process_events(&mut [&mut buf], &events));
    let offsets: Vec<u32> = h.midi_out().iter().map(|(offset, _, _)| *offset).collect();
    assert_eq!(offsets, vec![0, 32]);
}

/// A8: `InstanceHandle::set_normalized` with a custom scale that panics keeps the value; it does
/// not unwind into the caller.
#[test]
fn a8_instance_handle_conversions_survive_a_custom_scale_panic() {
    let h = Harness::<Scaled>::new(48_000.0, 512);
    let handle = h.instance();
    assert_eq!(handle.normalized("warp"), Some(0.5));
    handle.set_normalized("warp", 0.8);
    assert_eq!(handle.value("warp"), Some(0.5), "the value stays where it was");
}
