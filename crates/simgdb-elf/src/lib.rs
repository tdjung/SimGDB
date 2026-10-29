//! ELF access: the memory image the program starts from, and symbols.
//!
//! The initial image is each `PT_LOAD` segment's file bytes placed at its
//! *load* address (`p_paddr`). That is what is in memory at reset: code and
//! read-only data in place, and the initial values of `.data` in flash. The
//! RAM copy of `.data` (`p_vaddr`) only exists once startup code copies it,
//! and those stores are in the trace, so it is deliberately not part of the
//! image. `.bss` has no file bytes and is not part of it either.

pub mod source;
pub mod variables;

use std::path::Path;

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SectionKind, SymbolKind};

#[derive(Debug, thiserror::Error)]
pub enum ElfError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("cannot parse ELF: {0}")]
    Parse(#[from] object::Error),
    #[error("malformed ELF: {0}")]
    Malformed(&'static str),
    #[error("cannot read debug information: {0}")]
    Dwarf(String),
}

#[derive(Clone, Debug)]
pub struct Segment {
    pub addr: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub addr: u64,
    pub size: u64,
    pub is_function: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Elf {
    /// Sorted by address, non-overlapping.
    pub image: Vec<Segment>,
    /// Sorted by address.
    pub symbols: Vec<Symbol>,
    pub entry: u64,
    /// The ELF header's data encoding (ELFDATA2MSB: big-endian data, Arm BE8).
    pub big_endian: bool,
    /// Arm `Tag_CPU_arch` from the `.ARM.attributes` build attributes
    /// (13: Armv7E-M, 17: Armv8-M Mainline …), when present.
    pub arm_cpu_arch: Option<u32>,
}

impl Elf {
    pub fn load(path: &Path) -> Result<Elf, ElfError> {
        let bytes = std::fs::read(path).map_err(|e| ElfError::Io(path.display().to_string(), e))?;
        Elf::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Elf, ElfError> {
        let file = object::File::parse(bytes)?;
        let mut image = Vec::new();
        if let object::File::Elf32(elf) = &file {
            use object::read::elf::ProgramHeader;
            let endian = elf.endian();
            for ph in elf.elf_program_headers() {
                if ph.p_type(endian) != object::elf::PT_LOAD {
                    continue;
                }
                let data = ph.data(endian, bytes).map_err(|_| ElfError::Malformed("segment data out of range"))?;
                if !data.is_empty() {
                    image.push(Segment { addr: ph.p_paddr(endian) as u64, data: data.to_vec() });
                }
            }
        } else {
            // 64-bit or other: fall back to segment virtual addresses.
            for seg in file.segments() {
                let data = seg.data()?;
                if !data.is_empty() {
                    image.push(Segment { addr: seg.address(), data: data.to_vec() });
                }
            }
        }
        image.sort_by_key(|s| s.addr);
        // Assembly labels often have no type (STT_NOTYPE): in code they are
        // functions too. Arm mapping symbols (`$t`, `$d`) are not names.
        let in_code = |s: &object::Symbol<'_, '_>| {
            s.section_index()
                .and_then(|i| file.section_by_index(i).ok())
                .is_some_and(|sec| sec.kind() == SectionKind::Text)
        };
        let mut symbols: Vec<Symbol> = file
            .symbols()
            .filter(|s| {
                let name = s.name().unwrap_or("");
                !name.is_empty()
                    && !name.starts_with('$')
                    && (matches!(s.kind(), SymbolKind::Text | SymbolKind::Data)
                        || (s.kind() == SymbolKind::Unknown && s.is_global() && in_code(s)))
            })
            .map(|s| {
                let code = s.kind() != SymbolKind::Data;
                Symbol {
                    name: s.name().unwrap_or("").to_string(),
                    // Thumb function symbols carry bit 0.
                    addr: if code { s.address() & !1 } else { s.address() },
                    size: s.size(),
                    is_function: code,
                }
            })
            .collect();
        symbols.sort_by_key(|s| s.addr);
        let arm_cpu_arch = file
            .section_by_name(".ARM.attributes")
            .and_then(|s| s.data().ok())
            .and_then(|d| arm_cpu_arch(d, file.is_little_endian()));
        Ok(Elf { image, symbols, entry: file.entry(), big_endian: !file.is_little_endian(), arm_cpu_arch })
    }

    /// Initial byte at `addr`, if the load image has one.
    pub fn initial_byte(&self, addr: u64) -> Option<u8> {
        let i = self.image.partition_point(|s| s.addr <= addr);
        let seg = self.image.get(i.checked_sub(1)?)?;
        seg.data.get((addr - seg.addr) as usize).copied()
    }

    pub fn symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// Function containing `addr`.
    pub fn function_at(&self, addr: u64) -> Option<&Symbol> {
        let i = self.symbols.partition_point(|s| s.addr <= addr);
        self.symbols[..i].iter().rev().find(|s| s.is_function && addr < s.addr + s.size.max(1))
    }
}

/// `Tag_CPU_arch` (tag 6) of the "aeabi" build attributes: format version
/// 'A', then subsections (length, vendor name, then tagged sub-subsections,
/// tag 1 = the whole file) of ULEB128-tagged attributes (ARM IHI 0045).
fn arm_cpu_arch(d: &[u8], little: bool) -> Option<u32> {
    fn uleb(d: &[u8], i: &mut usize) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = *d.get(*i)?;
            *i += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    let word = |b: &[u8]| -> Option<usize> {
        let w: [u8; 4] = b.get(..4)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(w) } else { u32::from_be_bytes(w) } as usize)
    };
    if d.first() != Some(&b'A') {
        return None;
    }
    let mut pos = 1;
    while pos + 4 <= d.len() {
        let len = word(&d[pos..])?;
        let end = pos.checked_add(len).filter(|&e| e <= d.len() && len > 4)?;
        let mut i = pos + 4;
        let name_end = i + d[i..end].iter().position(|&b| b == 0)?;
        let vendor = &d[i..name_end];
        i = name_end + 1;
        while vendor == b"aeabi" && i < end {
            let tag = uleb(d, &mut i)?;
            let size = word(d.get(i..)?)?;
            let sub_end = (i - 1).checked_add(size).filter(|&e| e <= end)?;
            i += 4;
            if tag != 1 {
                i = sub_end;
                continue;
            }
            while i < sub_end {
                let attr = uleb(d, &mut i)?;
                // Values: strings for tags 4, 5, 67 and odd tags above 32;
                // tag 32 is a number then a string; the rest numbers.
                let string = matches!(attr, 4 | 5 | 67) || (attr > 32 && attr % 2 == 1);
                if string {
                    i += d[i..sub_end].iter().position(|&b| b == 0)? + 1;
                    continue;
                }
                let v = uleb(d, &mut i)?;
                if attr == 6 {
                    return Some(v as u32);
                }
                if attr == 32 {
                    i += d[i..sub_end].iter().position(|&b| b == 0)? + 1;
                }
            }
        }
        pos = end;
    }
    None
}

#[cfg(test)]
mod attribute_tests {
    #[test]
    fn cpu_arch_of_the_test_firmware() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware");
        // arm-none-eabi-readelf -A: Tag_CPU_arch v7E-M (13), v8-M.mainline (17).
        for (elf, arch) in [("state_m4.elf", 13), ("state_m4be.elf", 13), ("state_m4f.elf", 13), ("state_m35p.elf", 17)]
        {
            assert_eq!(super::Elf::load(&dir.join(elf)).unwrap().arm_cpu_arch, Some(arch), "{elf}");
        }
    }
}
