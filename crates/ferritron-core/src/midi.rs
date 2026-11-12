//! MIDI input, speaker layouts and the sidechain: what `ctx.midi()`,
//! `Setup::layout` and `ctx.sidechain()` hand to the plugin.

/// What a MIDI message says. Values are already scaled: pitch bend is
/// `-1.0..=1.0`, pressure and velocity `0.0..=1.0`, controller values `0..=127`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MidiKind {
    NoteOn { key: u8, velocity: f32 },
    NoteOff { key: u8, velocity: f32 },
    /// Controller change. `number` 0 and 32 are the bank select MSB and LSB.
    Cc { number: u8, value: u8 },
    /// `-1.0..=1.0`, 0 = centre.
    PitchBend(f32),
    /// Channel aftertouch.
    ChannelPressure(f32),
    /// Polyphonic aftertouch (CLAP and VST2 hosts; VST3 hosts do not send it).
    PolyPressure { key: u8, pressure: f32 },
    ProgramChange(u8),
}

/// A MIDI message inside the current block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiEvent {
    /// Sample offset inside `ctx.io()`.
    pub offset: u32,
    /// MIDI channel, 0..=15.
    pub channel: u8,
    pub kind: MidiKind,
}

pub const MIDI_STATUS_CC: u8 = 0xB0;
pub const MIDI_STATUS_PROGRAM: u8 = 0xC0;
pub const MIDI_STATUS_CHANNEL_PRESSURE: u8 = 0xD0;
pub const MIDI_STATUS_PITCH_BEND: u8 = 0xE0;
pub const MIDI_STATUS_POLY_PRESSURE: u8 = 0xA0;

impl MidiKind {
    /// From a raw 3-byte message (`status` without its channel nibble).
    pub fn from_raw(status: u8, data1: u8, data2: u8) -> Option<Self> {
        let (d1, d2) = (data1 & 0x7F, data2 & 0x7F);
        Some(match status & 0xF0 {
            0x90 if d2 > 0 => Self::NoteOn { key: d1, velocity: f32::from(d2) / 127.0 },
            0x90 | 0x80 => Self::NoteOff { key: d1, velocity: f32::from(d2) / 127.0 },
            MIDI_STATUS_POLY_PRESSURE => Self::PolyPressure { key: d1, pressure: f32::from(d2) / 127.0 },
            MIDI_STATUS_CC => Self::Cc { number: d1, value: d2 },
            MIDI_STATUS_PROGRAM => Self::ProgramChange(d1),
            MIDI_STATUS_CHANNEL_PRESSURE => Self::ChannelPressure(f32::from(d1) / 127.0),
            MIDI_STATUS_PITCH_BEND => {
                let v = (i32::from(d2) << 7) | i32::from(d1);
                Self::PitchBend(((v - 8192) as f32 / 8192.0).clamp(-1.0, 1.0))
            }
            _ => return None,
        })
    }
}

/// A speaker position (`Setup::layout`). Channel `i` of the bus is `layout[i]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    L,
    R,
    C,
    Lfe,
    Ls,
    Rs,
    Lss,
    Rss,
    Lsr,
    Rsr,
    Lc,
    Rc,
    Cs,
    Tc,
    Tfl,
    Tfc,
    Tfr,
    Trl,
    Trc,
    Trr,
    Tsl,
    Tsr,
    Lw,
    Rw,
    Bfl,
    Bfc,
    Bfr,
    Brl,
    Brr,
    Lfe2,
    /// A position the host named that Ferritron has no name for.
    Other(u32),
}

impl Speaker {
    /// The number shims use for this speaker (`FERRITRON_SPEAKER_*`).
    pub fn id(self) -> u32 {
        match self {
            Self::Other(n) => 0x1000 + n,
            s => ALL_SPEAKERS.iter().position(|x| *x == s).unwrap_or(0) as u32,
        }
    }

    pub fn from_id(id: u32) -> Self {
        match ALL_SPEAKERS.get(id as usize) {
            Some(s) if id < 0x1000 => *s,
            _ => Self::Other(id.saturating_sub(0x1000)),
        }
    }

    /// The usual layout of `count` channels when the host does not say:
    /// 1 centre, 2 L/R, 3 L/R/C, 4 quad, 6 5.1, 8 7.1.
    pub fn default_layout(count: usize) -> SpeakerLayout {
        use Speaker::*;
        let list: &[Speaker] = match count {
            1 => &[C],
            2 => &[L, R],
            3 => &[L, R, C],
            4 => &[L, R, Ls, Rs],
            6 => &[L, R, C, Lfe, Ls, Rs],
            8 => &[L, R, C, Lfe, Ls, Rs, Lss, Rss],
            _ => &[],
        };
        let mut layout = SpeakerLayout::default();
        for (i, s) in list.iter().enumerate() {
            layout.set(i, *s);
        }
        for i in list.len()..count.min(crate::MAX_CHANNELS) {
            layout.set(i, Self::Other(i as u32));
        }
        layout
    }
}

const ALL_SPEAKERS: [Speaker; 30] = {
    use Speaker::*;
    [
        L, R, C, Lfe, Ls, Rs, Lss, Rss, Lsr, Rsr, Lc, Rc, Cs, Tc, Tfl, Tfc, Tfr, Trl, Trc, Trr, Tsl, Tsr, Lw, Rw, Bfl, Bfc, Bfr, Brl, Brr, Lfe2,
    ]
};

/// The speakers of a bus, up to [`MAX_CHANNELS`](crate::MAX_CHANNELS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerLayout {
    speakers: [Speaker; crate::MAX_CHANNELS],
    len: usize,
}

impl Default for SpeakerLayout {
    fn default() -> Self {
        Self { speakers: [Speaker::C; crate::MAX_CHANNELS], len: 0 }
    }
}

impl SpeakerLayout {
    pub fn new(speakers: &[Speaker]) -> Self {
        let mut l = Self::default();
        for (i, s) in speakers.iter().take(crate::MAX_CHANNELS).enumerate() {
            l.set(i, *s);
        }
        l
    }

    pub(crate) fn set(&mut self, index: usize, speaker: Speaker) {
        if index < crate::MAX_CHANNELS {
            self.speakers[index] = speaker;
            self.len = self.len.max(index + 1);
        }
    }

    /// The speaker of each channel, in channel order.
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The sidechain input of a block (`ctx.sidechain()`): read-only audio. A
/// mono sidechain reads as left = right (`channel(1)` is `channel(0)`).
#[derive(Clone, Copy)]
pub struct Sidechain<'a> {
    channels: &'a [*const f32],
    frames: usize,
}

impl<'a> Sidechain<'a> {
    /// # Safety
    /// Every pointer must be valid for `frames` reads for `'a`.
    pub(crate) unsafe fn new(channels: &'a [*const f32], frames: usize) -> Self {
        Self { channels, frames }
    }

    /// Channels the host connected (at least 1).
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Channel `index`; beyond the last channel of a mono signal, the only
    /// channel. Panics for other out-of-range indexes.
    pub fn channel(&self, index: usize) -> &'a [f32] {
        let i = if self.channels.len() == 1 { 0 } else { index };
        // SAFETY: `new` promises the pointers are valid for `frames` samples.
        unsafe { std::slice::from_raw_parts(self.channels[i], self.frames) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_midi_becomes_scaled_events() {
        assert_eq!(MidiKind::from_raw(0x93, 60, 127), Some(MidiKind::NoteOn { key: 60, velocity: 1.0 }));
        assert_eq!(MidiKind::from_raw(0x90, 60, 0), Some(MidiKind::NoteOff { key: 60, velocity: 0.0 }));
        assert_eq!(MidiKind::from_raw(0xB0, 1, 64), Some(MidiKind::Cc { number: 1, value: 64 }));
        assert_eq!(MidiKind::from_raw(0xC0, 5, 0), Some(MidiKind::ProgramChange(5)));
        assert_eq!(MidiKind::from_raw(0xE0, 0, 64), Some(MidiKind::PitchBend(0.0)));
        assert_eq!(MidiKind::from_raw(0xE0, 127, 127), Some(MidiKind::PitchBend(8191.0 / 8192.0)));
        assert_eq!(MidiKind::from_raw(0xE0, 0, 0), Some(MidiKind::PitchBend(-1.0)));
        assert_eq!(MidiKind::from_raw(0xF0, 0, 0), None);
    }

    #[test]
    fn speakers_have_stable_ids() {
        for s in ALL_SPEAKERS {
            assert_eq!(Speaker::from_id(s.id()), s);
        }
        assert_eq!(Speaker::from_id(Speaker::Other(77).id()), Speaker::Other(77));
        assert_eq!(Speaker::default_layout(6).speakers(), [Speaker::L, Speaker::R, Speaker::C, Speaker::Lfe, Speaker::Ls, Speaker::Rs]);
        assert_eq!(Speaker::default_layout(12).len(), 12);
    }
}
