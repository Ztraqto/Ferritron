//! Polyphonic modulation (N5): per-voice offsets for parameters, and MPE-style expression.
//!
//! A synth keeps one [`VoiceMods`] next to its voices. It feeds it the block's MIDI
//! ([`VoiceMods::feed`]: note on allocates a voice, note off releases it (its offsets stay until
//! [`VoiceMods::end`]), MPE per-channel pitch bend,
//! pressure and slide (CC 74) and polyphonic pressure become that voice's [`Expression`]) and
//! sets modulation amounts of its own ([`VoiceMods::set`]: an envelope or LFO per voice). Then
//! [`Context::value_for`] reads a parameter *for one voice*: the parameter's value (smoothed, as
//! `value()`) plus that voice's offset, kept inside the parameter's range.
//!
//! ```ignore
//! for e in ctx.midi() { self.mods.feed(e); }
//! for v in &mut self.voices {
//!     let cutoff = ctx.value_for(&ctx.params().cutoff, &self.mods, v.id);   // base + this voice's offset
//! }
//! ```
//!
//! A parameter offset is in the parameter's plain units (as CLAP's `param-mod` is). Nothing
//! here allocates or locks, so it is for the audio thread.
//!
//! **Hosts' per-note modulation** arrives as [`Context::voice_events`]: CLAP polyphonic
//! modulation of parameters declared `poly = true`, CLAP note expressions and VST3 Note
//! Expression. Feed notes with [`VoiceMods::feed_note`] (it keeps the host's note IDs) and those
//! events with [`VoiceMods::feed_voice`]; `value_for` then adds the host's offset to the
//! plugin's own, and [`VoiceMods::expression`] carries the host's expressions.
//!
//! ```ignore
//! for n in ctx.notes() { self.mods.feed_note(n); }
//! for e in ctx.voice_events() { self.mods.feed_voice(e); }
//! ```

use crate::{Context, ExpressionKind, FloatParam, MidiEvent, MidiKind, NoteEvent, NoteKind, Param, ParamKind, ParamSet, VoiceEvent, VoiceEventKind};

/// Voices tracked at once.
pub const MAX_VOICES: usize = 64;
/// Parameters one voice can have an offset for.
pub const MAX_MODS_PER_VOICE: usize = 8;

/// Which voice: the MIDI channel and key of its note. With MPE every note has its own channel;
/// otherwise the key tells notes apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VoiceId {
    pub channel: u8,
    pub key: u8,
}

impl VoiceId {
    pub const fn new(channel: u8, key: u8) -> VoiceId {
        VoiceId { channel, key }
    }
}

/// What a voice's controller and the host say now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Expression {
    /// Pitch bend of the voice's channel, `-1.0..=1.0`.
    pub bend: f32,
    /// Pressure: the host's note expression, else the note's own (polyphonic), else the
    /// channel's, `0.0..=1.0`.
    pub pressure: f32,
    /// The host's brightness note expression, else CC 74 of the voice's channel (MPE "slide"),
    /// `0.0..=1.0`.
    pub slide: f32,
    /// The host's tuning note expression in semitones (0 = none).
    pub tuning: f32,
    /// The host's volume note expression, linear gain `0.0..=4.0` (1 = unchanged).
    pub volume: f32,
    /// The host's pan note expression, `0.0..=1.0` (0.5 = center).
    pub pan: f32,
    /// The host's vibrato note expression, `0.0..=1.0`.
    pub vibrato: f32,
    /// The host's expression note expression, `0.0..=1.0`.
    pub expression: f32,
}

const NEUTRAL: Expression = Expression { bend: 0.0, pressure: 0.0, slide: 0.0, tuning: 0.0, volume: 1.0, pan: 0.5, vibrato: 0.0, expression: 0.0 };

impl Default for Expression {
    fn default() -> Self {
        NEUTRAL
    }
}

/// Host note expressions of one voice (CLAP numbering), and which were set.
#[derive(Clone, Copy)]
struct HostExpr {
    values: [f32; 7],
    set: u8,
}

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    /// The note-off came: the voice is released but keeps its offsets until [`VoiceMods::end`].
    released: bool,
    id: VoiceId,
    /// Polyphonic pressure of this note, if it sent one.
    poly_pressure: Option<f32>,
    mods: [(u32, f64); MAX_MODS_PER_VOICE],
    n_mods: u8,
    /// The host's note ID (-1: none).
    note_id: i32,
    /// The host's polyphonic modulation (kept apart from the plugin's own offsets).
    host_mods: [(u32, f64); MAX_MODS_PER_VOICE],
    n_host: u8,
    host_expr: HostExpr,
}

const EMPTY: Slot = Slot {
    used: false,
    released: false,
    id: VoiceId { channel: 0, key: 0 },
    poly_pressure: None,
    mods: [(0, 0.0); MAX_MODS_PER_VOICE],
    n_mods: 0,
    note_id: -1,
    host_mods: [(0, 0.0); MAX_MODS_PER_VOICE],
    n_host: 0,
    host_expr: HostExpr { values: [0.0; 7], set: 0 },
};

/// Sets `(key, amount)` in a small table; false when it is full.
fn put(table: &mut [(u32, f64); MAX_MODS_PER_VOICE], n: &mut u8, key: u32, amount: f64) -> bool {
    let len = *n as usize;
    if let Some(m) = table[..len].iter_mut().find(|m| m.0 == key) {
        m.1 = amount;
        return true;
    }
    if len == MAX_MODS_PER_VOICE {
        return false;
    }
    table[len] = (key, amount);
    *n += 1;
    true
}

fn get(table: &[(u32, f64)], key: u32) -> f64 {
    table.iter().find(|m| m.0 == key).map_or(0.0, |m| m.1)
}

/// Per-voice parameter offsets and expression (see the [module docs](self)).
pub struct VoiceMods {
    slots: [Slot; MAX_VOICES],
    channels: [Expression; 16],
    /// The host's modulation for every note (CLAP monophonic `PARAM_MOD`).
    global: [(u32, f64); MAX_MODS_PER_VOICE],
    n_global: u8,
}

impl Default for VoiceMods {
    fn default() -> Self {
        VoiceMods::new()
    }
}

impl VoiceMods {
    pub fn new() -> VoiceMods {
        VoiceMods { slots: [EMPTY; MAX_VOICES], channels: [NEUTRAL; 16], global: [(0, 0.0); MAX_MODS_PER_VOICE], n_global: 0 }
    }

    /// Forgets every voice and the channels' expression.
    pub fn reset(&mut self) {
        *self = VoiceMods::new();
    }

    fn slot(&self, id: VoiceId) -> Option<&Slot> {
        self.slots.iter().find(|s| s.used && s.id == id)
    }

    fn slot_mut(&mut self, id: VoiceId) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.used && s.id == id)
    }

    /// Starts tracking a voice (a second `note_on` for the same id restarts it). Takes a free
    /// slot, else a released one. Returns `false` when all [`MAX_VOICES`] are in use and held.
    pub fn note_on(&mut self, id: VoiceId) -> bool {
        if let Some(s) = self.slot_mut(id) {
            *s = Slot { used: true, id, ..EMPTY };
            return true;
        }
        let free = self.slots.iter().position(|s| !s.used).or_else(|| self.slots.iter().position(|s| s.released));
        match free {
            Some(i) => {
                self.slots[i] = Slot { used: true, id, ..EMPTY };
                true
            }
            None => false,
        }
    }

    /// The note-off of a voice: it is released, but its offsets stay (a release tail still
    /// modulates) until [`end`](Self::end).
    pub fn note_off(&mut self, id: VoiceId) {
        if let Some(s) = self.slot_mut(id) {
            s.released = true;
        }
    }

    /// The voice has finished sounding: its offsets are dropped and its slot is free. Does
    /// nothing while the note is still held (the same key may have started a new note).
    pub fn end(&mut self, id: VoiceId) {
        if let Some(s) = self.slot_mut(id) {
            if s.released {
                *s = EMPTY;
            }
        }
    }

    /// Whether the voice's note is still held (released voices are not).
    pub fn is_active(&self, id: VoiceId) -> bool {
        self.slot(id).is_some_and(|s| !s.released)
    }

    /// Voices whose notes are still held.
    pub fn active_voices(&self) -> usize {
        self.slots.iter().filter(|s| s.used && !s.released).count()
    }

    /// Sets the offset of parameter `param_index` (the position in the project file) for a voice,
    /// in plain units. Returns `false` if the voice is unknown or has no room for another
    /// parameter.
    pub fn set(&mut self, id: VoiceId, param_index: usize, amount: f64) -> bool {
        let Some(s) = self.slot_mut(id) else { return false };
        put(&mut s.mods, &mut s.n_mods, param_index as u32, amount)
    }

    /// The offset of a voice and parameter: the plugin's own ([`set`](Self::set)) plus the
    /// host's (per note and for every note). 0 if none.
    pub fn amount(&self, id: VoiceId, param_index: usize) -> f64 {
        let key = param_index as u32;
        let global = get(&self.global[..self.n_global as usize], key);
        self.slot(id).map_or(global, |s| get(&s.mods[..s.n_mods as usize], key) + get(&s.host_mods[..s.n_host as usize], key) + global)
    }

    /// The voice's expression: its channel's bend, slide and pressure, the note's own
    /// polyphonic pressure over the channel's, and the host's note expressions over both.
    pub fn expression(&self, id: VoiceId) -> Expression {
        let mut e = self.channels[(id.channel & 15) as usize];
        if let Some(s) = self.slot(id) {
            if let Some(p) = s.poly_pressure {
                e.pressure = p;
            }
            let h = s.host_expr;
            let has = |k: ExpressionKind| h.set & (1 << k as u8) != 0;
            let v = |k: ExpressionKind| h.values[k as usize];
            if has(ExpressionKind::Volume) {
                e.volume = v(ExpressionKind::Volume);
            }
            if has(ExpressionKind::Pan) {
                e.pan = v(ExpressionKind::Pan);
            }
            if has(ExpressionKind::Tuning) {
                e.tuning = v(ExpressionKind::Tuning);
            }
            if has(ExpressionKind::Vibrato) {
                e.vibrato = v(ExpressionKind::Vibrato);
            }
            if has(ExpressionKind::Expression) {
                e.expression = v(ExpressionKind::Expression);
            }
            if has(ExpressionKind::Brightness) {
                e.slide = v(ExpressionKind::Brightness);
            }
            if has(ExpressionKind::Pressure) {
                e.pressure = v(ExpressionKind::Pressure);
            }
        }
        e
    }

    /// Follows a note of `ctx.notes()`: starts or stops its voice and keeps the host's note ID
    /// (per-note events may name a note by it).
    pub fn feed_note(&mut self, note: &NoteEvent) {
        let id = VoiceId::new(note.channel, note.key);
        match note.kind {
            NoteKind::On => {
                if self.note_on(id) {
                    if let Some(s) = self.slot_mut(id) {
                        s.note_id = note.note_id;
                    }
                }
            }
            NoteKind::Off => self.note_off(id),
        }
    }

    /// Follows a host's per-note event of `ctx.voice_events()`. A modulation for no note in
    /// particular (note ID, key and channel all -1) sets the offset for every note, now and
    /// later.
    pub fn feed_voice(&mut self, event: &VoiceEvent) {
        let everyone = event.note_id < 0 && event.key < 0 && event.channel < 0;
        if everyone {
            if let VoiceEventKind::Mod { index, amount } = event.kind {
                put(&mut self.global, &mut self.n_global, index as u32, amount);
                return;
            }
        }
        for s in self.slots.iter_mut().filter(|s| s.used) {
            let matches = if event.note_id >= 0 && s.note_id >= 0 {
                s.note_id == event.note_id
            } else if event.note_id >= 0 && event.key < 0 && event.channel < 0 {
                // A note ID with no key or channel names one note: a voice without that ID is not it.
                false
            } else {
                (event.key < 0 || event.key as u8 == s.id.key) && (event.channel < 0 || event.channel as u8 == s.id.channel)
            };
            if !matches {
                continue;
            }
            match event.kind {
                VoiceEventKind::Mod { index, amount } => {
                    put(&mut s.host_mods, &mut s.n_host, index as u32, amount);
                }
                VoiceEventKind::Expression { kind, value } => {
                    s.host_expr.values[kind as usize] = value;
                    s.host_expr.set |= 1 << kind as u8;
                }
            }
        }
    }

    /// Follows one MIDI event: notes start and stop voices, controllers set expression.
    pub fn feed(&mut self, event: &MidiEvent) {
        let ch = (event.channel & 15) as usize;
        match event.kind {
            MidiKind::NoteOn { key, .. } => {
                self.note_on(VoiceId::new(event.channel, key));
            }
            MidiKind::NoteOff { key, .. } => self.note_off(VoiceId::new(event.channel, key)),
            MidiKind::PitchBend(v) => self.channels[ch].bend = v,
            MidiKind::ChannelPressure(p) => self.channels[ch].pressure = p,
            MidiKind::Cc { number: 74, value } => self.channels[ch].slide = f32::from(value) / 127.0,
            MidiKind::PolyPressure { key, pressure } => {
                if let Some(s) = self.slot_mut(VoiceId::new(event.channel, key)) {
                    s.poly_pressure = Some(pressure);
                }
            }
            _ => {}
        }
    }
}

impl<P: ParamSet> Context<'_, P> {
    /// The value of float parameter `p` for one voice: its value now (smoothed, as
    /// [`FloatParam::value`]) plus the voice's offset from `mods`, kept inside the parameter's
    /// range. A voice without an offset gets the plain value. Never allocates.
    pub fn value_for(&self, p: &FloatParam, mods: &VoiceMods, voice: VoiceId) -> f32 {
        let base = p.value();
        let cell = p.param_cell();
        let Some(index) = self.seen.index(cell) else { return base };
        let amount = mods.amount(voice, index);
        if amount == 0.0 {
            return base;
        }
        match P::infos()[index].kind {
            ParamKind::Float { min, max } => (f64::from(base) + amount).clamp(min, max) as f32,
            _ => base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(channel: u8, kind: MidiKind) -> MidiEvent {
        MidiEvent { offset: 0, channel, kind }
    }

    #[test]
    fn notes_start_and_stop_voices() {
        let mut m = VoiceMods::new();
        m.feed(&ev(1, MidiKind::NoteOn { key: 60, velocity: 1.0 }));
        m.feed(&ev(2, MidiKind::NoteOn { key: 60, velocity: 1.0 }));
        assert_eq!(m.active_voices(), 2, "the same key on two channels (MPE) is two voices");
        assert!(m.is_active(VoiceId::new(1, 60)));
        m.feed(&ev(1, MidiKind::NoteOff { key: 60, velocity: 0.0 }));
        assert!(!m.is_active(VoiceId::new(1, 60)));
        assert_eq!(m.active_voices(), 1);
    }

    #[test]
    fn offsets_are_per_voice_and_dropped_with_the_note() {
        let mut m = VoiceMods::new();
        let (a, b) = (VoiceId::new(0, 60), VoiceId::new(0, 64));
        m.note_on(a);
        m.note_on(b);
        assert!(m.set(a, 3, 100.0));
        assert!(m.set(a, 3, 250.0), "setting again replaces");
        assert!(m.set(b, 3, -40.0));
        assert_eq!((m.amount(a, 3), m.amount(b, 3), m.amount(a, 4)), (250.0, -40.0, 0.0));
        assert!(!m.set(VoiceId::new(0, 99), 3, 1.0), "unknown voice");
        m.note_off(a);
        assert_eq!(m.amount(a, 3), 250.0, "a released voice keeps its offsets");
        m.end(a);
        assert_eq!(m.amount(a, 3), 0.0);
        m.note_on(a);
        assert_eq!(m.amount(a, 3), 0.0, "a new note starts clean");
    }

    #[test]
    fn the_tables_have_limits_and_say_so() {
        let mut m = VoiceMods::new();
        let v = VoiceId::new(0, 1);
        m.note_on(v);
        for i in 0..MAX_MODS_PER_VOICE {
            assert!(m.set(v, i, 1.0));
        }
        assert!(!m.set(v, MAX_MODS_PER_VOICE, 1.0), "no room for another parameter");
        let mut m = VoiceMods::new();
        for k in 0..MAX_VOICES as u8 {
            assert!(m.note_on(VoiceId::new(0, k)));
        }
        assert!(!m.note_on(VoiceId::new(0, 100)), "all voices in use");
        assert!(m.note_on(VoiceId::new(0, 3)), "restarting a running voice needs no free slot");
    }

    #[test]
    fn mpe_expression_follows_the_channel_and_the_note() {
        let mut m = VoiceMods::new();
        let v = VoiceId::new(3, 62);
        m.feed(&ev(3, MidiKind::NoteOn { key: 62, velocity: 0.5 }));
        m.feed(&ev(3, MidiKind::PitchBend(0.25)));
        m.feed(&ev(3, MidiKind::Cc { number: 74, value: 127 }));
        m.feed(&ev(3, MidiKind::ChannelPressure(0.5)));
        m.feed(&ev(4, MidiKind::PitchBend(-1.0)));
        let e = m.expression(v);
        assert_eq!((e.bend, e.slide, e.pressure), (0.25, 1.0, 0.5));
        m.feed(&ev(3, MidiKind::PolyPressure { key: 62, pressure: 0.9 }));
        assert_eq!(m.expression(v).pressure, 0.9, "the note's own pressure wins");
        assert_eq!(m.expression(VoiceId::new(4, 10)).bend, -1.0);
        m.reset();
        assert_eq!(m.expression(v), Expression::default());
    }
}
