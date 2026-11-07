//! Plugin state serialization, shared by all formats.
//!
//! Layout (little endian): `"FRTN"`, format version `u32`, parameter count
//! `u32`, then `(id: u32, plain value: f64)` per parameter. Parameters are
//! matched by ID, so adding, removing or reordering parameters keeps old
//! projects loadable. Since 1.2 an optional block may follow the entries:
//! the instance's key-value store (`crate::instance::write_store`). Readers
//! before 1.2 stop after the entries, so they still load such state.

use crate::ParamSet;

const MAGIC: [u8; 4] = *b"FRTN";
const VERSION: u32 = 1;

/// Saves the plugin's parameters plus framework-owned `extra` `(id, value)` pairs.
pub fn serialize<P: ParamSet>(params: &P, extra: &[(u32, f64)], store: &std::collections::BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let infos = P::infos();
    let values = infos.iter().enumerate().map(|(index, info)| (info.id, params.cell(index).get()));
    let entries: Vec<(u32, f64)> = values.chain(extra.iter().copied()).collect();
    let mut out = Vec::with_capacity(12 + entries.len() * 12);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (id, value) in entries {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    crate::instance::write_store(&mut out, store);
    out
}

/// The store block after the entries of saved state (empty if none).
pub fn store_of(data: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    if data.len() < 12 || data[..4] != MAGIC {
        return Default::default();
    }
    let count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let end = count.checked_mul(12).and_then(|n| n.checked_add(12));
    match end {
        Some(end) if end <= data.len() => crate::instance::read_store(&data[end..]),
        _ => Default::default(),
    }
}

/// Applies saved state; IDs that are not plugin parameters go to `other`.
/// Returns `false` if the data is not Ferritron state.
pub fn deserialize<P: ParamSet>(params: &P, data: &[u8], mut other: impl FnMut(u32, f64)) -> bool {
    if data.len() < 12 || data[..4] != MAGIC {
        return false;
    }
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    if u32_at(4) != VERSION {
        return false;
    }
    let count = u32_at(8) as usize;
    for entry in data[12..].as_chunks::<12>().0.iter().take(count) {
        let id = u32::from_le_bytes(entry[..4].try_into().unwrap());
        let value = f64::from_le_bytes(entry[4..].try_into().unwrap());
        match P::index_of(id) {
            Some(index) => params.cell(index).set(P::infos()[index].sanitize(value)),
            None => other(id, value),
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FloatParam, ParamCell, ParamInfo, ParamKind};

    struct Two {
        a: FloatParam,
        b: FloatParam,
    }

    static INFOS: [ParamInfo; 2] = [
        ParamInfo { id: 10, key: "a", name: "A", unit: "", kind: ParamKind::Float { min: 0.0, max: 1.0 }, default: 0.0 },
        ParamInfo { id: 20, key: "b", name: "B", unit: "", kind: ParamKind::Float { min: 0.0, max: 1.0 }, default: 0.0 },
    ];

    impl Default for Two {
        fn default() -> Self {
            Self { a: FloatParam::new(0.0), b: FloatParam::new(0.0) }
        }
    }

    impl ParamSet for Two {
        fn infos() -> &'static [ParamInfo] {
            &INFOS
        }
        fn cell(&self, index: usize) -> &ParamCell {
            [self.a.cell(), self.b.cell()][index]
        }
        fn index_of(id: u32) -> Option<usize> {
            INFOS.iter().position(|i| i.id == id)
        }
    }

    #[test]
    fn state_roundtrips_and_ignores_unknown_ids() {
        let src = Two::default();
        src.a.cell().set(0.25);
        src.b.cell().set(0.75);
        let mut data = serialize(&src, &[], &Default::default());
        // Append an entry for a parameter that no longer exists.
        data[8] = 3;
        data.extend_from_slice(&99u32.to_le_bytes());
        data.extend_from_slice(&0.5f64.to_le_bytes());

        let dst = Two::default();
        let mut unknown = Vec::new();
        assert!(deserialize(&dst, &data, |id, v| unknown.push((id, v))));
        assert_eq!(unknown, [(99, 0.5)]);
        assert_eq!(dst.a.value(), 0.25);
        assert_eq!(dst.b.value(), 0.75);
        assert!(!deserialize(&dst, b"garbage", |_, _| {}));
    }

    #[test]
    fn store_block_follows_the_entries() {
        let src = Two::default();
        let mut store = std::collections::BTreeMap::new();
        store.insert("name".to_string(), b"Lead".to_vec());
        let data = serialize(&src, &[(99, 1.0)], &store);
        let dst = Two::default();
        // The entries still read as before (what 1.1 readers do).
        let mut unknown = Vec::new();
        assert!(deserialize(&dst, &data, |id, v| unknown.push((id, v))));
        assert_eq!(unknown, [(99, 1.0)]);
        assert_eq!(store_of(&data), store);
        assert!(store_of(&serialize(&src, &[], &Default::default())).is_empty());
    }
}
