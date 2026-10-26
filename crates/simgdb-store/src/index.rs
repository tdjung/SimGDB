//! Build a dataset from a trace (the Runway).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use simgdb_elf::Elf;
use simgdb_isa::{Isa, RegState, RegWrite, TraceRegName};
use simgdb_tarmac::{Event, Parser, Record};

use crate::calls::Calls;
use crate::table::{Column, write};
use crate::{
    Capabilities, FLAG_FAILED, FLAG_FETCH_FAILED, FORMAT_VERSION, MEM_CLOBBER, MEM_FOREIGN, MEM_READ, MEM_WRITE, Meta,
    NONE, Report, Sample, StoreError, Unparsed,
};

pub struct IndexOptions {
    pub trace: PathBuf,
    pub elf: Option<PathBuf>,
    pub out: PathBuf,
    /// Core to index when the trace has `cpuN` prefixes (default: the first seen).
    pub core: Option<u32>,
    pub checkpoint_interval: u64,
    /// Replace hex digits in report samples (for sharing from a closed network).
    pub mask_samples: bool,
    /// Fail on the first line that does not parse.
    pub strict: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            trace: PathBuf::new(),
            elf: None,
            out: PathBuf::new(),
            core: None,
            checkpoint_interval: 4096,
            mask_samples: false,
            strict: false,
        }
    }
}

const SAMPLES: usize = 5;

#[derive(Default)]
struct Builder {
    // instructions
    pc: Vec<u64>,
    opcode: Vec<u32>,
    width: Vec<u8>,
    flags: Vec<u8>,
    time: Vec<u64>,
    line: Vec<u64>,
    // register writes
    rw_at: Vec<u64>,
    rw_reg: Vec<u16>,
    rw_value: Vec<u64>,
    rw_known: Vec<u64>,
    rw_line: Vec<u64>,
    rw_derived: Vec<u8>,
    // memory
    m_at: Vec<u64>,
    m_addr: Vec<u64>,
    m_size: Vec<u32>,
    m_kind: Vec<u8>,
    m_value: Vec<u64>,
    m_known: Vec<u8>,
    m_line: Vec<u64>,
    // checkpoints
    cp_index: Vec<u64>,
    cp_rw_row: Vec<u64>,
    cp_v: Vec<Vec<u64>>,
    cp_k: Vec<Vec<u64>>,
    cp_w: Vec<Vec<u64>>,
    // exceptions
    e_at: Vec<u64>,
    e_number: Vec<u32>,
    e_kind: Vec<u8>,
    e_target: Vec<u64>,
    e_line: Vec<u64>,
    // innermost call frame at each point, and the call table
    frame: Vec<u32>,
    calls: Calls,
}

/// An exception entry whose post-entry stack pointer is not known yet: its
/// placeholder clobber row is filled in when the next instruction starts.
struct PendingEntry {
    row: usize,
    /// The range below the pre-entry stack pointer was marked.
    placed_below: bool,
}

pub fn build(isa: &dyn Isa, opts: &IndexOptions) -> Result<Meta, StoreError> {
    let elf = match &opts.elf {
        Some(p) => Some(Elf::load(p)?),
        None => None,
    };
    if opts.checkpoint_interval == 0 {
        return Err(StoreError::Trace("checkpoint interval must be at least 1".into()));
    }
    std::fs::create_dir_all(&opts.out).map_err(|e| StoreError::io(&opts.out, e))?;
    let reader = simgdb_tarmac::open(&opts.trace).map_err(|e| StoreError::io(&opts.trace, e))?;
    let nregs = isa.registers().len();
    let mut b = Builder {
        cp_v: vec![vec![]; nregs],
        cp_k: vec![vec![]; nregs],
        cp_w: vec![vec![]; nregs],
        ..Default::default()
    };
    let mut report = Report { time_monotonic: true, ..Default::default() };
    let mut state = RegState::new(nregs);
    let mut last_write_row = vec![NONE; nregs];
    let mut parser = Parser::new();
    let mut core = opts.core;
    let mut current_core: Option<u32> = None;
    let mut pending: Option<PendingEntry> = None;
    let mut stacking_seen = false;
    let mut writes_since_entry = false;
    let mut block = Block { written: vec![false; nregs], line: 0 };
    let frame_max = isa.exception_frame_max();
    let sp_bits = isa.registers()[isa.stack_pointer() as usize].bits;
    let limit = isa.address_limit();
    let sp = isa.stack_pointer();
    let mask = |s: &str| if opts.mask_samples { mask_hex(s) } else { s.to_string() };

    for (n, line) in reader.lines().enumerate() {
        let lineno = n as u64 + 1;
        let text = line.map_err(|e| StoreError::io(&opts.trace, e))?;
        report.lines += 1;
        let parsed = match parser.parse(&text) {
            Ok(p) => p,
            Err(e) => {
                if opts.strict {
                    return Err(StoreError::Trace(format!("{}:{lineno}: {e}: {text}", opts.trace.display())));
                }
                report.parse_errors.count += 1;
                if report.parse_errors.samples.len() < SAMPLES {
                    report.parse_errors.samples.push(Sample { line: lineno, text: format!("{e}: {}", mask(&text)) });
                }
                continue;
            }
        };
        if let Some(c) = parsed.core {
            current_core = Some(c);
            if core.is_none() {
                core = Some(c);
            }
        }
        if let (Some(c), Some(want)) = (current_core, core)
            && c != want
        {
            *report.other_cores.entry(c).or_default() += 1;
            // Another core's stores change memory this core sees: those
            // bytes become unknown from here on.
            if let Record::Memory(accesses) = &parsed.record {
                let now = b.pc.len() as u64;
                for a in accesses.iter().filter(|a| a.write) {
                    if a.addr.checked_add(a.size as u64).is_some_and(|end| end <= limit) {
                        report.other_core_writes += 1;
                        mark_unknown(&mut b, MEM_FOREIGN, now, a.addr, a.size as u64, lineno);
                    }
                }
            }
            continue;
        }
        let at = b.pc.len() as u64;
        match parsed.record {
            Record::Instruction(i) => {
                if i.event == Event::If && b.pc.last() == Some(&i.pc) {
                    report.if_duplicates += 1;
                    continue;
                }
                report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at);
                if let Some(p) = pending.take() {
                    report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa) as u64;
                }
                if at.is_multiple_of(opts.checkpoint_interval) {
                    b.cp_index.push(at);
                    b.cp_rw_row.push(b.rw_at.len() as u64);
                    for (r, &row) in last_write_row.iter().enumerate() {
                        b.cp_v[r].push(state.values[r]);
                        b.cp_k[r].push(state.known[r]);
                        b.cp_w[r].push(row);
                    }
                }
                let t = parsed.time.unwrap_or(NONE);
                if report.time_unit.is_none() {
                    report.time_unit = parser.unit().cloned();
                }
                if t != NONE {
                    if report.first_time.is_none() {
                        report.first_time = Some(t);
                    }
                    if report.last_time.is_some_and(|l| t < l) {
                        report.time_monotonic = false;
                    }
                    report.last_time = Some(t);
                }
                let mut flags = 0;
                if i.failed {
                    flags |= FLAG_FAILED;
                    report.condition_failed += 1;
                }
                if i.opcode.is_none() {
                    flags |= FLAG_FETCH_FAILED;
                    report.fetch_failed += 1;
                }
                let frame = b.calls.at_instruction(at, i.pc, isa.exception_number(&state));
                b.frame.push(frame);
                if let (false, Some(op)) = (i.failed, i.opcode) {
                    b.calls.after_instruction(at, i.pc, i.width, isa.flow(op, i.width));
                }
                b.pc.push(i.pc);
                b.opcode.push(i.opcode.unwrap_or(0));
                b.width.push(i.width);
                b.flags.push(flags);
                b.time.push(t);
                b.line.push(lineno);
                report.instructions += 1;
            }
            Record::Register(r) => {
                let name = TraceRegName { name: &r.name, bank: r.bank.as_deref() };
                let Some(reg) = isa.lookup(&name) else {
                    *report.unmodelled_registers.entry(r.name.to_ascii_lowercase()).or_default() += 1;
                    continue;
                };
                if reg == isa.pc() {
                    // The PC comes from instruction records.
                    continue;
                }
                let full = RegState::full_mask(isa.registers()[reg as usize].bits);
                let w = RegWrite { reg, value: r.value & full, known: r.known & full, derived: false };
                push_write(&mut b, &mut state, &mut last_write_row, &w, at, lineno);
                block.written[reg as usize] = true;
                block.line = lineno;
                report.register_writes += 1;
            }
            Record::Memory(accesses) => {
                for a in accesses {
                    if a.addr.checked_add(a.size as u64).is_none_or(|end| end > limit) {
                        report.parse_errors.count += 1;
                        if report.parse_errors.samples.len() < SAMPLES {
                            report.parse_errors.samples.push(Sample {
                                line: lineno,
                                text: format!("address out of range for {}: {}", isa.name(), mask(&text)),
                            });
                        }
                        continue;
                    }
                    // A read shows what was there before the instruction ran.
                    let row_at = if a.write { at } else { at.saturating_sub(1) };
                    if a.write {
                        report.memory_writes += 1;
                        if pending.is_some() {
                            writes_since_entry = true;
                        }
                    } else {
                        report.memory_reads += 1;
                    }
                    b.m_at.push(row_at);
                    b.m_addr.push(a.addr);
                    b.m_size.push(a.size as u32);
                    b.m_kind.push(if a.write { MEM_WRITE } else { MEM_READ });
                    b.m_value.push(a.value);
                    b.m_known.push(a.known);
                    b.m_line.push(lineno);
                }
            }
            Record::Exception { number, name, target } => {
                report.exceptions += 1;
                let reset = name.to_ascii_lowercase().contains("reset");
                b.e_at.push(at);
                b.e_number.push(number.unwrap_or(u32::MAX));
                b.e_kind.push(reset as u8);
                b.e_target.push(target.unwrap_or(NONE));
                b.e_line.push(lineno);
                // Writes before the record belong to the interrupted code;
                // resolve them before the entry changes the mode.
                report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at);
                if reset {
                    b.calls.reset(at);
                    continue;
                }
                b.calls.exception(at, number);
                if let Some(p) = pending.take() {
                    report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa) as u64;
                }
                // The frame is pushed below the stack pointer in use before
                // entry; bytes there are unknown unless the trace shows the
                // stores (they follow this record and override this). Below
                // the stack pointer nothing is live, so the largest frame is
                // covered.
                let below = state.get(sp, sp_bits);
                if let Some(s) = below {
                    mark_unknown(&mut b, MEM_CLOBBER, at, s.saturating_sub(frame_max), s.min(frame_max), lineno);
                }
                // Placeholder for the exact frame at the post-entry stack
                // pointer (known once the entry's register records are read).
                mark_unknown(&mut b, MEM_CLOBBER, at, 0, 0, lineno);
                pending = Some(PendingEntry { row: b.m_at.len() - 1, placed_below: below.is_some() });
                writes_since_entry = false;
            }
            Record::Header | Record::Blank => {}
            Record::Ignored(kind) => *report.ignored.entry(kind).or_default() += 1,
            Record::Unrecognised(kind) => {
                let u = report.unrecognised.entry(kind).or_default();
                u.count += 1;
                if u.samples.len() < SAMPLES {
                    u.samples.push(Sample { line: lineno, text: mask(&text) });
                }
            }
        }
        if pending.is_some() && writes_since_entry {
            stacking_seen = true;
        }
    }
    let at = b.pc.len() as u64;
    report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at);
    if let Some(p) = pending.take() {
        report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa) as u64;
    }
    report.exceptions_inferred = b.calls.inferred.len() as u64;
    let instructions = b.pc.len() as u64;
    if instructions == 0 {
        return Err(StoreError::Trace(format!(
            "{}: no instruction records found ({} lines; see the report for what was found)",
            opts.trace.display(),
            report.lines
        )));
    }
    report.registers_never_written = isa
        .registers()
        .iter()
        .enumerate()
        .filter(|(r, _)| *r as u16 != isa.pc() && !b.rw_reg.contains(&(*r as u16)))
        .map(|(_, d)| d.name.to_string())
        .collect();

    write_tables(&b, &opts.out, nregs, elf.as_ref())?;
    let sources = match &opts.elf {
        Some(p) => crate::sources::write(p, &opts.out)?,
        None => crate::sources::write_empty(&opts.out)?,
    };

    report.sources = sources;
    let meta = Meta {
        format_version: FORMAT_VERSION,
        isa: isa.name().to_string(),
        run: 0,
        core: core.unwrap_or(0),
        trace: opts.trace.display().to_string(),
        elf: opts.elf.as_ref().map(|p| p.display().to_string()),
        instructions,
        checkpoint_interval: opts.checkpoint_interval,
        registers: isa.registers().iter().map(|r| r.name.to_string()).collect(),
        volatile_regions: isa.volatile_regions().to_vec(),
        capabilities: Capabilities {
            register_writes: report.register_writes > 0,
            memory_values: report.memory_reads + report.memory_writes > 0,
            memory_writes: report.memory_writes > 0,
            exceptions: report.exceptions > 0,
            exception_stacking_writes: stacking_seen,
            time_unit: report.time_unit.clone(),
            elf: elf.is_some(),
            source_lines: report.sources.instructions > 0,
        },
        report,
    };
    let db_path = opts.out.join("meta.sqlite");
    let _ = std::fs::remove_file(&db_path);
    let db = rusqlite::Connection::open(&db_path)?;
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    db.execute("INSERT INTO meta VALUES('meta', ?1)", [serde_json::to_string(&meta)?])?;
    Ok(meta)
}

/// Registers written since the last alias resolution.
struct Block {
    written: Vec<bool>,
    line: u64,
}

impl Block {
    /// Append the alias writes the ISA derives for this block; returns how many.
    fn resolve(&mut self, isa: &dyn Isa, b: &mut Builder, state: &mut RegState, rows: &mut [u64], at: u64) -> u64 {
        if !self.written.iter().any(|&w| w) {
            return 0;
        }
        let derived = isa.resolve(state, &self.written);
        for w in &derived {
            push_write(b, state, rows, w, at, self.line);
        }
        self.written.iter_mut().for_each(|w| *w = false);
        derived.len() as u64
    }
}

fn push_write(b: &mut Builder, state: &mut RegState, rows: &mut [u64], w: &RegWrite, at: u64, line: u64) {
    rows[w.reg as usize] = b.rw_at.len() as u64;
    b.rw_at.push(at);
    b.rw_reg.push(w.reg);
    b.rw_value.push(w.value);
    b.rw_known.push(w.known);
    b.rw_line.push(line);
    b.rw_derived.push(w.derived as u8);
    state.apply(w);
}

fn mark_unknown(b: &mut Builder, kind: u8, at: u64, addr: u64, size: u64, line: u64) {
    b.m_at.push(at);
    b.m_addr.push(addr);
    b.m_size.push(size as u32);
    b.m_kind.push(kind);
    b.m_value.push(0);
    b.m_known.push(0);
    b.m_line.push(line);
}

/// Fill in the placeholder with the exact frame the ISA reads from the
/// post-entry state. Returns true when neither range could be placed.
fn finish_entry(b: &mut Builder, p: PendingEntry, state: &RegState, isa: &dyn Isa) -> bool {
    match isa.exception_frame(state) {
        Some((base, size)) => {
            b.m_addr[p.row] = base;
            b.m_size[p.row] = size as u32;
            false
        }
        None => !p.placed_below,
    }
}

fn write_tables(b: &Builder, out: &Path, nregs: usize, elf: Option<&Elf>) -> Result<(), StoreError> {
    write(
        &out.join("instructions.arrow"),
        &[
            ("pc", Column::U64(b.pc.clone())),
            ("opcode", Column::U32(b.opcode.clone())),
            ("width", Column::U8(b.width.clone())),
            ("flags", Column::U8(b.flags.clone())),
            ("time", Column::U64(b.time.clone())),
            ("line", Column::U64(b.line.clone())),
            ("frame", Column::U32(b.frame.clone())),
        ],
    )?;
    let c = &b.calls;
    write(
        &out.join("calls.arrow"),
        &[
            ("kind", Column::U8(c.kind.clone())),
            ("call_index", Column::U64(c.call_index.clone())),
            ("entry_index", Column::U64(c.entry_index.clone())),
            ("return_index", Column::U64(c.return_index.clone())),
            ("call_pc", Column::U64(c.call_pc.clone())),
            ("return_pc", Column::U64(c.return_pc.clone())),
            ("number", Column::U32(c.number.clone())),
            ("parent", Column::U32(c.parent.clone())),
            ("depth", Column::U32(c.depth.clone())),
        ],
    )?;
    write(
        &out.join("reg_writes.arrow"),
        &[
            ("at", Column::U64(b.rw_at.clone())),
            ("reg", Column::U16(b.rw_reg.clone())),
            ("value", Column::U64(b.rw_value.clone())),
            ("known", Column::U64(b.rw_known.clone())),
            ("line", Column::U64(b.rw_line.clone())),
            ("derived", Column::U8(b.rw_derived.clone())),
        ],
    )?;
    write(
        &out.join("mem.arrow"),
        &[
            ("at", Column::U64(b.m_at.clone())),
            ("addr", Column::U64(b.m_addr.clone())),
            ("size", Column::U32(b.m_size.clone())),
            ("kind", Column::U8(b.m_kind.clone())),
            ("value", Column::U64(b.m_value.clone())),
            ("known", Column::U8(b.m_known.clone())),
            ("line", Column::U64(b.m_line.clone())),
        ],
    )?;
    // Byte index: every byte of every access, sorted by (addr, at, row) so the
    // last entry with at <= i is the state at point i. Read bytes the trace
    // does not show carry no information and are left out.
    let mut entries: Vec<(u64, u64, u32)> = Vec::new();
    for row in 0..b.m_at.len() {
        let (addr, size, kind) = (b.m_addr[row], b.m_size[row] as u64, b.m_kind[row]);
        for k in 0..size {
            if kind == MEM_READ && b.m_known[row] & (1 << k) == 0 {
                continue;
            }
            entries.push((addr + k, b.m_at[row], row as u32));
        }
    }
    entries.sort_unstable();
    write(
        &out.join("mem_index.arrow"),
        &[
            ("addr", Column::U64(entries.iter().map(|e| e.0).collect())),
            ("at", Column::U64(entries.iter().map(|e| e.1).collect())),
            ("row", Column::U32(entries.iter().map(|e| e.2).collect())),
        ],
    )?;
    let mut order: Vec<u64> = (0..b.pc.len() as u64).collect();
    order.sort_unstable_by_key(|&i| (b.pc[i as usize], i));
    write(
        &out.join("pc_index.arrow"),
        &[("pc", Column::U64(order.iter().map(|&i| b.pc[i as usize]).collect())), ("index", Column::U64(order))],
    )?;
    let mut cols = vec![
        ("index".to_string(), Column::U64(b.cp_index.clone())),
        ("rw_row".to_string(), Column::U64(b.cp_rw_row.clone())),
    ];
    for r in 0..nregs {
        cols.push((format!("v{r}"), Column::U64(b.cp_v[r].clone())));
        cols.push((format!("k{r}"), Column::U64(b.cp_k[r].clone())));
        cols.push((format!("w{r}"), Column::U64(b.cp_w[r].clone())));
    }
    let cols_ref: Vec<(&str, Column)> = cols.into_iter().map(|(n, c)| (leak(n), c)).collect();
    write(&out.join("checkpoints.arrow"), &cols_ref)?;
    write(
        &out.join("exceptions.arrow"),
        &[
            ("at", Column::U64(b.e_at.clone())),
            ("number", Column::U32(b.e_number.clone())),
            ("kind", Column::U8(b.e_kind.clone())),
            ("target", Column::U64(b.e_target.clone())),
            ("line", Column::U64(b.e_line.clone())),
        ],
    )?;
    let image = elf.map(|e| e.image.clone()).unwrap_or_default();
    write(
        &out.join("image.arrow"),
        &[
            ("addr", Column::U64(image.iter().map(|s| s.addr).collect())),
            ("data", Column::Binary(image.into_iter().map(|s| s.data).collect())),
        ],
    )?;
    Ok(())
}

/// Column names for checkpoints are built at run time; the writer wants
/// `&str` for the lifetime of the call only, so a short-lived leak per index
/// build is acceptable (a few dozen small strings).
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Replace hex digits in hex-looking tokens so samples can leave a closed
/// network without addresses or data. Record types and keywords stay.
pub fn mask_hex(line: &str) -> String {
    line.split_inclusive(char::is_whitespace)
        .map(|tok| {
            let t = tok.trim_end();
            let core = t.trim_matches(|c| c == '(' || c == ')' || c == '[' || c == ']' || c == ',' || c == ':');
            let hexish = core.len() >= 3
                && core.bytes().all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'_' || c == b'x' || c == b'.')
                && (core.len() >= 4 || core.bytes().any(|c| c.is_ascii_digit()));
            if hexish {
                tok.chars().map(|c| if c.is_ascii_hexdigit() { '#' } else { c }).collect()
            } else {
                tok.to_string()
            }
        })
        .collect()
}

#[allow(dead_code)]
fn _counts(_: &BTreeMap<String, Unparsed>) {}

#[cfg(test)]
mod tests {
    use super::mask_hex;

    #[test]
    fn masks_addresses_and_values() {
        assert_eq!(mask_hex("3039 cyc MNW4___D 2002fa00 efbeefbe"), "#### cyc MNW4___D ######## ########");
        assert_eq!(mask_hex("IT (00000044:00000000) LDR r2"), "IT (########:########) LDR r2");
    }
}
