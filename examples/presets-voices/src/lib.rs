//! Built-in display formats (N1), factory presets (N4) and per-voice parameter modulation (N5).
//!
//! * `display = "hz"` / `"db"` / ... and `format = "%d taps"` in the project file give every
//!   parameter its text and typed input (`2k`, `A#5 + 20`, `50%`, `0.5s`) without code.
//! * `[presets.warm]` lists values; a host or editor loads one in one step (the smoothers jump
//!   to it): `ferritron::presets::load::<PresetsVoices>(&instance, "warm")`.
//! * `ctx.value_for(&param, &mods, voice)` is a parameter's value for one voice: its value plus
//!   that voice's offset. Here MIDI CC 20 moves the cutoff of the note on key 60 by
//!   `(value - 64) * 500` Hz, the way a per-note modulator (an envelope, MPE slide) would.
//! * `poly = true` on the cutoff lets hosts modulate it per note (CLAP polyphonic modulation):
//!   `ctx.voice_events()` fed to the same `VoiceMods` adds the host's offset.
//!
//! The audio output carries what the test wants to see: channel 0 is the cutoff for voice
//! (channel 0, key 60), channel 1 the cutoff for key 61.

use ferritron::prelude::*;
use ferritron::{MidiKind, VoiceId, VoiceMods};

const VOICE_A: VoiceId = VoiceId::new(0, 60);
const VOICE_B: VoiceId = VoiceId::new(0, 61);

#[derive(Default)]
struct PresetsVoices {
    mods: VoiceMods,
    /// Key 60 was released in the last block: its offsets end at the start of the next one.
    ending: bool,
}

#[ferritron::plugin]
impl Process for PresetsVoices {
    fn process(&mut self, ctx: &mut Context) {
        // A released voice keeps its offsets (its release tail) until `end`: here the release is one block.
        if std::mem::take(&mut self.ending) {
            self.mods.end(VOICE_A);
        }
        for e in ctx.midi() {
            self.mods.feed(e);
            if let MidiKind::NoteOff { key: 60, .. } = e.kind {
                self.ending = true;
            }
            if let MidiKind::Cc { number: 20, value } = e.kind {
                // parameter 0 is the cutoff (the first one in the project file)
                self.mods.set(VOICE_A, 0, (f64::from(value) - 64.0) * 500.0);
            }
        }
        for e in ctx.voice_events() {
            self.mods.feed_voice(e);
        }
        let params = ctx.params();
        let a = ctx.value_for(&params.cutoff, &self.mods, VOICE_A);
        let b = ctx.value_for(&params.cutoff, &self.mods, VOICE_B);
        let io = ctx.io();
        io.channel_mut(0).fill(a);
        io.channel_mut(1).fill(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;
    use ferritron::{MidiKind, ParamSet};

    fn harness() -> Harness<PresetsVoices> {
        Harness::<PresetsVoices>::new(48_000.0, 512)
    }

    fn run(h: &mut Harness<PresetsVoices>) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        assert!(h.process(&mut [&mut l, &mut r]));
        (l, r)
    }

    // ---- N1 ---------------------------------------------------------------------------------

    #[test]
    fn every_display_shows_its_default() {
        let h = harness();
        let t = |k: &str| h.text(k);
        assert_eq!(t("cutoff"), "1 kHz");
        assert_eq!(t("level"), "0.0 dB");
        assert_eq!(t("mix"), "50%");
        assert_eq!(t("delay"), "250 ms");
        assert_eq!(t("pitch"), "A4");
        assert_eq!(t("speed"), "1x");
        assert_eq!(t("taps"), "4 taps");
        assert_eq!(t("shape"), "12.5 dB");
        // enums and switches are in the user's language (Ferritron's built-in Japanese too)
        assert!(["Clean", "クリーン"].contains(&t("mode").as_str()), "{}", t("mode"));
        assert!(["On", "オン"].contains(&t("on").as_str()), "{}", t("on"));
    }

    #[test]
    fn the_bottom_of_a_decibel_range_is_minus_infinity_and_the_text_follows_the_value() {
        let mut h = harness();
        h.set_param("level", -90.0);
        assert_eq!(h.text("level"), "-INF dB");
        h.set_param("level", -6.0);
        assert_eq!(h.text("level"), "-6.0 dB");
        h.set_param("cutoff", 1250.0);
        assert_eq!(h.text("cutoff"), "1.25 kHz");
        h.set_param("delay", 1500.0);
        assert_eq!(h.text("delay"), "1.5 s");
    }

    #[test]
    fn typed_text_is_understood_and_bad_text_leaves_the_value_alone() {
        let mut h = harness();
        // what a host does: text -> normalized -> set
        let mut typed = |h: &mut Harness<PresetsVoices>, key: &str, text: &str| -> Option<f64> {
            let n = h.parse_text(key, text)?;
            h.set_normalized(key, n);
            Some(h.params().cell(PresetsVoices::index(key)).get())
        };
        let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-6 * b.abs().max(1.0));
        assert!(close(typed(&mut h, "cutoff", "2k"), 2000.0));
        assert!(close(typed(&mut h, "cutoff", "1.5 kHz"), 1500.0));
        assert!(close(typed(&mut h, "pitch", "A#5 + 20"), 82.2));
        assert!(close(typed(&mut h, "pitch", "C#3+13"), 49.13));
        assert!(close(typed(&mut h, "pitch", "C4"), 60.0));
        assert!(close(typed(&mut h, "mix", "50%"), 0.5));
        assert!(close(typed(&mut h, "mix", "25"), 0.25));
        assert!(close(typed(&mut h, "delay", "0.5s"), 500.0));
        assert!(close(typed(&mut h, "delay", "10ms"), 10.0));
        assert!(close(typed(&mut h, "speed", "2x"), 2.0));
        assert!(close(typed(&mut h, "taps", "6"), 6.0));
        assert!(close(typed(&mut h, "taps", "6 taps"), 6.0));
        assert!(close(typed(&mut h, "level", "-inf"), -90.0), "minus infinity is the bottom of the range");
        // failures change nothing
        let before = h.params().cell(PresetsVoices::index("cutoff")).get();
        assert_eq!(h.parse_text("cutoff", "loud"), None);
        assert_eq!(h.parse_text("cutoff", "2 ms"), None);
        assert_eq!(h.parse_text("pitch", "H4"), None);
        assert_eq!(h.params().cell(PresetsVoices::index("cutoff")).get(), before);
    }

    #[test]
    fn hosts_are_told_no_unit_because_the_text_has_it() {
        let infos = <Params as ParamSet>::infos();
        for key in ["cutoff", "level", "mix", "delay", "pitch", "speed", "taps", "shape"] {
            let i = infos.iter().find(|i| i.key == key).unwrap();
            assert_eq!(i.unit, "", "{key}");
        }
    }

    impl PresetsVoices {
        fn index(key: &str) -> usize {
            <Params as ParamSet>::infos().iter().position(|i| i.key == key).unwrap()
        }
    }

    // ---- N4 ---------------------------------------------------------------------------------

    #[test]
    fn the_project_file_lists_two_presets() {
        let list = ferritron::presets::list::<PresetsVoices>();
        assert_eq!(list.iter().map(|p| (p.id, p.name)).collect::<Vec<_>>(), vec![("warm", "Warm Pad"), ("bright", "bright")]);
    }

    #[test]
    fn loading_a_preset_sets_the_listed_parameters_only_and_jumps_the_smoothers() {
        let mut h = harness();
        run(&mut h);
        // an ordinary change glides (cutoff has `smooth = "100ms"`)...
        h.set_param("cutoff", 5000.0);
        let (_, glide) = run(&mut h);
        assert!(glide[0] < 2000.0, "a plain change starts from the old value: {}", glide[0]);
        // ...a preset does not
        assert!(h.load_preset("Warm Pad"), "by name");
        let (_, r) = run(&mut h);
        assert!((r[0] - 800.0).abs() < 0.01, "the first sample is already the preset's cutoff: {}", r[0]);
        assert_eq!(h.params().mix.value(), 0.35);
        assert_eq!(h.params().mode.index(), 1);
        assert_eq!(h.params().level.value(), 0.0, "unlisted parameters keep their value");
        assert!(h.params().on.value(), "and so does `on`");
        // another preset, by id; it sets a bool and a negative number
        assert!(h.load_preset("BRIGHT"));
        let (_, r) = run(&mut h);
        assert!((r[0] - 12000.0).abs() < 0.5, "{}", r[0]);
        assert_eq!(h.params().level.value(), -6.0);
        assert!(!h.params().on.value());
        assert_eq!(h.params().mix.value(), 0.35, "still what the first preset set");
        assert!(!h.load_preset("no such preset"));
    }

    #[test]
    fn the_host_program_parameter_loads_presets_on_the_main_thread() {
        let mut h = harness();
        run(&mut h);
        let id = ferritron::__private::PROGRAM_ID;
        let index = (0..h.host_param_count()).find(|&i| h.host_param(i).unwrap().id == id).expect("a Program parameter");
        assert_eq!(h.text_index(index), "Warm Pad");
        // The host picks "bright" inside a block (the audio thread): nothing loads there...
        let mut a = [0.0f32; 64];
        let mut b = [0.0f32; 64];
        let event = ferritron::abi::Event::param(0, id, 1.0);
        h.process_events(&mut [&mut a, &mut b], &[event]);
        assert_eq!(h.params().level.value(), 0.0);
        // ...the main thread's poll loads it, and tells the host the values changed.
        assert_ne!(h.take_main_requests() & ferritron::abi::REQUEST_VALUES, 0);
        run(&mut h);
        assert_eq!(h.params().level.value(), -6.0);
        assert_eq!(ferritron::presets::current::<PresetsVoices>(h.instance()), Some(1));
        // The choice is saved; restoring it does not load the preset over the saved values.
        h.set_param("level", -12.0);
        run(&mut h);
        let state = h.save_state();
        let mut other = harness();
        assert!(other.load_state(&state));
        assert_eq!(other.take_main_requests() & ferritron::abi::REQUEST_VALUES, 0);
        run(&mut other);
        assert_eq!(other.params().level.value(), -12.0);
        assert_eq!(other.param_index(index), 1.0);
        // An editor's preset menu updates the host's program too.
        assert!(other.load_preset("warm"));
        assert_eq!(other.param_index(index), 0.0);
    }

    #[test]
    fn user_presets_are_saved_listed_loaded_and_deleted() {
        let dir = std::env::temp_dir().join(format!("ferritron-user-presets-{}", std::process::id()));
        // SAFETY: only this test reads the variable.
        unsafe { std::env::set_var("FERRITRON_PRESETS_DIR", &dir) };
        let mut h = harness();
        h.set_param("cutoff", 2345.0);
        h.set_param("mode", 2.0);
        run(&mut h);
        let path = ferritron::presets::save_user::<PresetsVoices>(h.instance(), "暗い: テスト").unwrap();
        assert!(path.starts_with(&dir) && path.exists());
        assert_eq!(ferritron::presets::user_list::<PresetsVoices>(), vec!["暗い: テスト".to_string()]);
        let mut other = harness();
        ferritron::presets::load_user::<PresetsVoices>(other.instance(), "暗い: テスト").unwrap();
        run(&mut other);
        assert_eq!(other.params().mode.index(), 2);
        assert!((other.params().cutoff.value() - 2345.0).abs() < 0.01);
        ferritron::presets::delete_user::<PresetsVoices>("暗い: テスト").unwrap();
        assert!(ferritron::presets::user_list::<PresetsVoices>().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_host_modulates_a_poly_parameter_per_note_and_for_all_notes() {
        let mut h = harness();
        let cutoff = h.host_param(0).unwrap();
        assert_ne!(cutoff.flags & ferritron::abi::PARAM_POLY, 0, "hosts see that it can be modulated per note");
        h.midi(0, 0, MidiKind::NoteOn { key: 60, velocity: 1.0 });
        h.midi(0, 0, MidiKind::NoteOn { key: 61, velocity: 1.0 });
        run(&mut h);
        let (mut a, mut b) = ([0.0f32; 64], [0.0f32; 64]);
        // CLAP PARAM_MOD for (channel 0, key 60): +2000 Hz on that voice only.
        h.process_events(&mut [&mut a, &mut b], &[ferritron::abi::Event::note_mod(0, -1, 60, 0, cutoff.id, 2000.0)]);
        assert_eq!((a[0], b[0]), (3000.0, 1000.0));
        // A modulation for every note adds to both, and to notes that start later.
        h.process_events(&mut [&mut a, &mut b], &[ferritron::abi::Event::note_mod(0, -1, -1, -1, cutoff.id, 500.0)]);
        assert_eq!((a[0], b[0]), (3500.0, 1500.0));
        // A note expression for a voice is kept for it.
        let mut mods = VoiceMods::new();
        mods.note_on(VOICE_A);
        mods.feed_voice(&ferritron::VoiceEvent { offset: 0, note_id: -1, key: 60, channel: 0, kind: ferritron::VoiceEventKind::Expression { kind: ferritron::ExpressionKind::Tuning, value: -2.0 } });
        assert_eq!(mods.expression(VOICE_A).tuning, -2.0);
        assert_eq!(mods.expression(VOICE_A).volume, 1.0, "unset expressions are neutral");
    }

    // ---- N5 ---------------------------------------------------------------------------------

    #[test]
    fn a_voice_gets_its_own_offset_and_the_others_keep_the_parameter() {
        let mut h = harness();
        h.midi(0, 0, MidiKind::NoteOn { key: 60, velocity: 1.0 });
        h.midi(0, 0, MidiKind::NoteOn { key: 61, velocity: 1.0 });
        let (a, b) = run(&mut h);
        assert_eq!((a[0], b[0]), (1000.0, 1000.0), "no offset yet");
        h.midi(0, 0, MidiKind::Cc { number: 20, value: 70 }); // (70 - 64) * 500 = +3000 Hz
        let (a, b) = run(&mut h);
        assert_eq!((a[0], b[0]), (4000.0, 1000.0), "only the voice that was modulated moves");
        h.midi(0, 0, MidiKind::Cc { number: 20, value: 0 }); // -32000: kept inside the range
        let (a, _) = run(&mut h);
        assert_eq!(a[0], 20.0, "clamped to the parameter's minimum");
        h.midi(0, 0, MidiKind::Cc { number: 20, value: 127 });
        let (a, _) = run(&mut h);
        assert_eq!(a[0], 20000.0, "and to its maximum");
        // the note ends: its offset stays while the voice is released, and goes at `end`
        h.midi(0, 0, MidiKind::NoteOff { key: 60, velocity: 0.0 });
        let (a, _) = run(&mut h);
        assert_eq!(a[0], 20000.0, "a released voice keeps its modulation");
        let (a, _) = run(&mut h);
        assert_eq!(a[0], 1000.0, "the offset goes once the voice has ended");
    }

    #[test]
    fn the_voice_value_follows_the_parameter_underneath() {
        let mut h = harness();
        h.midi(0, 0, MidiKind::NoteOn { key: 60, velocity: 1.0 });
        h.load_preset("warm");
        h.midi(0, 0, MidiKind::Cc { number: 20, value: 66 }); // +1000
        let (a, b) = run(&mut h);
        assert!((a[0] - 1800.0).abs() < 0.01 && (b[0] - 800.0).abs() < 0.01, "{} {}", a[0], b[0]);
    }

    #[test]
    fn modulation_costs_the_audio_thread_no_allocation() {
        let mut h = harness();
        run(&mut h);
        for k in 0..10u8 {
            h.midi(0, 0, MidiKind::NoteOn { key: 50 + k, velocity: 1.0 });
            h.midi(1, 0, MidiKind::Cc { number: 20, value: 80 + k });
            run(&mut h);
        }
        #[cfg(feature = "alloc-check")]
        h.assert_no_alloc();
    }
}
