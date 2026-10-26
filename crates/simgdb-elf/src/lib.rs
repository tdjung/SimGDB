//! ELF access: the memory image the program starts from, and symbols.
//!
//! The initial image is each `PT_LOAD` segment's file bytes placed at its
//! *load* address (`p_paddr`). That is what is in memory at reset: code and
//! read-only data in place, and the initial values of `.data` in flash. The
//! RAM copy of `.data` (`p_vaddr`) only exists once startup code copies it,
//! and those stores are in the trace, so it is deliberately not part of the
//! image. `.bss` has no file bytes and is not part of it either.

pub mod source;

use std::path::Path;

use object::{Object, ObjectSegment, ObjectSymbol, SymbolKind};

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
        let mut symbols: Vec<Symbol> = file
            .symbols()
            .filter(|s| matches!(s.kind(), SymbolKind::Text | SymbolKind::Data) && !s.name().unwrap_or("").is_empty())
            .map(|s| Symbol {
                name: s.name().unwrap_or("").to_string(),
                // Thumb function symbols carry bit 0.
                addr: if s.kind() == SymbolKind::Text { s.address() & !1 } else { s.address() },
                size: s.size(),
                is_function: s.kind() == SymbolKind::Text,
            })
            .collect();
        symbols.sort_by_key(|s| s.addr);
        Ok(Elf { image, symbols, entry: file.entry() })
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
