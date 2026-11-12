use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The value range and type of a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Float { min: f64, max: f64 },
    Int { min: i32, max: i32 },
    Bool,
    Enum { choices: &'static [&'static str] },
}

/// How a float parameter's plain value maps to the normalized `0..=1` hosts
/// automate (`scale = ...` in the project file). Stored values are always plain.
#[derive(Debug, Clone, Copy)]
pub enum ParamScale {
    /// `min + n * (max - min)` (the default, and what 1.3.0 did).
    Linear,
    /// `min * (max / min)^n`: equal steps are equal ratios (frequencies). `min` must be above 0.
    Log,
    /// Linear, bent so that `center` sits at `n = 0.5` (`scale = { skew_center = 0 }`).
    Skew { center: f64 },
    /// The plugin's own: `Process::param_normalize` / `param_denormalize`.
    /// Called on any thread, so it must not allocate or lock.
    Custom { normalize: fn(f64) -> f64, denormalize: fn(f64) -> f64 },
}

/// Which parameter a hook of the plugin is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamRef {
    /// Position among the plugin's own parameters.
    pub index: usize,
    /// The parameter ID hosts know it by.
    pub id: u32,
    pub key: &'static str,
}

/// What a plugin's `parse_param_text` makes of the text a user typed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParsedText {
    /// The plain value.
    Value(f64),
    /// Not accepted: the value stays as it was.
    Reject,
    /// Let Ferritron's own parser try (number, unit suffix, choice names).
    Default,
}

/// A plugin's own display and text input for one parameter (`text = "custom"`
/// in the project file). The functions are the plugin's
/// `Process::format_param_text` / `parse_param_text`, with the parameter bound.
#[derive(Debug, Clone, Copy)]
pub struct ParamTextFns {
    /// Writes the display of a plain value; `false` = use the default display.
    pub format: fn(f64, &mut dyn std::fmt::Write) -> bool,
    pub parse: fn(&str) -> ParsedText,
}

/// Static description of a parameter, generated from the project file (`.ferritroject`).
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
    /// Plain <-> normalized mapping (floats only; others are linear).
    pub scale: ParamScale,
    /// The plugin's own display and text input, if the project file asks for it.
    pub text: Option<ParamTextFns>,
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
        let plain = self.sanitize(plain);
        let n = match self.scale {
            ParamScale::Linear => (plain - min) / (max - min),
            ParamScale::Log if min > 0.0 => (plain / min).ln() / (max / min).ln(),
            ParamScale::Log => (plain - min) / (max - min),
            ParamScale::Skew { center } => match skew(min, max, center) {
                Some(k) => ((plain - min) / (max - min)).powf(k),
                None => (plain - min) / (max - min),
            },
            ParamScale::Custom { normalize, .. } => normalize(plain),
        };
        if n.is_nan() { 0.0 } else { n.clamp(0.0, 1.0) }
    }

    /// Converts a normalized `0.0..=1.0` value to a plain value.
    pub fn denormalize(&self, normalized: f64) -> f64 {
        let (min, max) = self.plain_range();
        let n = if normalized.is_nan() { 0.0 } else { normalized.clamp(0.0, 1.0) };
        let plain = match self.scale {
            ParamScale::Linear => min + n * (max - min),
            ParamScale::Log if min > 0.0 && max > min => min * (max / min).powf(n),
            ParamScale::Log => min + n * (max - min),
            ParamScale::Skew { center } => match skew(min, max, center) {
                Some(k) => min + (max - min) * n.powf(1.0 / k),
                None => min + n * (max - min),
            },
            ParamScale::Custom { denormalize, .. } => denormalize(n),
        };
        self.sanitize(plain)
    }

    /// Formats a plain value for display, without the unit.
    pub fn format(&self, plain: f64) -> String {
        let plain = self.sanitize(plain);
        if let Some(text) = &self.text {
            let mut out = String::new();
            if (text.format)(plain, &mut out) {
                return out;
            }
        }
        match self.kind {
            ParamKind::Float { .. } => format!("{plain:.2}"),
            ParamKind::Int { .. } => format!("{}", plain as i64),
            ParamKind::Bool => if plain >= 0.5 { "On" } else { "Off" }.to_string(),
            ParamKind::Enum { choices } => choices[plain as usize].to_string(),
        }
    }

    /// Parses display text back to a plain value.
    pub fn parse(&self, text: &str) -> Option<f64> {
        if let Some(own) = &self.text {
            match (own.parse)(text) {
                ParsedText::Value(v) => return Some(self.sanitize(v)),
                ParsedText::Reject => return None,
                ParsedText::Default => {}
            }
        }
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

/// The exponent that puts `center` at the middle of `min..max`, if it can.
fn skew(min: f64, max: f64, center: f64) -> Option<f64> {
    let mid = (center - min) / (max - min);
    (mid > 0.0 && mid < 1.0 && (mid - 0.5).abs() > 1e-12).then(|| 0.5f64.ln() / mid.ln())
}

const NOT_SMOOTHED: u64 = u64::MAX;

/// Lock-free storage of one parameter's plain value, shared between the
/// audio thread and the host's UI thread.
#[derive(Debug)]
pub struct ParamCell {
    value: AtomicU64,
    /// The smoothed value the plugin reads (`FloatParam::value`);
    /// `NOT_SMOOTHED` for parameters without `smooth`.
    shown: AtomicU64,
    /// Counts the writes that changed the value (wraps). Readers compare it
    /// with the version they saw last (`Context::changed`).
    version: AtomicU32,
}

impl ParamCell {
    pub const fn new(plain: f64) -> Self {
        Self { value: AtomicU64::new(plain.to_bits()), shown: AtomicU64::new(NOT_SMOOTHED), version: AtomicU32::new(0) }
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.value.load(Ordering::Relaxed))
    }

    /// Writing the value it already has changes nothing, not even the version.
    pub fn set(&self, plain: f64) {
        let bits = plain.to_bits();
        if self.value.swap(bits, Ordering::Relaxed) != bits {
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// The value the plugin sees: the smoothed one when the parameter is smoothed.
    pub fn shown(&self) -> f64 {
        let s = self.shown.load(Ordering::Relaxed);
        if s == NOT_SMOOTHED { self.get() } else { f64::from_bits(s) }
    }

    pub(crate) fn set_shown(&self, plain: Option<f64>) {
        self.shown.store(plain.map_or(NOT_SMOOTHED, f64::to_bits), Ordering::Relaxed);
    }

    /// How many times the value changed (wrapping).
    pub fn version(&self) -> u32 {
        self.version.load(Ordering::Acquire)
    }
}

/// A typed parameter (`FloatParam`, `IntParam`, ...).
pub trait Param {
    fn param_cell(&self) -> &ParamCell;
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

        impl Param for $name {
            fn param_cell(&self) -> &ParamCell {
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
    /// The value for the piece being processed. With `smooth` in the project
    /// file this is the smoothed value at the start of the piece; without it,
    /// the host's value.
    pub fn value(&self) -> f32 {
        self.0.shown() as f32
    }

    /// The value the host set (no smoothing).
    pub fn target(&self) -> f32 {
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

/// How hosts may use a parameter: `automatable` and `hidden` in the
/// project file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamOptions {
    /// Hosts offer it for automation (default true).
    pub automatable: bool,
    /// Hosts do not show it in their parameter lists (default false); it is
    /// still saved and the plugin's own editor may still show it.
    pub hidden: bool,
    /// Hosts may modulate it per note (`poly = true`, floats only): CLAP polyphonic modulation.
    /// The plugin reads per-voice values with `ctx.value_for`.
    pub poly: bool,
}

impl ParamOptions {
    pub const DEFAULT: Self = Self { automatable: true, hidden: false, poly: false };
}

impl Default for ParamOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The full parameter set of a plugin. Implemented by generated code.
pub trait ParamSet: Default + Send + Sync + 'static {
    fn infos() -> &'static [ParamInfo];
    fn cell(&self, index: usize) -> &ParamCell;
    /// Maps a parameter ID to its index. Must not allocate or lock.
    fn index_of(id: u32) -> Option<usize>;
    /// How the parameter at `index` is smoothed (`smooth` in the project file).
    fn smoothing(index: usize) -> Option<crate::smooth::Smoothing> {
        let _ = index;
        None
    }
    /// Options of the parameter at `index` (see [`ParamOptions`]).
    fn options(index: usize) -> ParamOptions {
        let _ = index;
        ParamOptions::DEFAULT
    }
    /// The streams declared under `[streams]` (generated code overrides this).
    fn stream_hub(&self) -> Option<&crate::stream::StreamHub> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: ParamKind, default: f64) -> ParamInfo {
        ParamInfo { id: 1, key: "p", name: "P", unit: "dB", kind, default, scale: ParamScale::Linear, text: None }
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
