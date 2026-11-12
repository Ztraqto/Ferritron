//! `cargo ferritron bundle --size`: the size of the code (`.text`) in a plugin binary, read from
//! the file's section table (Windows PE, ELF64, Mach-O 64 also inside a fat file, WebAssembly's
//! code section). Smaller code stays in the instruction cache (X13 of the 2.0 plan).

use std::path::Path;

fn u16le(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?) as usize)
}
fn u32le(d: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?) as usize)
}
fn u64le(d: &[u8], at: usize) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?)).ok()
}
fn u32be(d: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// Bytes of code in `path`; `None` when the format is not known.
pub fn text_size(path: &Path) -> Result<Option<usize>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(text_size_of(&data))
}

pub fn text_size_of(d: &[u8]) -> Option<usize> {
    if d.starts_with(b"\0asm") {
        wasm(d)
    } else if d.starts_with(b"\x7fELF") {
        elf64(d)
    } else if d.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
        // fat file: the first architecture (fat_arch: cputype, cpusubtype, offset, size, align; big endian)
        let offset = u32be(d, 16)?;
        macho64(d.get(offset..)?)
    } else if d.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        macho64(d)
    } else {
        pe(d)
    }
}

fn pe(d: &[u8]) -> Option<usize> {
    if !d.starts_with(b"MZ") {
        return None;
    }
    let pe = u32le(d, 0x3c)?;
    if d.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16le(d, pe + 6)?;
    let table = pe + 24 + u16le(d, pe + 20)?;
    let mut total = 0;
    for i in 0..sections {
        let at = table + i * 40;
        if d.get(at..at + 8)?.starts_with(b".text") {
            total += u32le(d, at + 8)?; // VirtualSize
        }
    }
    Some(total)
}

fn elf64(d: &[u8]) -> Option<usize> {
    if *d.get(4)? != 2 || *d.get(5)? != 1 {
        return None; // 64 bit, little endian only
    }
    let (shoff, shentsize, shnum, shstrndx) = (u64le(d, 0x28)?, u16le(d, 0x3a)?, u16le(d, 0x3c)?, u16le(d, 0x3e)?);
    let header = |i: usize| shoff + i * shentsize;
    let strtab = u64le(d, header(shstrndx) + 24)?;
    let mut total = 0;
    for i in 0..shnum {
        let name_at = strtab + u32le(d, header(i))?;
        if d.get(name_at..name_at + 6)? == b".text\0" {
            total += u64le(d, header(i) + 32)?;
        }
    }
    Some(total)
}

fn macho64(d: &[u8]) -> Option<usize> {
    if !d.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        return None;
    }
    let mut at = 32;
    let mut total = 0;
    for _ in 0..u32le(d, 16)? {
        let (cmd, size) = (u32le(d, at)?, u32le(d, at + 4)?);
        if cmd == 0x19 {
            // LC_SEGMENT_64: 72 bytes, then `nsects` sections of 80 bytes
            for s in 0..u32le(d, at + 64)? {
                let section = at + 72 + s * 80;
                if d.get(section..section + 16)?.starts_with(b"__text\0") {
                    total += u64le(d, section + 40)?;
                }
            }
        }
        at += size;
    }
    Some(total)
}

fn wasm(d: &[u8]) -> Option<usize> {
    let leb = |at: &mut usize| -> Option<usize> {
        let (mut v, mut shift) = (0usize, 0);
        loop {
            let b = *d.get(*at)?;
            *at += 1;
            v |= usize::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 56 {
                return None;
            }
        }
    };
    let mut at = 8;
    while at < d.len() {
        let id = *d.get(at)?;
        at += 1;
        let size = leb(&mut at)?;
        if id == 10 {
            return Some(size);
        }
        at += size;
    }
    Some(0)
}

/// `1234567` as `1,234,567 bytes (1.18 MiB)`.
pub fn show(bytes: usize) -> String {
    let digits = bytes.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{grouped} bytes ({:.2} MiB)", bytes as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_numbers() {
        assert_eq!(show(0), "0 bytes (0.00 MiB)");
        assert_eq!(show(999), "999 bytes (0.00 MiB)");
        assert_eq!(show(1_234_567), "1,234,567 bytes (1.18 MiB)");
    }

    #[test]
    fn reads_the_code_section_of_this_very_program() {
        // the running test executable is a PE / ELF / Mach-O file of the current platform
        let exe = std::env::current_exe().unwrap();
        let size = text_size(&exe).unwrap();
        assert!(size.is_some_and(|s| s > 10_000), "{size:?}");
    }

    #[test]
    fn wasm_code_section() {
        // magic, version, a type section (id 1, 1 byte of data) and a code section (id 10, 3 bytes)
        let module = [0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 1, 0, 10, 3, 1, 1, 0x0b];
        assert_eq!(text_size_of(&module), Some(3));
        assert_eq!(text_size_of(b"not a binary"), None);
    }
}
