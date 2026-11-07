use std::sync::atomic::{AtomicU64, Ordering};

/// The value range and type of a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Float { min: f64, max: f64 },
    Int { min: i32, max: i32 },
    Bool,
    Enum { choices: &'static [&'static str] },
}

/// Static description of a parameter, generated from the project file (`.frtx`).
#[derive(Debug)]
pub struct ParamInfo {
    /// Stable ID derived from `key`; this is what hosts store in automation.
    pub id: u32,
    pub key: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    pub kind: ParamKind,
    /// Default value in plain units.
    pub default: f64,
}

impl ParamInfo {
    /// Number of discrete steps: 0 for continuous parameters.
    pub fn step_count(&self) -> u32 {
        match self.kind {
            ParamKind::Float { .. } => 0,
            ParamKind::Int { min, max } => (max - min).max(0) as u32,
            ParamKind::Bool => 1,
            ParamKind::Enum { choices } => choices.len().saturating_sub(1) as u32,
        }
    }

    fn plain_range(&self) -> (f64, f64) {
        match self.kind {
            ParamKind::Float { min, max } => (min, max),
            ParamKind::Int { min, max } => (min as f64, max as f64),
            ParamKind::Bool => (0.0, 1.0),
            ParamKind::Enum { choices } => (0.0, choices.len().saturating_sub(1) as f64),
        }
    }

    /// Clamps a plain value into range and rounds it for discrete parameters.
    pub fn sanitize(&self, plain: f64) -> f64 {
        let (min, max) = self.plain_range();
        let clamped = if plain.is_nan() { self.default } else { plain.clamp(min, max) };
        if self.step_count() > 0 { clamped.round() } else { clamped }
    }

    /// Converts a plain value to the normalized `0.0..=1.0` range.
    pub fn normalize(&self, plain: f64) -> f64 {
        let (min, max) = self.plain_range();
        if max <= min {
            return 0.0;
        }
        (self.sanitize(plain) - min) / (max - min)
    }

    /// Converts a normalized `0.0..=1.0` value to a plain value.
    pub fn denormalize(&self, normalized: f64) -> f64 {
        let (min, max) = self.plain_range();
        self.sanitize(min + normalized.clamp(0.0, 1.0) * (max - min))
    }

    /// Formats a plain value for display, without the unit.
    pub fn format(&self, plain: f64) -> String {
        let plain = self.sanitize(plain);
        match self.kind {
            ParamKind::Float { .. } => format!("{plain:.2}"),
            ParamKind::Int { .. } => format!("{}", plain as i64),
            ParamKind::Bool => if plain >= 0.5 { "On" } else { "Off" }.to_string(),
            ParamKind::Enum { choices } => choices[plain as usize].to_string(),
        }
    }

    /// Parses display text back to a plain value.
    pub fn parse(&self, text: &str) -> Option<f64> {
        let text = text.trim();
        let text = text.strip_suffix(self.unit).unwrap_or(text).trim();
        let value = match self.kind {
            ParamKind::Float { .. } | ParamKind::Int { .. } => text.parse().ok()?,
            ParamKind::Bool => match text.to_ascii_lowercase().as_str() {
                "on" | "true" | "1" => 1.0,
                "off" | "false" | "0" => 0.0,
                _ => return None,
            },
            ParamKind::Enum { choices } => match choices.iter().position(|c| c.eq_ignore_ascii_case(text)) {
                Some(index) => index as f64,
                None => text.parse().ok()?,
            },
        };
        Some(self.sanitize(value))
    }
}

/// Lock-free storage of one parameter's plain value, shared between the
/// audio thread and the host's UI thread.
#[derive(Debug)]
pub struct ParamCell(AtomicU64);

impl ParamCell {
    pub const fn new(plain: f64) -> Self {
        Self(AtomicU64::new(plain.to_bits()))
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, plain: f64) {
        self.0.store(plain.to_bits(), Ordering::Relaxed);
    }
}

macro_rules! typed_param {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name(ParamCell);

        impl $name {
            pub const fn new(default: f64) -> Self {
                Self(ParamCell::new(default))
            }

            pub fn cell(&self) -> &ParamCell {
                &self.0
            }
        }
    };
}

typed_param!(
    /// A continuous parameter.
    FloatParam
);
typed_param!(
    /// An integer parameter.
    IntParam
);
typed_param!(
    /// An on/off parameter.
    BoolParam
);
typed_param!(
    /// A parameter that selects one of several named choices.
    EnumParam
);

impl FloatParam {
    pub fn value(&self) -> f32 {
        self.0.get() as f32
    }
}

impl IntParam {
    pub fn value(&self) -> i32 {
        self.0.get() as i32
    }
}

impl BoolParam {
    pub fn value(&self) -> bool {
        self.0.get() >= 0.5
    }
}

impl EnumParam {
    /// Index of the selected choice.
    pub fn index(&self) -> usize {
        self.0.get() as usize
    }
}

/// The full parameter set of a plugin. Implemented by generated code.
pub trait ParamSet: Default + Send + Sync + 'static {
    fn infos() -> &'static [ParamInfo];
    fn cell(&self, index: usize) -> &ParamCell;
    /// Maps a parameter ID to its index. Must not allocate or lock.
    fn index_of(id: u32) -> Option<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: ParamKind, default: f64) -> ParamInfo {
        ParamInfo { id: 1, key: "p", name: "P", unit: "dB", kind, default }
    }

    #[test]
    fn float_normalization_roundtrip() {
        let p = info(ParamKind::Float { min: -60.0, max: 12.0 }, 0.0);
        assert_eq!(p.normalize(-60.0), 0.0);
        assert_eq!(p.normalize(12.0), 1.0);
        assert!((p.denormalize(p.normalize(-6.0)) + 6.0).abs() < 1e-9);
        assert_eq!(p.denormalize(2.0), 12.0);
    }

    #[test]
    fn discrete_values_are_rounded() {
        let p = info(ParamKind::Int { min: 0, max: 4 }, 0.0);
        assert_eq!(p.step_count(), 4);
        assert_eq!(p.denormalize(0.6), 2.0);
        let e = info(ParamKind::Enum { choices: &["A", "B", "C"] }, 0.0);
        assert_eq!(e.denormalize(1.0), 2.0);
        assert_eq!(e.format(1.0), "B");
    }

    #[test]
    fn parse_accepts_display_text() {
        let p = info(ParamKind::Float { min: -60.0, max: 12.0 }, 0.0);
        assert_eq!(p.parse("-6.5 dB"), Some(-6.5));
        assert_eq!(p.parse("100"), Some(12.0));
        assert_eq!(p.parse("abc"), None);
        let b = info(ParamKind::Bool, 0.0);
        assert_eq!(b.parse("On"), Some(1.0));
        let e = info(ParamKind::Enum { choices: &["Low", "High"] }, 0.0);
        assert_eq!(e.parse("high"), Some(1.0));
    }

    #[test]
    fn degenerate_ranges_do_not_divide_by_zero() {
        let p = info(ParamKind::Int { min: 3, max: 3 }, 3.0);
        assert_eq!(p.normalize(3.0), 0.0);
        let e = info(ParamKind::Enum { choices: &["Only"] }, 0.0);
        assert_eq!(e.step_count(), 0);
        assert_eq!(e.denormalize(1.0), 0.0);
    }
}
