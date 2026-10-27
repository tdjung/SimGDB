//! Build a dataset from a trace (the Runway).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use simgdb_elf::Elf;
use simgdb_isa::{Endian, Isa, RegState, RegWrite, TraceRegName};
use simgdb_tarmac::{Event, Parser, Record};

use crate::calls::Calls;
use crate::sort::ExtSorter;
use crate::table::{Column, TableWriter, write};
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
    /// Rows per record batch in the column files (a power of two).
    pub batch_rows: usize,
    /// Records an index sort holds in memory before spilling a run to disk.
    pub sort_records: usize,
    /// Site configuration (notation, register aliases, memory regions, time map).
    pub config: Option<crate::config::SiteConfig>,
    /// Data byte order; overrides the configuration and the ELF header.
    pub endian: Option<simgdb_isa::Endian>,
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
            batch_rows: crate::table::BATCH,
            sort_records: 16 << 20,
            config: None,
            endian: None,
        }
    }
}

const SAMPLES: usize = 5;

/// Index under construction. The large tables stream to their files as
/// rows arrive; the sorted indexes go through external sorts; only the small
/// tables (checkpoints, exceptions, calls) stay in memory.
struct Builder {
    /// instructions: pc, opcode, width, flags, time, line, frame
    insn: TableWriter,
    /// reg_writes: at, reg, value, known, line, derived
    rw: TableWriter,
    /// mem: at, addr, size, kind, value, known, line
    mem: TableWriter,
    /// Memory index entries (addr, at, row) and PC index entries (pc, index).
    mi: ExtSorter<3>,
    pi: ExtSorter<2>,
    /// Instructions so far, and the last one's PC.
    count: u64,
    last_pc: Option<u64>,
    /// Registers that have a write record.
    written_regs: Vec<bool>,
    /// Registers not fully known at some point, and each register's
    /// all-bits-known mask.
    not_always_known: Vec<bool>,
    full_mask: Vec<u64>,
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
    calls: Calls,
}

/// Memory rows kept buffered so a pending exception frame row can still be patched.
const MEM_KEEP: usize = 4096;

impl Builder {
    fn new(out: &Path, nregs: usize, opts: &IndexOptions, tmp: &Path) -> Result<Builder, StoreError> {
        let t = |name: &str, cols: Vec<(&str, Column)>| TableWriter::create(&out.join(name), cols, opts.batch_rows);
        let u64s = || Column::U64(vec![]);
        Ok(Builder {
            insn: t(
                "instructions.arrow",
                vec![
                    ("pc", u64s()),
                    ("opcode", Column::U32(vec![])),
                    ("width", Column::U8(vec![])),
                    ("flags", Column::U8(vec![])),
                    ("time", u64s()),
                    ("line", u64s()),
                    ("frame", Column::U32(vec![])),
                ],
            )?,
            rw: t(
                "reg_writes.arrow",
                vec![
                    ("at", u64s()),
                    ("reg", Column::U16(vec![])),
                    ("value", u64s()),
                    ("known", u64s()),
                    ("line", u64s()),
                    ("derived", Column::U8(vec![])),
                ],
            )?,
            mem: t(
                "mem.arrow",
                vec![
                    ("at", u64s()),
                    ("addr", u64s()),
                    ("size", Column::U32(vec![])),
                    ("kind", Column::U8(vec![])),
                    ("value", u64s()),
                    ("known", Column::U8(vec![])),
                    ("line", u64s()),
                ],
            )?,
            mi: ExtSorter::new(tmp, "mem_index", opts.sort_records),
            pi: ExtSorter::new(tmp, "pc_index", opts.sort_records),
            count: 0,
            last_pc: None,
            written_regs: vec![false; nregs],
            not_always_known: vec![false; nregs],
            full_mask: vec![u64::MAX; nregs],
            cp_index: vec![],
            cp_rw_row: vec![],
            cp_v: vec![vec![]; nregs],
            cp_k: vec![vec![]; nregs],
            cp_w: vec![vec![]; nregs],
            e_at: vec![],
            e_number: vec![],
            e_kind: vec![],
            e_target: vec![],
            e_line: vec![],
            calls: Calls::with_file(&tmp.join("calls.frames"))?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_instruction(
        &mut self,
        pc: u64,
        opcode: u32,
        width: u8,
        flags: u8,
        time: u64,
        line: u64,
        frame: u32,
    ) -> Result<(), StoreError> {
        let c = &mut self.insn.cols;
        c[0].u64s().push(pc);
        c[1].u32s().push(opcode);
        c[2].u8s().push(width);
        c[3].u8s().push(flags);
        c[4].u64s().push(time);
        c[5].u64s().push(line);
        c[6].u32s().push(frame);
        self.insn.flush(0)?;
        self.pi.push([pc, self.count])?;
        self.count += 1;
        self.last_pc = Some(pc);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_mem(
        &mut self,
        at: u64,
        addr: u64,
        size: u32,
        kind: u8,
        value: u64,
        known: u8,
        line: u64,
    ) -> Result<(), StoreError> {
        let c = &mut self.mem.cols;
        c[0].u64s().push(at);
        c[1].u64s().push(addr);
        c[2].u32s().push(size);
        c[3].u8s().push(kind);
        c[4].u64s().push(value);
        c[5].u8s().push(known);
        c[6].u64s().push(line);
        let mi = &mut self.mi;
        self.mem.flush_with(MEM_KEEP, |cols, first, n| index_bytes(mi, cols, first, n))
    }

    /// Change a buffered memory row (the pending exception frame).
    fn patch_mem(&mut self, row: usize, addr: u64, size: u32) -> Result<(), StoreError> {
        let k = row
            .checked_sub(self.mem.written)
            .ok_or_else(|| StoreError::Internal("patched row already written".into()))?;
        self.mem.cols[1].u64s()[k] = addr;
        self.mem.cols[2].u32s()[k] = size;
        Ok(())
    }

    fn mem_rows(&self) -> usize {
        self.mem.rows()
    }
}

/// Byte index entries for buffered memory rows: every byte of every access,
/// except read bytes the trace does not show (they carry no information).
fn index_bytes(mi: &mut ExtSorter<3>, cols: &[Column], first: usize, n: usize) -> Result<(), StoreError> {
    let (at, addr, size, kind, known) =
        (cols[0].as_u64(), cols[1].as_u64(), cols[2].as_u32(), cols[3].as_u8(), cols[5].as_u8());
    if first + n > u32::MAX as usize {
        return Err(StoreError::Trace("more than 2^32 memory records: not supported yet".into()));
    }
    for k in 0..n {
        for b in 0..size[k] as u64 {
            if kind[k] == MEM_READ && known[k] & (1 << b) == 0 {
                continue;
            }
            mi.push([addr[k] + b, at[k], (first + k) as u64])?;
        }
    }
    Ok(())
}

/// An exception entry whose post-entry stack pointer is not known yet: its
/// placeholder clobber row is filled in when the next instruction starts.
struct PendingEntry {
    row: usize,
    /// The range below the pre-entry stack pointer was marked.
    placed_below: bool,
}

/// Data byte order, where it came from, and a note when an explicit choice
/// disagrees with the ELF header: the option, else the configuration, else
/// the ELF header, else the ISA default.
fn data_endian(isa: &dyn Isa, opts: &IndexOptions, elf: Option<&Elf>) -> (Endian, &'static str, Option<String>) {
    let from_elf = elf.map(|e| if e.big_endian { Endian::Big } else { Endian::Little });
    let explicit = match (opts.endian, opts.config.as_ref().and_then(|c| c.memory.endian)) {
        (Some(e), _) => Some((e, "option")),
        (None, Some(e)) => Some((e, "config")),
        _ => None,
    };
    match (explicit, from_elf) {
        (Some((e, src)), Some(f)) if e != f => {
            (e, src, Some(format!("{src} says {} but the ELF header says {}; using {}", e.name(), f.name(), e.name())))
        }
        (Some((e, src)), _) => (e, src, None),
        (None, Some(f)) => (f, "elf", None),
        (None, None) => (isa.default_endian(), "isa default", None),
    }
}

pub fn build(isa: &dyn Isa, opts: &IndexOptions) -> Result<Meta, StoreError> {
    let elf = match &opts.elf {
        Some(p) => Some(Elf::load(p)?),
        None => None,
    };
    if opts.checkpoint_interval == 0 {
        return Err(StoreError::Trace("checkpoint interval must be at least 1".into()));
    }
    let (endian, endian_source, endian_conflict) = data_endian(isa, opts, elf.as_ref());
    std::fs::create_dir_all(&opts.out).map_err(|e| StoreError::io(&opts.out, e))?;
    let reader = simgdb_tarmac::open(&opts.trace).map_err(|e| StoreError::io(&opts.trace, e))?;
    let nregs = isa.registers().len();
    if !opts.batch_rows.is_power_of_two() {
        return Err(StoreError::Trace("batch rows must be a power of two".into()));
    }
    let tmp = opts.out.join(".sort");
    std::fs::create_dir_all(&tmp).map_err(|e| StoreError::io(&tmp, e))?;
    let mut b = Builder::new(&opts.out, nregs, opts, &tmp)?;
    for (r, def) in isa.registers().iter().enumerate() {
        b.full_mask[r] = RegState::full_mask(def.bits);
    }
    let mut report = Report { time_monotonic: true, ..Default::default() };
    let mut state = RegState::new(nregs);
    let mut last_write_row = vec![NONE; nregs];
    if let Some(c) = &opts.config {
        c.validate(isa).map_err(StoreError::Trace)?;
    }
    let mut parser = match &opts.config {
        Some(c) => Parser::with_notation(c.notation()),
        None => Parser::new(),
    };
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
                let now = b.count;
                for a in accesses.iter().filter(|a| a.write) {
                    if a.addr.checked_add(a.size as u64).is_some_and(|end| end <= limit) {
                        report.other_core_writes += 1;
                        mark_unknown(&mut b, MEM_FOREIGN, now, a.addr, a.size as u64, lineno)?;
                    }
                }
            }
            continue;
        }
        let at = b.count;
        match parsed.record {
            Record::Instruction(i) => {
                if i.event == Event::If && b.last_pc == Some(i.pc) {
                    report.if_duplicates += 1;
                    continue;
                }
                report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at)?;
                if let Some(p) = pending.take() {
                    report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa)? as u64;
                }
                if at == 0 {
                    // The first point: anything not known yet. Later a
                    // register's knowledge changes only through a write.
                    for r in 0..nregs {
                        if state.known[r] != b.full_mask[r] {
                            b.not_always_known[r] = true;
                        }
                    }
                }
                if at.is_multiple_of(opts.checkpoint_interval) {
                    b.cp_index.push(at);
                    b.cp_rw_row.push(b.rw.rows() as u64);
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
                let frame = b.calls.at_instruction(at, i.pc, isa.exception_number(&state))?;
                if let (false, Some(op)) = (i.failed, i.opcode) {
                    b.calls.after_instruction(at, i.pc, i.width, isa.flow(op, i.width))?;
                }
                b.push_instruction(i.pc, i.opcode.unwrap_or(0), i.width, flags, t, lineno, frame)?;
                report.instructions += 1;
            }
            Record::Register(r) => {
                let aliased = opts.config.as_ref().and_then(|c| c.register_alias(&r.name));
                let name = match aliased {
                    Some(to) => TraceRegName { name: to, bank: None },
                    None => TraceRegName { name: &r.name, bank: r.bank.as_deref() },
                };
                let Some(reg) = isa.lookup(&name) else {
                    *report.unmodelled_registers.entry(r.name.to_ascii_lowercase()).or_default() += 1;
                    continue;
                };
                if reg == isa.pc() {
                    // The PC comes from instruction records.
                    continue;
                }
                let full = RegState::full_mask(isa.registers()[reg as usize].bits);
                // Bits the record does not write (`-` digits) keep their value.
                let (wm, k) = (r.written & full, reg as usize);
                let w = RegWrite {
                    reg,
                    value: (state.values[k] & !wm) | (r.value & wm),
                    known: (state.known[k] & !wm) | (r.known & wm),
                    derived: false,
                };
                push_write(&mut b, &mut state, &mut last_write_row, &w, at, lineno)?;
                block.written[reg as usize] = true;
                block.line = lineno;
                report.register_writes += 1;
            }
            Record::Memory(accesses) => {
                for a in accesses {
                    // A number written in the trace is in the core's data
                    // byte order; the bytes are stored by address.
                    let a = if a.logical && endian.at(isa, a.addr) == Endian::Big { a.reversed() } else { a };
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
                    let kind = if a.write { MEM_WRITE } else { MEM_READ };
                    b.push_mem(row_at, a.addr, a.size as u32, kind, a.value, a.known, lineno)?;
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
                report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at)?;
                if reset {
                    b.calls.reset(at)?;
                    continue;
                }
                b.calls.exception(at, number)?;
                if let Some(p) = pending.take() {
                    report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa)? as u64;
                }
                // The frame is pushed below the stack pointer in use before
                // entry; bytes there are unknown unless the trace shows the
                // stores (they follow this record and override this). Below
                // the stack pointer nothing is live, so the largest frame is
                // covered.
                let below = state.get(sp, sp_bits);
                if let Some(s) = below {
                    mark_unknown(&mut b, MEM_CLOBBER, at, s.saturating_sub(frame_max), s.min(frame_max), lineno)?;
                }
                // Placeholder for the exact frame at the post-entry stack
                // pointer (known once the entry's register records are read).
                mark_unknown(&mut b, MEM_CLOBBER, at, 0, 0, lineno)?;
                pending = Some(PendingEntry { row: b.mem_rows() - 1, placed_below: below.is_some() });
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
    let at = b.count;
    report.derived_register_writes += block.resolve(isa, &mut b, &mut state, &mut last_write_row, at)?;
    if let Some(p) = pending.take() {
        report.exception_frames_unplaced += finish_entry(&mut b, p, &state, isa)? as u64;
    }
    report.exceptions_inferred = b.calls.inferred.len() as u64;
    let instructions = b.count;
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
        .filter(|(r, _)| *r as u16 != isa.pc() && !b.written_regs[*r])
        .map(|(_, d)| d.name.to_string())
        .collect();
    report.registers_not_always_known = Some(
        isa.registers()
            .iter()
            .enumerate()
            .filter(|(r, _)| *r as u16 != isa.pc() && b.not_always_known[*r])
            .map(|(_, d)| d.name.to_string())
            .collect(),
    );

    write_tables(b, &opts.out, nregs, elf.as_ref(), opts.batch_rows)?;
    let _ = std::fs::remove_dir_all(&tmp);
    let sources = match &opts.elf {
        Some(p) => crate::sources::write(p, &opts.out)?,
        None => crate::sources::write_empty(&opts.out)?,
    };

    report.sources = sources;
    report.endian_source = Some(endian_source.to_string());
    report.endian_conflict = endian_conflict;
    let meta = Meta {
        format_version: FORMAT_VERSION,
        endian,
        isa: isa.name().to_string(),
        run: 0,
        core: core.unwrap_or(0),
        trace: opts.trace.display().to_string(),
        elf: opts.elf.as_ref().map(|p| p.display().to_string()),
        instructions,
        checkpoint_interval: opts.checkpoint_interval,
        registers: isa.registers().iter().map(|r| r.name.to_string()).collect(),
        volatile_regions: match opts.config.as_ref().and_then(|c| c.memory.volatile.as_ref()) {
            Some(v) => v.iter().map(|r| (r[0], r[1])).collect(),
            None => isa.volatile_regions().to_vec(),
        },
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
        config: opts.config.clone(),
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
    fn resolve(
        &mut self,
        isa: &dyn Isa,
        b: &mut Builder,
        state: &mut RegState,
        rows: &mut [u64],
        at: u64,
    ) -> Result<u64, StoreError> {
        if !self.written.iter().any(|&w| w) {
            return Ok(0);
        }
        let derived = isa.resolve(state, &self.written);
        for w in &derived {
            push_write(b, state, rows, w, at, self.line)?;
        }
        self.written.iter_mut().for_each(|w| *w = false);
        Ok(derived.len() as u64)
    }
}

fn push_write(
    b: &mut Builder,
    state: &mut RegState,
    rows: &mut [u64],
    w: &RegWrite,
    at: u64,
    line: u64,
) -> Result<(), StoreError> {
    rows[w.reg as usize] = b.rw.rows() as u64;
    let c = &mut b.rw.cols;
    c[0].u64s().push(at);
    c[1].u16s().push(w.reg);
    c[2].u64s().push(w.value);
    c[3].u64s().push(w.known);
    c[4].u64s().push(line);
    c[5].u8s().push(w.derived as u8);
    b.rw.flush(0)?;
    b.written_regs[w.reg as usize] = true;
    state.apply(w);
    let r = w.reg as usize;
    if state.known[r] != b.full_mask[r] {
        b.not_always_known[r] = true;
    }
    Ok(())
}

fn mark_unknown(b: &mut Builder, kind: u8, at: u64, addr: u64, size: u64, line: u64) -> Result<(), StoreError> {
    b.push_mem(at, addr, size as u32, kind, 0, 0, line)
}

/// Fill in the placeholder with the exact frame the ISA reads from the
/// post-entry state. Returns true when neither range could be placed.
fn finish_entry(b: &mut Builder, p: PendingEntry, state: &RegState, isa: &dyn Isa) -> Result<bool, StoreError> {
    match isa.exception_frame(state) {
        Some((base, size)) => {
            b.patch_mem(p.row, base, size as u32)?;
            Ok(false)
        }
        None => Ok(!p.placed_below),
    }
}

fn write_tables(b: Builder, out: &Path, nregs: usize, elf: Option<&Elf>, batch: usize) -> Result<(), StoreError> {
    let Builder { insn, rw, mem, mut mi, pi, calls, .. } = b;
    insn.finish()?;
    rw.finish()?;
    mem.finish_with(|cols, first, n| index_bytes(&mut mi, cols, first, n))?;
    calls.write(&out.join("calls.arrow"), batch)?;
    // Byte index sorted by (addr, at, row): the last entry with at <= i is
    // the state at point i. PC index sorted by (pc, index).
    let mut w = TableWriter::create(
        &out.join("mem_index.arrow"),
        vec![("addr", Column::U64(vec![])), ("at", Column::U64(vec![])), ("row", Column::U32(vec![]))],
        batch,
    )?;
    mi.finish(|[addr, at, row]| {
        w.cols[0].u64s().push(addr);
        w.cols[1].u64s().push(at);
        w.cols[2].u32s().push(row as u32);
        w.flush(0)
    })?;
    w.finish()?;
    let mut w = TableWriter::create(
        &out.join("pc_index.arrow"),
        vec![("pc", Column::U64(vec![])), ("index", Column::U64(vec![]))],
        batch,
    )?;
    pi.finish(|[pc, index]| {
        w.cols[0].u64s().push(pc);
        w.cols[1].u64s().push(index);
        w.flush(0)
    })?;
    w.finish()?;
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
