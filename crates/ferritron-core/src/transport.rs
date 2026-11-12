//! What the host says about time: tempo, time signature, play state and
//! position (`ctx.transport()`).
//!
//! Every field is optional: a host that says nothing leaves them `None` /
//! `false`. The values are those at the first sample of the piece being processed (see Context::block_offset); inside one piece they do not move. A loop jump inside a block is not followed.

/// The host's transport at the start of a block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transport {
    /// Beats per minute (quarter notes).
    pub tempo: Option<f64>,
    /// Numerator and denominator, e.g. `(7, 8)`.
    pub time_signature: Option<(u16, u16)>,
    pub playing: bool,
    pub recording: bool,
    /// Position of the block's first sample in quarter notes from the project start.
    pub position_beats: Option<f64>,
    /// Position of the start of the current bar, in quarter notes.
    pub bar_start_beats: Option<f64>,
    /// Position of the block's first sample in samples from the project start.
    pub position_samples: Option<i64>,
    /// The loop, in quarter notes (`start`, `end`), if the host has one set.
    pub loop_range: Option<(f64, f64)>,
    /// The host is looping now.
    pub looping: bool,
}

impl Transport {
    /// The tempo, or `fallback` when the host did not say (120 BPM is the usual choice).
    pub fn tempo_or(&self, fallback: f64) -> f64 {
        self.tempo.filter(|t| *t > 0.0).unwrap_or(fallback)
    }

    /// Where in the current bar the block starts, in quarter notes, if known.
    pub fn beats_into_bar(&self) -> Option<f64> {
        Some(self.position_beats? - self.bar_start_beats?)
    }
}

pub const TRANSPORT_TEMPO: u32 = 1 << 0;
pub const TRANSPORT_TIME_SIGNATURE: u32 = 1 << 1;
pub const TRANSPORT_PLAYING: u32 = 1 << 2;
pub const TRANSPORT_RECORDING: u32 = 1 << 3;
pub const TRANSPORT_POSITION_BEATS: u32 = 1 << 4;
pub const TRANSPORT_BAR_START: u32 = 1 << 5;
pub const TRANSPORT_POSITION_SAMPLES: u32 = 1 << 6;
pub const TRANSPORT_LOOP_RANGE: u32 = 1 << 7;
pub const TRANSPORT_LOOPING: u32 = 1 << 8;

/// `ferritron_transport` of the C header: what shims hand over per block.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TransportC {
    /// `TRANSPORT_*` bits: which of the fields below are valid.
    pub flags: u32,
    pub numerator: u16,
    pub denominator: u16,
    pub tempo: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub position_samples: i64,
    pub loop_start: f64,
    pub loop_end: f64,
}

impl TransportC {
    pub(crate) fn to_transport(self) -> Transport {
        let has = |bit: u32| self.flags & bit != 0;
        Transport {
            tempo: has(TRANSPORT_TEMPO).then_some(self.tempo),
            time_signature: (has(TRANSPORT_TIME_SIGNATURE) && self.numerator > 0 && self.denominator > 0).then_some((self.numerator, self.denominator)),
            playing: has(TRANSPORT_PLAYING),
            recording: has(TRANSPORT_RECORDING),
            position_beats: has(TRANSPORT_POSITION_BEATS).then_some(self.position_beats),
            bar_start_beats: has(TRANSPORT_BAR_START).then_some(self.bar_start_beats),
            position_samples: has(TRANSPORT_POSITION_SAMPLES).then_some(self.position_samples),
            loop_range: has(TRANSPORT_LOOP_RANGE).then_some((self.loop_start, self.loop_end)),
            looping: has(TRANSPORT_LOOPING),
        }
    }

    pub(crate) fn from_transport(t: &Transport) -> Self {
        let mut c = Self::default();
        let bits = [
            (TRANSPORT_TEMPO, t.tempo.is_some()),
            (TRANSPORT_TIME_SIGNATURE, t.time_signature.is_some()),
            (TRANSPORT_PLAYING, t.playing),
            (TRANSPORT_RECORDING, t.recording),
            (TRANSPORT_POSITION_BEATS, t.position_beats.is_some()),
            (TRANSPORT_BAR_START, t.bar_start_beats.is_some()),
            (TRANSPORT_POSITION_SAMPLES, t.position_samples.is_some()),
            (TRANSPORT_LOOP_RANGE, t.loop_range.is_some()),
            (TRANSPORT_LOOPING, t.looping),
        ];
        for (bit, on) in bits {
            if on {
                c.flags |= bit;
            }
        }
        c.tempo = t.tempo.unwrap_or(0.0);
        (c.numerator, c.denominator) = t.time_signature.unwrap_or((0, 0));
        c.position_beats = t.position_beats.unwrap_or(0.0);
        c.bar_start_beats = t.bar_start_beats.unwrap_or(0.0);
        c.position_samples = t.position_samples.unwrap_or(0);
        (c.loop_start, c.loop_end) = t.loop_range.unwrap_or((0.0, 0.0));
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_round_trips_and_defaults_to_nothing() {
        assert_eq!(TransportC::default().to_transport(), Transport::default());
        let t = Transport {
            tempo: Some(133.5),
            time_signature: Some((7, 8)),
            playing: true,
            recording: false,
            position_beats: Some(12.25),
            bar_start_beats: Some(10.5),
            position_samples: Some(96_000),
            loop_range: Some((0.0, 16.0)),
            looping: true,
        };
        assert_eq!(TransportC::from_transport(&t).to_transport(), t);
        assert_eq!(t.beats_into_bar(), Some(1.75));
        assert_eq!(Transport::default().tempo_or(120.0), 120.0);
    }
}
