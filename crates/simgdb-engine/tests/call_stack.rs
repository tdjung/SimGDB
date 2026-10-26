//! Call stacks against gdb backtraces read through QEMU's gdbstub during
//! replay (tests/firmware/traces/*.truth.json.gz). SimGDB derives the stack
//! from the trace flow alone; gdb unwinds stack memory with DWARF CFI, so the
//! two are independent.
//!
//! gdb's view is reproduced from ours: inline frames become frames of their
//! own, a caller's PC is the return address, an exception shows as
//! `<signal handler called>` (function null) whose PC is EXC_RETURN, and the
//! backtrace stops at `main`.
//!
//! Lines follow ARCHITECTURE §6 (as addr2line and tarmac_to_cachegrind): an
//! address with several line-table rows gets the last row. gdb takes the
//! last `is_stmt` row instead, so where they differ both lines must be rows
//! at that address.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;

use serde_json::{Value as Json, json};
use simgdb_engine::Engine;
use simgdb_store::{IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

fn truth(core: &str) -> Json {
    let path = firmware().join(format!("traces/state_{core}.truth.json.gz"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Lines of every line-table row, by address.
fn line_rows(elf: &std::path::Path) -> BTreeMap<u64, BTreeSet<u64>> {
    use addr2line::gimli;
    use object::{Object, ObjectSection};
    let data = std::fs::read(elf).unwrap();
    let file = object::File::parse(&*data).unwrap();
    let load = |id: gimli::SectionId| -> Result<std::borrow::Cow<[u8]>, gimli::Error> {
        Ok(file.section_by_name(id.name()).map(|s| s.uncompressed_data().unwrap()).unwrap_or_default())
    };
    let sections = gimli::DwarfSections::load(load).unwrap();
    let endian = if file.is_little_endian() { gimli::RunTimeEndian::Little } else { gimli::RunTimeEndian::Big };
    let dwarf = sections.borrow(|s| gimli::EndianSlice::new(s, endian));
    let mut out: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next().unwrap() {
        let unit = dwarf.unit(header).unwrap();
        let Some(program) = unit.line_program.clone() else { continue };
        let mut rows = program.rows();
        while let Some((_, row)) = rows.next_row().unwrap() {
            if let Some(line) = row.line() {
                out.entry(row.address()).or_default().insert(line.get());
            }
        }
    }
    out
}

/// Same frames, with lines equal or both rows at the frame's address (for a
/// caller, gdb looks the line up at the return address - 1).
fn same(got: &[Json], want: &[Json], rows: &BTreeMap<u64, BTreeSet<u64>>, insn_at: impl Fn(u64) -> u64) -> bool {
    got.len() == want.len()
        && got.iter().zip(want).enumerate().all(|(k, (g, w))| {
            let strip = |f: &Json| {
                let mut f = f.clone();
                f["line"] = Json::Null;
                f
            };
            if strip(g) != strip(w) {
                return false;
            }
            if g["line"] == w["line"] {
                return true;
            }
            let pc = g["pc"].as_u64().unwrap();
            let caller = k > 0 && !got[k - 1]["function"].is_null() && got[k - 1]["pc"] != g["pc"];
            let addr = if caller { insn_at(pc - 1) } else { pc };
            rows.get(&addr)
                .is_some_and(|l| l.contains(&g["line"].as_u64().unwrap()) && l.contains(&w["line"].as_u64().unwrap()))
        })
}

/// Our call stack at `index` in gdb's backtrace shape.
fn as_gdb(engine: &Engine, index: u64) -> Vec<Json> {
    let lr = engine.register_id("lr").unwrap();
    let stack = engine.call_stack(index).unwrap();
    let mut out = Vec::new();
    let mut pc = stack[0].pc;
    for (k, entry) in stack.iter().enumerate() {
        let source = entry.source.as_ref();
        for f in source.map(|s| s.inline.as_slice()).unwrap_or_default() {
            // gdb prints the name the compile unit gave, here the basename.
            let file = f.file.rsplit('/').next().unwrap();
            out.push(json!({"function": f.function, "file": file, "line": f.line, "pc": pc}));
            if f.function == "main" {
                return out;
            }
        }
        if source.is_none() {
            out.push(json!({"function": null, "file": null, "line": 0, "pc": pc}));
        }
        match entry.kind {
            "exception" => {
                let exc_return = engine.register(entry.entered_at.unwrap(), lr).unwrap().value;
                out.push(json!({"function": null, "file": null, "line": 0, "pc": exc_return}));
                pc = stack.get(k + 1).and_then(|e| e.pc);
            }
            "call" => pc = entry.returns_to,
            _ => {}
        }
    }
    out
}

fn check(core: &str, dialect: &str) -> (usize, usize) {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join(format!("traces/state_{core}.{dialect}.log.gz")),
            elf: Some(firmware().join(format!("state_{core}.elf"))),
            out: dir.path().to_path_buf(),
            checkpoint_interval: 256,
            ..Default::default()
        },
    )
    .unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    let elf = firmware().join(format!("state_{core}.elf"));
    let rows = line_rows(&elf);
    let starts: Vec<u64> = rows.keys().copied().collect();
    let insn_at = |a: u64| starts[starts.partition_point(|&s| s <= a) - 1];
    let mut mismatches = Vec::new();
    let mut line_rule = 0;
    let mut pc_from = BTreeMap::new();
    let mut deepest = 0;
    let points = truth(core)["points"].as_array().unwrap().clone();
    for p in &points {
        let index = p["index"].as_u64().unwrap();
        let mut want: Vec<Json> = p["backtrace"].as_array().unwrap().clone();
        // gdb's unwinder gives up past the reset handler with a bogus frame.
        if want.last().is_some_and(|f| f["function"].is_null() && f["pc"] == 0xffff_ffffu64) {
            want.pop();
        }
        for e in engine.call_stack(index).unwrap() {
            *pc_from.entry(e.pc_from).or_insert(0u32) += 1;
        }
        let got = as_gdb(&engine, index);
        deepest = deepest.max(got.len());
        if got != want && same(&got, &want, &rows, insn_at) {
            line_rule += 1;
        } else if got != want {
            mismatches.push(format!("index {index}:\n  simgdb {got:?}\n  gdb    {want:?}"));
        }
    }
    assert!(mismatches.is_empty(), "{core} {dialect}: {} mismatches\n{}", mismatches.len(), mismatches.join("\n"));
    eprintln!("{core} {dialect}: frame PCs from {pc_from:?}");
    eprintln!("{core} {dialect}: {line_rule} points differ from gdb only by the last-row / last-is_stmt line rule");
    (points.len(), deepest)
}

#[test]
fn call_stacks_match_gdb() {
    for core in ["m4", "m35p", "m4be"] {
        for dialect in ["fm", "m4rtl", "es"] {
            let (points, deepest) = check(core, dialect);
            eprintln!("{core} {dialect}: {points} points, deepest {deepest} frames");
            assert!(deepest >= 5, "exception frames are exercised");
        }
    }
}
