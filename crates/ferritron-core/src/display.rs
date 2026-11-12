//! Built-in display formats (N1): `display = "db"` and friends in the project file.
//!
//! A parameter with `display` shows and reads its text without any plugin code. The same text
//! goes to every host (VST3, CLAP, VST2, LV2, AAX), the editors and AI clients, because it is the
//! parameter's [`ParamTextFns`](crate::ParamTextFns). The unit is part of the text (`-6.0 dB`),
//! so such a parameter tells hosts an empty unit and they do not add a second one.
//!
//! | `display` | the plain value is | shows | accepts |
//! |---|---|---|---|
//! | `"db"` | dB | `-6.0 dB`, `-INF dB` at the bottom of a range that reaches -60 dB or lower | `-6`, `-6 dB`, `-inf` |
//! | `"hz"` | Hz | 6 significant digits, `Hz` below 1000, else `kHz` (`1.25 kHz`) | `2k`, `1.5kHz`, `440` |
//! | `"percent"` | a fraction (0.5) | `50%` | `50%`, `50` (as percent) |
//! | `"ms"` | milliseconds | `250 ms`, `1.5 s` from 1000 ms | `10ms`, `0.5s`, `10` |
//! | `"note"` | a MIDI note number (60 = C4) | `A4`, `C#3+13` (cents) | `A4`, `C#3+13`, `A#5 + 20`, `69` |
//! | `"ratio"` | a factor | `2x`, `0.5x` | `2x`, `2`, `0.5×` |
//! | `format = "%.1f dB"` | a number | the template (`%.Nf`, `%d`, `%%`) | the template's text |
//!
//! `digits` sets the decimals of `"db"` (default 1) and the significant digits of the others
//! (default 6, trailing zeros trimmed).

use std::fmt::Write;

use crate::ParsedText;

/// Which format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    Db,
    Hz,
    Percent,
    Ms,
    Note,
    Ratio,
    /// `format = "..."`: the template is in [`Display::template`].
    Template,
}

/// A built-in display format, as the generated code builds it from the project file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub kind: DisplayKind,
    /// 0 = the format's default.
    pub digits: u8,
    /// `"db"`: at or below this value the text is `-INF dB`. NaN = never.
    pub inf_at: f64,
    pub template: &'static str,
}

impl Display {
    const fn of(kind: DisplayKind) -> Display {
        Display { kind, digits: 0, inf_at: f64::NAN, template: "" }
    }
    pub const fn db() -> Display {
        Display::of(DisplayKind::Db)
    }
    pub const fn hz() -> Display {
        Display::of(DisplayKind::Hz)
    }
    pub const fn percent() -> Display {
        Display::of(DisplayKind::Percent)
    }
    pub const fn ms() -> Display {
        Display::of(DisplayKind::Ms)
    }
    pub const fn note() -> Display {
        Display::of(DisplayKind::Note)
    }
    pub const fn ratio() -> Display {
        Display::of(DisplayKind::Ratio)
    }
    pub const fn template(template: &'static str) -> Display {
        Display { kind: DisplayKind::Template, digits: 0, inf_at: f64::NAN, template }
    }
    pub const fn with_digits(mut self, digits: u8) -> Display {
        self.digits = digits;
        self
    }
    pub const fn with_inf_at(mut self, value: f64) -> Display {
        self.inf_at = value;
        self
    }

    /// Writes the text of `plain`. Always returns `true` (it is the parameter's whole display).
    pub fn format(&self, plain: f64, out: &mut dyn Write) -> bool {
        let _ = self.write(plain, out);
        true
    }

    fn write(&self, v: f64, out: &mut dyn Write) -> std::fmt::Result {
        let digits = if self.digits == 0 { 6 } else { self.digits as usize };
        match self.kind {
            DisplayKind::Db => {
                if !self.inf_at.is_nan() && v <= self.inf_at {
                    return out.write_str("-INF dB");
                }
                let decimals = if self.digits == 0 { 1 } else { self.digits as usize };
                write!(out, "{v:.decimals$} dB")
            }
            DisplayKind::Hz => {
                // 999.9999 shows as "1 kHz": decide on the rounded value
                if round_significant(v, digits).abs() >= 1000.0 {
                    significant(v / 1000.0, digits, out)?;
                    out.write_str(" kHz")
                } else {
                    significant(v, digits, out)?;
                    out.write_str(" Hz")
                }
            }
            DisplayKind::Percent => {
                significant(v * 100.0, digits, out)?;
                out.write_char('%')
            }
            DisplayKind::Ms => {
                if round_significant(v, digits).abs() >= 1000.0 {
                    significant(v / 1000.0, digits, out)?;
                    out.write_str(" s")
                } else {
                    significant(v, digits, out)?;
                    out.write_str(" ms")
                }
            }
            DisplayKind::Ratio => {
                significant(v, digits, out)?;
                out.write_char('x')
            }
            DisplayKind::Note => {
                const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
                let nearest = v.round();
                let cents = ((v - nearest) * 100.0).round() as i32;
                let n = nearest as i64;
                let octave = n.div_euclid(12) - 1;
                write!(out, "{}{octave}", NAMES[n.rem_euclid(12) as usize])?;
                if cents != 0 {
                    write!(out, "{cents:+}")?;
                }
                Ok(())
            }
            DisplayKind::Template => write_template(self.template, v, out),
        }
    }

    /// What the user typed: a value, or `Reject` (the value stays).
    pub fn parse(&self, text: &str) -> ParsedText {
        match self.parse_value(text.trim()) {
            Some(v) => ParsedText::Value(v),
            None => ParsedText::Reject,
        }
    }

    fn parse_value(&self, text: &str) -> Option<f64> {
        match self.kind {
            DisplayKind::Note => return parse_note(text),
            DisplayKind::Template => return parse_template(self.template, text),
            DisplayKind::Db => {
                let lower = text.to_lowercase();
                let t = lower.trim_end_matches("db").trim();
                if matches!(t, "-inf" | "-∞" | "−∞" | "-infinity" | "−inf") {
                    return Some(f64::NEG_INFINITY);
                }
            }
            _ => {}
        }
        let (v, suffix) = split_number(text)?;
        let suffix = suffix.trim().to_lowercase();
        let suffix = suffix.as_str();
        match self.kind {
            DisplayKind::Db => matches!(suffix, "" | "db").then_some(v),
            DisplayKind::Hz => match suffix {
                "" | "hz" => Some(v),
                "k" | "khz" => Some(v * 1000.0),
                _ => None,
            },
            DisplayKind::Percent => matches!(suffix, "" | "%").then_some(v / 100.0),
            DisplayKind::Ms => match suffix {
                "" | "ms" => Some(v),
                "s" | "sec" => Some(v * 1000.0),
                _ => None,
            },
            DisplayKind::Ratio => matches!(suffix, "" | "x" | "×").then_some(v),
            _ => None,
        }
    }
}

/// `v` rounded to `digits` significant digits.
fn round_significant(v: f64, digits: usize) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return v;
    }
    let magnitude = v.abs().log10().floor() as i64 + 1;
    let factor = 10f64.powi((digits as i64 - magnitude).clamp(-300, 300) as i32);
    (v * factor).round() / factor
}

/// Writes `v` with at most `digits` significant digits, without trailing zeros.
fn significant(v: f64, digits: usize, out: &mut dyn Write) -> std::fmt::Result {
    if !v.is_finite() {
        return write!(out, "{v}");
    }
    let magnitude = if v == 0.0 { 0 } else { v.abs().log10().floor() as i64 + 1 };
    let decimals = (digits as i64 - magnitude).clamp(0, 12) as usize;
    let mut buf = StackText::default();
    write!(buf, "{v:.decimals$}")?;
    let s = buf.as_str();
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s };
    // "-0" is just "0"
    out.write_str(if s == "-0" { "0" } else { s })
}

/// A small stack buffer for number text.
struct StackText {
    bytes: [u8; 40],
    len: usize,
}

impl Default for StackText {
    fn default() -> Self {
        StackText { bytes: [0; 40], len: 0 }
    }
}

impl StackText {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl Write for StackText {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let b = s.as_bytes();
        if self.len + b.len() > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        Ok(())
    }
}

/// Splits a leading number (`-1.5e3` style, no thousands separators) from what follows it.
fn split_number(text: &str) -> Option<(f64, &str)> {
    let b = text.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start_digits = i;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    if i == start_digits {
        return None;
    }
    // an exponent only when digits follow (so "2e" is not one)
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let v: f64 = text[..i].parse().ok()?;
    v.is_finite().then(|| (v, &text[i..]))
}

/// `C4`, `c#3+13`, `A#5 + 20`, `Db3 - 5`, or a plain number: a MIDI note number (60 = C4).
fn parse_note(text: &str) -> Option<f64> {
    let t = text.trim();
    if let Some((v, rest)) = split_number(t) {
        if rest.trim().is_empty() {
            return Some(v);
        }
    }
    let mut chars = t.chars().peekable();
    let letter = chars.next()?.to_ascii_uppercase();
    let base = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut semitone = base;
    while let Some(&c) = chars.peek() {
        match c {
            '#' | '♯' => semitone += 1,
            'b' | '♭' => semitone -= 1,
            _ => break,
        }
        chars.next();
    }
    let rest: String = chars.collect();
    let rest = rest.trim();
    // octave: an optional '-' and digits
    let end = rest.char_indices().find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && *c == '-'))).map_or(rest.len(), |(i, _)| i);
    let octave: i64 = rest[..end].parse().ok()?;
    let cents_text = rest[end..].trim();
    let cents = if cents_text.is_empty() {
        0.0
    } else {
        let (sign, digits) = match cents_text.as_bytes()[0] {
            b'+' => (1.0, &cents_text[1..]),
            b'-' => (-1.0, &cents_text[1..]),
            _ => return None,
        };
        let digits = digits.trim().trim_end_matches(['c', 'C']).trim();
        sign * digits.parse::<f64>().ok().filter(|c| c.is_finite())?
    };
    Some(((octave + 1) * 12 + semitone as i64) as f64 + cents / 100.0)
}

/// A piece of a `format = "..."` template.
enum Part<'a> {
    Text(&'a str),
    /// `%.Nf`
    Float(usize),
    /// `%d`
    Int,
}

fn template_parts(template: &str) -> Option<Vec<Part<'_>>> {
    let mut parts = Vec::new();
    let b = template.as_bytes();
    let (mut i, mut start) = (0, 0);
    let mut numbers = 0;
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        if start < i {
            parts.push(Part::Text(&template[start..i]));
        }
        i += 1;
        match b.get(i)? {
            b'%' => parts.push(Part::Text("%")),
            b'd' => {
                parts.push(Part::Int);
                numbers += 1;
            }
            b'.' => {
                let digits_start = i + 1;
                let mut j = digits_start;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == digits_start || b.get(j) != Some(&b'f') {
                    return None;
                }
                parts.push(Part::Float(template[digits_start..j].parse().ok()?));
                numbers += 1;
                i = j;
            }
            b'f' => {
                parts.push(Part::Float(6));
                numbers += 1;
            }
            _ => return None,
        }
        i += 1;
        start = i;
    }
    if start < b.len() {
        parts.push(Part::Text(&template[start..]));
    }
    (numbers == 1).then_some(parts)
}

fn write_template(template: &str, v: f64, out: &mut dyn Write) -> std::fmt::Result {
    let Some(parts) = template_parts(template) else { return write!(out, "{v}") };
    for p in parts {
        match p {
            Part::Text(t) => out.write_str(t)?,
            Part::Float(d) => write!(out, "{v:.d$}")?,
            Part::Int => write!(out, "{}", v.round() as i64)?,
        }
    }
    Ok(())
}

fn parse_template(template: &str, text: &str) -> Option<f64> {
    let parts = template_parts(template)?;
    let mut rest = text.trim();
    // Text before and after the number is optional on input ("-6" is as good as "-6.0 dB").
    for p in &parts {
        if let Part::Text(t) = p {
            let t = t.trim();
            if !t.is_empty() {
                if let Some(r) = rest.strip_prefix(t) {
                    rest = r.trim_start();
                }
            }
            // a unit after the number is handled by the number scan below
            if !t.is_empty() && rest.is_empty() {
                break;
            }
        } else {
            break;
        }
    }
    let (v, suffix) = split_number(rest)?;
    let suffix = suffix.trim();
    let allowed = parts.iter().rev().find_map(|p| if let Part::Text(t) = p { Some(t.trim()) } else { None }).unwrap_or("");
    (suffix.is_empty() || suffix.eq_ignore_ascii_case(allowed)).then_some(v)
}

/// Whether `template` is a valid `format = "..."` (one `%.Nf`, `%f` or `%d`; `%%` for a percent sign).
pub fn template_is_valid(template: &str) -> bool {
    template_parts(template).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(d: Display, v: f64) -> String {
        let mut s = String::new();
        d.format(v, &mut s);
        s
    }

    fn value(d: Display, text: &str) -> Option<f64> {
        match d.parse(text) {
            ParsedText::Value(v) => Some(v),
            _ => None,
        }
    }

    #[test]
    fn decibels_with_the_infinity_floor() {
        let d = Display::db().with_inf_at(-60.0);
        assert_eq!(show(d, -6.0), "-6.0 dB");
        assert_eq!(show(d, 0.0), "0.0 dB");
        assert_eq!(show(d, -60.0), "-INF dB");
        assert_eq!(show(Display::db(), -60.0), "-60.0 dB", "no floor unless asked");
        assert_eq!(show(Display::db().with_digits(2), 3.456), "3.46 dB");
        assert_eq!(value(d, "-6"), Some(-6.0));
        assert_eq!(value(d, "-6.5 dB"), Some(-6.5));
        assert_eq!(value(d, "-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(value(d, "-INF dB"), Some(f64::NEG_INFINITY));
        assert_eq!(value(d, "loud"), None);
    }

    #[test]
    fn frequency_switches_to_khz_with_six_significant_digits() {
        let d = Display::hz();
        assert_eq!(show(d, 440.0), "440 Hz");
        assert_eq!(show(d, 20.5), "20.5 Hz");
        assert_eq!(show(d, 999.0), "999 Hz");
        assert_eq!(show(d, 1000.0), "1 kHz");
        assert_eq!(show(d, 999.999_999_9), "1 kHz", "a value a hair under 1000 (a normalized round trip) shows as 1 kHz");
        assert_eq!(show(d, 1250.0), "1.25 kHz");
        assert_eq!(show(d, 20000.0), "20 kHz");
        assert_eq!(show(d, 12345.678), "12.3457 kHz");
        assert_eq!(show(d.with_digits(3), 12345.678), "12.3 kHz");
        for (text, hz) in [("1k", 1000.0), ("2k", 2000.0), ("1.5kHz", 1500.0), ("1.5 KHZ", 1500.0), ("440", 440.0), ("440 Hz", 440.0), ("0.5k", 500.0)] {
            assert_eq!(value(d, text), Some(hz), "{text}");
        }
        assert_eq!(value(d, "k"), None);
        assert_eq!(value(d, "10 ms"), None);
    }

    #[test]
    fn percent_is_a_fraction() {
        let d = Display::percent();
        assert_eq!(show(d, 0.5), "50%");
        assert_eq!(show(d, 1.0), "100%");
        assert_eq!(show(d, 0.123456), "12.3456%");
        assert_eq!(show(d, 0.0), "0%");
        assert_eq!(value(d, "50%"), Some(0.5));
        assert_eq!(value(d, "50"), Some(0.5), "a bare number is a percentage");
        assert_eq!(value(d, "12.5 %"), Some(0.125));
        assert_eq!(value(d, "half"), None);
    }

    #[test]
    fn milliseconds_switch_to_seconds() {
        let d = Display::ms();
        assert_eq!(show(d, 250.0), "250 ms");
        assert_eq!(show(d, 0.5), "0.5 ms");
        assert_eq!(show(d, 1500.0), "1.5 s");
        assert_eq!(value(d, "10ms"), Some(10.0));
        assert_eq!(value(d, "0.5s"), Some(500.0));
        assert_eq!(value(d, "10"), Some(10.0));
        assert_eq!(value(d, "2 s"), Some(2000.0));
        assert_eq!(value(d, "2 hz"), None);
    }

    #[test]
    fn notes_with_cents() {
        let d = Display::note();
        assert_eq!(show(d, 60.0), "C4");
        assert_eq!(show(d, 69.0), "A4");
        assert_eq!(show(d, 49.13), "C#3+13");
        assert_eq!(show(d, 70.0 - 0.2), "A#4-20");
        assert_eq!(show(d, 12.0), "C0");
        assert_eq!(show(d, 0.0), "C-1");
        for (text, v) in [("C4", 60.0), ("A4", 69.0), ("c#3+13", 49.13), ("A#5 + 20", 82.2), ("Db3", 49.0), ("Bb2 - 5", 45.95), ("E-1", 4.0), ("60", 60.0), ("69.5", 69.5)] {
            let got = value(d, text).unwrap_or(f64::NAN);
            assert!((got - v).abs() < 1e-9, "{text}: {got} vs {v}");
        }
        assert_eq!(value(d, "H4"), None);
        assert_eq!(value(d, "C"), None);
        assert_eq!(value(d, "C4 *3"), None);
    }

    #[test]
    fn ratio_and_templates() {
        let r = Display::ratio();
        assert_eq!(show(r, 2.0), "2x");
        assert_eq!(show(r, 0.5), "0.5x");
        assert_eq!(value(r, "2x"), Some(2.0));
        assert_eq!(value(r, "2"), Some(2.0));
        assert_eq!(value(r, "0.5×"), Some(0.5));

        let t = Display::template("%.1f dB");
        assert_eq!(show(t, -6.0), "-6.0 dB");
        assert_eq!(value(t, "-6.5 dB"), Some(-6.5));
        assert_eq!(value(t, "-6.5"), Some(-6.5));
        assert_eq!(value(t, "-6.5 Hz"), None, "another unit is rejected");
        let p = Display::template("Voices: %d");
        assert_eq!(show(p, 4.0), "Voices: 4");
        assert_eq!(value(p, "Voices: 6"), Some(6.0));
        assert_eq!(value(p, "6"), Some(6.0));
        let pc = Display::template("%d%%");
        assert_eq!(show(pc, 50.0), "50%");
        assert_eq!(value(pc, "50%"), Some(50.0));
        assert!(template_is_valid("%.1f dB") && template_is_valid("%d") && template_is_valid("x%%y %f"));
        assert!(!template_is_valid("no number") && !template_is_valid("%.f") && !template_is_valid("%d %d") && !template_is_valid("%s"));
    }

    #[test]
    fn number_scanning_is_careful() {
        assert_eq!(split_number("2k"), Some((2.0, "k")));
        assert_eq!(split_number("2e3 Hz"), Some((2000.0, " Hz")));
        assert_eq!(split_number("2ms"), Some((2.0, "ms")));
        assert_eq!(split_number("-.5"), Some((-0.5, "")));
        assert_eq!(split_number("abc"), None);
        assert_eq!(split_number("-"), None);
        assert_eq!(split_number("1.2.3"), None);
    }
}
