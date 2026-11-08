//! Reads the exported symbol names of a plugin library (Windows PE/COFF, ELF
//! on Linux, Mach-O on macOS), so bundling can tell which plugin formats a
//! library was built with without loading it.

use std::path::Path;

pub fn exported_names(path: &Path) -> Result<Vec<String>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let names = if data.starts_with(b"\0asm") {
        parse_wasm(&data)
    } else if data.starts_with(b"\x7fELF") {
        parse_elf(&data)
    } else if data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || data.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
        parse_macho(&data)
    } else {
        parse(&data)
    };
    names.ok_or_else(|| format!("{} is not a valid plugin library", path.display()))
}

/// A LEB128 unsigned number at `*at`, advancing it.
fn leb(d: &[u8], at: &mut usize) -> Option<u64> {
    let (mut value, mut shift) = (0u64, 0);
    loop {
        let b = *d.get(*at)?;
        *at += 1;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Exported function names of a WebAssembly module (its export section).
fn parse_wasm(d: &[u8]) -> Option<Vec<String>> {
    const EXPORT_SECTION: u8 = 7;
    const FUNCTION: u8 = 0;
    let mut at = 8; // magic and version
    let mut names = Vec::new();
    while at < d.len() {
        let id = *d.get(at)?;
        at += 1;
        let size = leb(d, &mut at)? as usize;
        let end = at.checked_add(size)?;
        if id == EXPORT_SECTION {
            let mut p = at;
            for _ in 0..leb(d, &mut p)? {
                let len = leb(d, &mut p)? as usize;
                let name = std::str::from_utf8(d.get(p..p + len)?).ok()?.to_string();
                p += len;
                let kind = *d.get(p)?;
                p += 1;
                leb(d, &mut p)?;
                if kind == FUNCTION {
                    names.push(name);
                }
            }
        }
        at = end;
    }
    Some(names)
}

fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// Defined global symbols of the dynamic symbol table (64-bit, little-endian ELF).
fn parse_elf(d: &[u8]) -> Option<Vec<String>> {
    const ELFCLASS64: u8 = 2;
    const SHT_DYNSYM: u32 = 11;
    if *d.get(4)? != ELFCLASS64 || *d.get(5)? != 1 {
        return None;
    }
    let (shoff, shentsize, shnum) = (u64_at(d, 0x28)? as usize, u16_at(d, 0x3a)? as usize, u16_at(d, 0x3c)? as usize);
    let section = |i: usize| shoff + i * shentsize;
    let dynsym = (0..shnum).map(section).find(|&h| u32_at(d, h + 4) == Some(SHT_DYNSYM))?;
    let strtab = section(u32_at(d, dynsym + 0x28)? as usize);
    let (strings, symbols) = (u64_at(d, strtab + 0x18)? as usize, u64_at(d, dynsym + 0x18)? as usize);
    let (size, entry) = (u64_at(d, dynsym + 0x20)? as usize, u64_at(d, dynsym + 0x38)?.max(1) as usize);
    let mut names = Vec::new();
    for sym in (symbols..symbols + size).step_by(entry) {
        let (name, info, shndx) = (u32_at(d, sym)? as usize, *d.get(sym + 4)?, u16_at(d, sym + 6)?);
        // Defined (section index not 0), global or weak.
        if shndx != 0 && matches!(info >> 4, 1 | 2) {
            let start = strings + name;
            let end = start + d.get(start..)?.iter().position(|&b| b == 0)?;
            names.push(String::from_utf8_lossy(&d[start..end]).into_owned());
        }
    }
    Some(names)
}

fn u32_be(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// External symbols defined in a section, from the symbol table of a 64-bit
/// Mach-O image (the first one of a universal binary). C names carry a
/// leading underscore there; it is removed.
fn parse_macho(d: &[u8]) -> Option<Vec<String>> {
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SYMTAB: u32 = 0x2;
    const N_EXT: u8 = 0x01;
    const N_TYPE: u8 = 0x0e;
    const N_SECT: u8 = 0x0e;
    let base = if u32_be(d, 0)? == FAT_MAGIC {
        // fat_header, then fat_arch { cputype, cpusubtype, offset, size, align }
        if u32_be(d, 4)? == 0 {
            return None;
        }
        u32_be(d, 8 + 8)? as usize
    } else {
        0
    };
    let image = d.get(base..)?;
    if u32_at(image, 0)? != MH_MAGIC_64 {
        return None;
    }
    let commands = u32_at(image, 16)? as usize;
    let mut at = 32; // mach_header_64
    for _ in 0..commands {
        let (cmd, size) = (u32_at(image, at)?, u32_at(image, at + 4)? as usize);
        if cmd == LC_SYMTAB {
            let (symoff, nsyms, stroff) = (u32_at(image, at + 8)? as usize, u32_at(image, at + 12)? as usize, u32_at(image, at + 16)? as usize);
            let mut names = Vec::new();
            for i in 0..nsyms {
                let sym = symoff + i * 16; // nlist_64
                let (strx, kind) = (u32_at(image, sym)? as usize, *image.get(sym + 4)?);
                if kind & N_EXT != 0 && kind & N_TYPE == N_SECT {
                    let start = stroff + strx;
                    let end = start + image.get(start..)?.iter().position(|&b| b == 0)?;
                    let name = String::from_utf8_lossy(&image[start..end]);
                    names.push(name.strip_prefix('_').unwrap_or(&name).to_string());
                }
            }
            return Some(names);
        }
        at += size.max(8);
    }
    Some(Vec::new())
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn parse(d: &[u8]) -> Option<Vec<String>> {
    if d.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(d, 0x3c)? as usize;
    if d.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(d, coff + 2)? as usize;
    let optional_size = u16_at(d, coff + 16)? as usize;
    let optional = coff + 20;
    // Data directories start at 96 (PE32) or 112 (PE32+); entry 0 is the export table.
    let dirs = match u16_at(d, optional)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    let export_rva = u32_at(d, dirs)?;
    if export_rva == 0 {
        return Some(Vec::new());
    }

    let section_table = optional + optional_size;
    let to_offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = section_table + i * 40;
            let (size, va, raw) = (u32_at(d, s + 8)?, u32_at(d, s + 12)?, u32_at(d, s + 20)?);
            (rva >= va && rva < va + size.max(1)).then(|| (rva - va + raw) as usize)
        })
    };

    let dir = to_offset(export_rva)?;
    let count = u32_at(d, dir + 24)? as usize;
    let names = to_offset(u32_at(d, dir + 32)?)?;
    (0..count)
        .map(|i| {
            let start = to_offset(u32_at(d, names + i * 4)?)?;
            let len = d.get(start..)?.iter().position(|&b| b == 0)?;
            Some(String::from_utf8_lossy(&d[start..start + len]).into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_pe_files() {
        assert!(parse(b"not a dll").is_none());
        assert!(parse(&[]).is_none());
    }
    /// A minimal Mach-O image: header, one LC_SYMTAB, two symbols.
    fn macho() -> Vec<u8> {
        let mut d = vec![0u8; 32];
        d[..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        d[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds
        let symoff = 32 + 24;
        let stroff = symoff + 2 * 16;
        for v in [0x2u32, 24, symoff as u32, 2, stroff as u32, 30] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let symbol = |strx: u32, kind: u8| {
            let mut s = strx.to_le_bytes().to_vec();
            s.extend_from_slice(&[kind, 1, 0, 0]);
            s.extend_from_slice(&0u64.to_le_bytes());
            s
        };
        d.extend(symbol(1, 0x0f)); // external, defined
        d.extend(symbol(13, 0x01)); // external, undefined
        d.extend_from_slice(b"\0_clap_entry\0_malloc\0");
        d
    }

    #[test]
    fn reads_mach_o_exports() {
        assert_eq!(parse_macho(&macho()), Some(vec!["clap_entry".to_string()]));
        // A universal binary whose first slice is the image.
        let image = macho();
        let mut fat = Vec::new();
        for v in [0xcafe_babeu32, 1, 0x0100_000c, 0, 4096, image.len() as u32, 12] {
            fat.extend_from_slice(&v.to_be_bytes());
        }
        fat.resize(4096, 0);
        fat.extend(image);
        assert_eq!(parse_macho(&fat), Some(vec!["clap_entry".to_string()]));
    }

}
