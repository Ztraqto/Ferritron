//! `[streams]`: numbers the audio thread hands to editors and workers (R3 of the 2.0 plan).
//!
//! ```toml
//! [streams]
//! spectrum = { len = 2048 }                  # a data stream: the latest 2048 values
//! wave     = { len = 16384, kind = "tap" }   # a ring of samples a worker reads
//! ```
//!
//! Keys are snake_case (`ctx.streams().spectrum`); `len` is 1 to 4194304 values.

use super::*;

/// Largest `len` of a stream.
pub const MAX_STREAM_LEN: i64 = 1 << 22;

/// One entry of `[streams]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub key: String,
    /// Values a data stream holds, or the samples a tap's ring holds.
    pub len: usize,
    /// `kind = "tap"`: a ring for a worker; otherwise a data stream for editors.
    pub tap: bool,
}

pub(crate) fn parse_streams(root: &Table) -> Result<Vec<Stream>, Error> {
    let Some(v) = root.get("streams") else { return Ok(Vec::new()) };
    let table = v.as_table().ok_or_else(|| not_table("[streams]"))?;
    let mut out = Vec::new();
    for (key, value) in table {
        let valid = key.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(error!("bad-stream-key", "{$key}: stream keys must be snake_case identifiers (lowercase letters, digits and _)", key = key));
        }
        let at = format!("streams.{key}");
        let t = value.as_table().ok_or_else(|| not_table(&at))?;
        reject_unknown(t, &format!("{at}."), &["len", "kind"])?;
        let len = match t.get("len") {
            None => return Err(required(&format!("{at}.len"))),
            Some(v) => match v.as_integer() {
                Some(n) if (1..=MAX_STREAM_LEN).contains(&n) => n as usize,
                _ => return Err(error!("bad-stream-len", "{$at} must be an integer from 1 to {$max}", at = format!("{at}.len"), max = MAX_STREAM_LEN)),
            },
        };
        let tap = match t.get("kind") {
            None => false,
            Some(v) => match v.as_str() {
                Some("data") => false,
                Some("tap") => true,
                _ => return Err(not_one_of(&format!("{at}.kind"), &v.to_string(), "data, tap")),
            },
        };
        out.push(Stream { key: key.clone(), len, tap });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_toml(streams: &str) -> Result<Vec<Stream>, Error> {
        let root: Table = format!("[streams]\n{streams}").parse().unwrap();
        parse_streams(&root)
    }

    #[test]
    fn data_streams_and_taps_are_parsed_in_key_order() {
        let s = parse_toml("spectrum = { len = 2048 }\nwave = { len = 16384, kind = \"tap\" }").unwrap();
        assert_eq!(s, vec![Stream { key: "spectrum".into(), len: 2048, tap: false }, Stream { key: "wave".into(), len: 16384, tap: true }]);
        assert!(parse_streams(&Table::new()).unwrap().is_empty());
    }

    #[test]
    fn mistakes_are_reported_with_codes() {
        assert_eq!(parse_toml("Spectrum = { len = 4 }").err().unwrap().code, "bad-stream-key");
        assert_eq!(parse_toml("s = { len = 0 }").err().unwrap().code, "bad-stream-len");
        assert_eq!(parse_toml("s = { len = 99999999 }").err().unwrap().code, "bad-stream-len");
        assert_eq!(parse_toml("s = { kind = \"tap\" }").err().unwrap().code, "required");
        assert_eq!(parse_toml("s = { len = 4, kind = \"ring\" }").err().unwrap().code, "not-one-of");
        assert_eq!(parse_toml("s = { len = 4, size = 1 }").err().unwrap().code, "unknown-key");
        assert_eq!(parse_toml("s = 4").err().unwrap().code, "not-table");
    }

    #[test]
    fn the_project_file_parser_reads_streams() {
        let m = parse("[plugin]\nid = \"a.b\"\nname = \"X\"\n[streams]\nspectrum = { len = 8 }").unwrap();
        assert_eq!(m.streams.len(), 1);
        assert_eq!(m.streams[0].key, "spectrum");
        let err = parse("[plugin]\nid = \"a.b\"\nname = \"X\"\n[strems]\nx = 1").err().unwrap();
        assert_eq!(err.code, "unknown-key");
    }
}
