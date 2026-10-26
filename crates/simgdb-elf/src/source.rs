//! Source attribution (ARCHITECTURE §6), the same rules as tarmac_to_cachegrind
//! with independent code:
//!
//! - Every instruction of the ELF code is resolved through DWARF with its
//!   inline chain (like `addr2line -a -f -i -C`): file and line come from the
//!   innermost frame, the function from the outermost one.
//! - Instructions the compiler gave line 0 are placed: (1) previous: the
//!   instruction before falls through into this one and is in the same
//!   (inlined) function; (2) next: the next instruction of this straight-line
//!   run in the same function; (3) call_site: inlined code takes the line
//!   that called it; (4) unresolved: line 0 stays.
//! - The instruction listing comes from the ELF code itself: `$t`/`$d`
//!   mapping symbols separate code from data, other symbols separate
//!   neighbours, and the ISA classifier says which instructions end flow.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use object::{Object, ObjectSection, ObjectSymbol, SectionFlags, SymbolKind};
use serde::Serialize;
use simgdb_isa::thumb;

use crate::ElfError;

pub const UNKNOWN: &str = "???";
const RUN_LIMIT: usize = 256;

/// One frame of the inline chain at an address, innermost first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Frame {
    pub function: String,
    pub file: String,
    pub line: u32,
}

/// How a line-0 instruction got its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// DWARF gives the line directly.
    Dwarf,
    Previous,
    Next,
    CallSite,
    /// Line 0 kept.
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attribution {
    pub file: String,
    /// Outermost function: the one the code physically lives in.
    pub function: String,
    pub line: u32,
    pub placement: Placement,
    /// Inline chain, innermost first (one frame when nothing is inlined).
    pub frames: Vec<Frame>,
}

impl Attribution {
    fn needs_line(&self) -> bool {
        self.line == 0 || self.file == UNKNOWN
    }

    /// Innermost function name (the inlined one, or the containing one).
    pub fn inner(&self) -> &str {
        self.frames.first().map(|f| f.function.as_str()).unwrap_or(UNKNOWN)
    }
}

/// Instruction order of the ELF code: neighbours within one symbol and
/// whether each instruction can continue into the next.
#[derive(Default)]
pub struct Listing {
    pub pcs: Vec<u64>,
    prev: HashMap<u64, u64>,
    next: HashMap<u64, u64>,
    falls: HashMap<u64, bool>,
}

impl Listing {
    pub fn falls(&self, pc: u64) -> bool {
        self.falls.get(&pc).copied().unwrap_or(false)
    }
}

fn known(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() && n != "??" => n.to_string(),
        _ => UNKNOWN.to_string(),
    }
}

/// Build the Thumb listing of an ELF image.
pub fn listing(bytes: &[u8]) -> Result<Listing, ElfError> {
    let file = object::File::parse(bytes)?;
    let mut out = Listing::default();
    for section in file.sections() {
        let exec = match section.flags() {
            SectionFlags::Elf { sh_flags } => sh_flags & object::elf::SHF_EXECINSTR as u64 != 0,
            _ => false,
        };
        if !exec {
            continue;
        }
        let (start, data) = (section.address(), section.data()?);
        let end = start + data.len() as u64;
        // Mapping symbols mark code ($t, $a) and data ($d); other symbols
        // start a new neighbour chain, as objdump's symbol headers do.
        let mut marks: BTreeMap<u64, bool> = BTreeMap::new();
        let mut breaks: Vec<u64> = Vec::new();
        for sym in file.symbols().filter(|s| s.section_index() == Some(section.index())) {
            let name = sym.name().unwrap_or("");
            let addr = sym.address() & !1;
            if addr < start || addr >= end {
                continue;
            }
            if name == "$t" || name.starts_with("$t.") || name == "$a" || name.starts_with("$a.") {
                marks.insert(addr, true);
            } else if name == "$d" || name.starts_with("$d.") {
                marks.insert(addr, false);
            } else if !name.is_empty() && !matches!(sym.kind(), SymbolKind::Section | SymbolKind::File) {
                breaks.push(addr);
            }
        }
        if marks.is_empty() {
            marks.insert(start, true);
        }
        breaks.sort_unstable();
        let mut regions: Vec<(u64, u64)> = Vec::new();
        let points: Vec<(u64, bool)> = marks.into_iter().collect();
        for (k, &(addr, code)) in points.iter().enumerate() {
            let stop = points.get(k + 1).map(|p| p.0).unwrap_or(end);
            if code && stop > addr {
                regions.push((addr, stop));
            }
        }
        for (lo, hi) in regions {
            // Also restart the sweep (and IT state) at symbol boundaries.
            let mut cuts: Vec<u64> = breaks.iter().copied().filter(|&b| b > lo && b < hi).collect();
            cuts.insert(0, lo);
            cuts.push(hi);
            for w in cuts.windows(2) {
                let code = &data[(w[0] - start) as usize..(w[1] - start) as usize];
                let insns = thumb::sweep(code, w[0]);
                let mut last: Option<u64> = None;
                for i in insns {
                    if let Some(p) = last {
                        out.prev.insert(i.addr, p);
                        out.next.insert(p, i.addr);
                    }
                    out.falls.insert(i.addr, !i.flow.ends_flow());
                    out.pcs.push(i.addr);
                    last = Some(i.addr);
                }
            }
        }
    }
    out.pcs.sort_unstable();
    out.pcs.dedup();
    Ok(out)
}

/// Attribute every instruction of the ELF code.
pub fn attribute(path: &Path) -> Result<BTreeMap<u64, Attribution>, ElfError> {
    let bytes = std::fs::read(path).map_err(|e| ElfError::Io(path.display().to_string(), e))?;
    let listing = listing(&bytes)?;
    let file = object::File::parse(&*bytes)?;
    let mut functions: Vec<(u64, u64, String)> = file
        .symbols()
        .filter(|s| s.kind() == SymbolKind::Text && !s.name().unwrap_or("").is_empty())
        .map(|s| (s.address() & !1, s.size(), s.name().unwrap_or("").to_string()))
        .collect();
    functions.sort();
    let loader = addr2line::Loader::new(path).map_err(|e| ElfError::Dwarf(e.to_string()))?;
    let mut sources = BTreeMap::new();
    for &pc in &listing.pcs {
        let mut frames = Vec::new();
        if let Ok(mut iter) = loader.find_frames(pc) {
            while let Ok(Some(f)) = iter.next() {
                let function = f.function.as_ref().and_then(|n| n.demangle().ok()).map(|n| n.into_owned());
                let (file, line) = match &f.location {
                    Some(l) => (known(l.file), l.line.unwrap_or(0)),
                    None => (UNKNOWN.to_string(), 0),
                };
                frames.push(Frame { function: known(function.as_deref()), file, line });
            }
        }
        if frames.is_empty() {
            frames.push(Frame { function: UNKNOWN.into(), file: UNKNOWN.into(), line: 0 });
        }
        // No DWARF function: the symbol containing the address (as
        // addr2line falls back to the symbol table).
        if frames.last().unwrap().function == UNKNOWN {
            if let Some(name) = containing(&functions, pc) {
                frames.last_mut().unwrap().function = name.to_string();
                if frames.len() == 1 {
                    frames[0].function = name.to_string();
                }
            }
        }
        let first = &frames[0];
        sources.insert(
            pc,
            Attribution {
                file: first.file.clone(),
                function: frames.last().unwrap().function.clone(),
                line: first.line,
                placement: Placement::Dwarf,
                frames,
            },
        );
    }
    place_line_zero(&mut sources, &listing);
    Ok(sources)
}

/// The nearest function symbol at or below `pc`, as binutils addr2line
/// names code without a DWARF function (assembly, padding): symbol sizes are
/// not required to cover the address.
fn containing(functions: &[(u64, u64, String)], pc: u64) -> Option<&str> {
    let i = functions.partition_point(|f| f.0 <= pc);
    functions[..i].last().map(|f| f.2.as_str())
}

/// Give line-0 instructions a line, in ascending address order so that a run
/// of line-0 instructions inherits through itself.
pub fn place_line_zero(sources: &mut BTreeMap<u64, Attribution>, listing: &Listing) {
    let pending: Vec<u64> = sources.iter().filter(|(_, a)| a.needs_line()).map(|(pc, _)| *pc).collect();
    let same_function = |s: &BTreeMap<u64, Attribution>, a: u64, b: u64| {
        let (x, y) = (s[&a].inner(), s[&b].inner());
        x == UNKNOWN || y == UNKNOWN || x == y
    };
    for pc in pending {
        let mut placed: Option<(String, u32, String, Placement)> = None;
        if let Some(&prev) = listing.prev.get(&pc) {
            if listing.falls(prev)
                && sources.contains_key(&prev)
                && same_function(sources, prev, pc)
                && !sources[&prev].needs_line()
            {
                let p = &sources[&prev];
                placed = Some((p.file.clone(), p.line, p.function.clone(), Placement::Previous));
            }
        }
        if placed.is_none() {
            let mut cur = pc;
            for _ in 0..RUN_LIMIT {
                let Some(&nxt) = listing.next.get(&cur) else { break };
                if !listing.falls(cur) || !sources.contains_key(&nxt) || !same_function(sources, nxt, pc) {
                    break;
                }
                if !sources[&nxt].needs_line() {
                    let n = &sources[&nxt];
                    placed = Some((n.file.clone(), n.line, n.function.clone(), Placement::Next));
                    break;
                }
                cur = nxt;
            }
        }
        if placed.is_none() {
            let a = &sources[&pc];
            if a.frames.len() > 1 {
                let call = &a.frames[1];
                if call.line > 0 && call.file != UNKNOWN {
                    placed = Some((call.file.clone(), call.line, UNKNOWN.to_string(), Placement::CallSite));
                }
            }
        }
        let a = sources.get_mut(&pc).unwrap();
        match placed {
            Some((file, line, function, rule)) => {
                a.file = file;
                a.line = line;
                if a.function == UNKNOWN {
                    a.function = function;
                }
                a.placement = rule;
            }
            None => a.placement = Placement::Unresolved,
        }
    }
}
