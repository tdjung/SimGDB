//! Source attribution tables, shared by every run of the same ELF.
//!
//! | file | rows | columns |
//! |---|---|---|
//! | `sources.arrow` | one per ELF instruction, sorted by pc | pc, file, function, line, placement, frame_start, frame_count |
//! | `frames.arrow` | inline chains, innermost first | function, file, line |
//! | `strings.arrow` | interned file and function names | s |
//!
//! `variables.json` holds the fixed-address variables and their types (a tree,
//! small, read whole).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use simgdb_elf::source::{Placement, attribute};
use simgdb_elf::variables::Variables;

use crate::StoreError;
use crate::table::{self, Column};

pub const PLACEMENTS: [&str; 5] = ["dwarf", "previous", "next", "call_site", "unresolved"];

fn placement_code(p: Placement) -> u8 {
    match p {
        Placement::Dwarf => 0,
        Placement::Previous => 1,
        Placement::Next => 2,
        Placement::CallSite => 3,
        Placement::Unresolved => 4,
    }
}

/// What source attribution found, for the report.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSummary {
    pub instructions: u64,
    /// Line-0 instructions by how they were placed.
    pub line_zero: BTreeMap<String, u64>,
    /// Fixed-address variables read from DWARF.
    #[serde(default)]
    pub variables: u64,
}

#[derive(Default)]
struct Strings {
    ids: HashMap<String, u32>,
    list: Vec<Vec<u8>>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.ids.get(s) {
            return i;
        }
        let i = self.list.len() as u32;
        self.ids.insert(s.to_string(), i);
        self.list.push(s.as_bytes().to_vec());
        i
    }
}

pub fn write(elf: &Path, out: &Path) -> Result<SourceSummary, StoreError> {
    let attributions = attribute(elf)?;
    let mut strings = Strings::default();
    let (mut pc, mut file, mut function, mut line, mut placement, mut start, mut count) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut f_function, mut f_file, mut f_line) = (vec![], vec![], vec![]);
    let mut summary = SourceSummary { instructions: attributions.len() as u64, ..Default::default() };
    for (addr, a) in &attributions {
        pc.push(*addr);
        file.push(strings.id(&a.file));
        function.push(strings.id(&a.function));
        line.push(a.line);
        placement.push(placement_code(a.placement));
        if a.placement != Placement::Dwarf {
            *summary.line_zero.entry(PLACEMENTS[placement_code(a.placement) as usize].to_string()).or_default() += 1;
        }
        start.push(f_function.len() as u32);
        count.push(a.frames.len() as u32);
        for fr in &a.frames {
            f_function.push(strings.id(&fr.function));
            f_file.push(strings.id(&fr.file));
            f_line.push(fr.line);
        }
    }
    write_tables(out, pc, file, function, line, placement, start, count, f_function, f_file, f_line, strings.list)?;
    let symbols: Vec<SymbolEntry> = simgdb_elf::Elf::load(elf)?
        .symbols
        .into_iter()
        .map(|s| SymbolEntry { name: s.name, addr: s.addr, size: s.size, function: s.is_function })
        .collect();
    write_json(out, "symbols.json", &symbols)?;
    let variables = simgdb_elf::variables::variables(elf)?;
    summary.variables = variables.variables.len() as u64;
    write_variables(out, &variables)?;
    Ok(summary)
}

pub fn write_empty(out: &Path) -> Result<SourceSummary, StoreError> {
    write_tables(out, vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![])?;
    write_variables(out, &Variables::default())?;
    write_json(out, "symbols.json", &Vec::<SymbolEntry>::new())?;
    Ok(SourceSummary::default())
}

/// An ELF symbol (`symbols.json`, sorted by address).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolEntry {
    pub name: String,
    pub addr: u64,
    pub size: u64,
    pub function: bool,
}

fn write_json<T: Serialize>(out: &Path, name: &str, v: &T) -> Result<(), StoreError> {
    let path = out.join(name);
    std::fs::write(&path, serde_json::to_vec(v)?).map_err(|e| StoreError::Io { path: path.clone(), source: e })
}

fn write_variables(out: &Path, v: &Variables) -> Result<(), StoreError> {
    write_json(out, "variables.json", v)
}

pub fn read_symbols(dir: &Path) -> Result<Vec<SymbolEntry>, StoreError> {
    let path = dir.join("symbols.json");
    let bytes = std::fs::read(&path).map_err(|e| StoreError::Io { path: path.clone(), source: e })?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn read_variables(dir: &Path) -> Result<Variables, StoreError> {
    let path = dir.join("variables.json");
    let bytes = std::fs::read(&path).map_err(|e| StoreError::Io { path: path.clone(), source: e })?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[allow(clippy::too_many_arguments)]
fn write_tables(
    out: &Path,
    pc: Vec<u64>,
    file: Vec<u32>,
    function: Vec<u32>,
    line: Vec<u32>,
    placement: Vec<u8>,
    start: Vec<u32>,
    count: Vec<u32>,
    f_function: Vec<u32>,
    f_file: Vec<u32>,
    f_line: Vec<u32>,
    strings: Vec<Vec<u8>>,
) -> Result<(), StoreError> {
    table::write(
        &out.join("sources.arrow"),
        &[
            ("pc", Column::U64(pc)),
            ("file", Column::U32(file)),
            ("function", Column::U32(function)),
            ("line", Column::U32(line)),
            ("placement", Column::U8(placement)),
            ("frame_start", Column::U32(start)),
            ("frame_count", Column::U32(count)),
        ],
    )?;
    table::write(
        &out.join("frames.arrow"),
        &[("function", Column::U32(f_function)), ("file", Column::U32(f_file)), ("line", Column::U32(f_line))],
    )?;
    table::write(&out.join("strings.arrow"), &[("s", Column::Binary(strings))])
}
