//! SimGDB query engine (the Hangar): state at any point, with provenance.
//!
//! Point layer (ARCHITECTURE §4): `registers`, `register`, `memory`, `pc`.
//! Set layer, as needed by the GDB server in this milestone: `executions`,
//! `next_execution`. Every value says where it came from; nothing is guessed.

use std::path::Path;

use serde::Serialize;
use simgdb_isa::{Isa, RegId, RegState};
use simgdb_model::{Conflict, Evidence, Point, SimTime, Source, TimeUnit, Value};
use simgdb_store::{Dataset, FLAG_FAILED, MEM_CLOBBER, MEM_READ, MEM_WRITE, NONE, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("unknown ISA {0}")]
    Isa(String),
    #[error("point {index} is outside the trace (0..={last})")]
    OutOfRange { index: u64, last: u64 },
    #[error("unknown register {0}")]
    Register(String),
}

pub struct Engine {
    pub ds: Dataset,
    isa: Box<dyn Isa>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegisterValue {
    pub name: &'static str,
    #[serde(flatten)]
    pub value: Value<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Info {
    pub isa: String,
    pub trace: String,
    pub elf: Option<String>,
    pub instructions: u64,
    /// The last valid point (state after the last instruction).
    pub end: u64,
    pub time_unit: Option<TimeUnit>,
    pub first_time: Option<u64>,
    pub last_time: Option<u64>,
    pub capabilities: simgdb_store::Capabilities,
    pub limitations: Vec<String>,
}

impl Engine {
    pub fn open(dir: &Path) -> Result<Engine, EngineError> {
        let ds = Dataset::open(dir)?;
        let isa = simgdb_isa::by_name(&ds.meta.isa).ok_or_else(|| EngineError::Isa(ds.meta.isa.clone()))?;
        Ok(Engine { ds, isa })
    }

    pub fn isa(&self) -> &dyn Isa {
        self.isa.as_ref()
    }

    /// Number of instruction records; valid points are `0..=end()`.
    pub fn end(&self) -> u64 {
        self.ds.instructions()
    }

    fn check(&self, index: u64) -> Result<(), EngineError> {
        if index > self.end() {
            return Err(EngineError::OutOfRange { index, last: self.end() });
        }
        Ok(())
    }

    pub fn info(&self) -> Info {
        let m = &self.ds.meta;
        let c = &m.capabilities;
        let mut limitations = Vec::new();
        if !c.register_writes {
            limitations.push("no register records: register values are unknown".into());
        }
        if !c.memory_values {
            limitations.push("no memory records: memory comes only from the ELF image".into());
        }
        if c.exceptions && !c.exception_stacking_writes {
            limitations.push(
                "exception entry stores (stacking) are not in the trace: frame bytes are unknown until written again"
                    .into(),
            );
        }
        if matches!(c.time_unit, Some(TimeUnit::Cycle(_))) {
            limitations.push("time stamps are cycles: absolute time needs a time map (not given)".into());
        }
        if !m.report.registers_never_written.is_empty() {
            limitations.push(format!("never written, always unknown: {}", m.report.registers_never_written.join(", ")));
        }
        Info {
            isa: m.isa.clone(),
            trace: m.trace.clone(),
            elf: m.elf.clone(),
            instructions: m.instructions,
            end: self.end(),
            time_unit: c.time_unit.clone(),
            first_time: m.report.first_time,
            last_time: m.report.last_time,
            capabilities: c.clone(),
            limitations,
        }
    }

    pub fn point(&self, index: u64) -> Result<Point, EngineError> {
        self.check(index)?;
        let sim_time =
            (index < self.end()).then(|| self.ds.insn_time.get(index as usize)).filter(|&t| t != NONE).map(|value| {
                SimTime { value, unit: self.ds.meta.capabilities.time_unit.clone().unwrap_or(TimeUnit::None) }
            });
        Ok(Point { run: self.ds.meta.run, core: self.ds.meta.core, index, sim_time })
    }

    /// First point whose instruction time is at or after `time` (same unit as
    /// the trace). `None` if the trace has no times or ends before.
    pub fn index_at_time(&self, time: u64) -> Option<u64> {
        let n = self.end() as usize;
        if !self.ds.meta.report.time_monotonic {
            return (0..n)
                .find(|&i| {
                    let t = self.ds.insn_time.get(i);
                    t != NONE && t >= time
                })
                .map(|i| i as u64);
        }
        let i = self.ds.insn_time.partition_point(0, n, |t| t == NONE || t < time);
        (i < n).then_some(i as u64)
    }

    pub fn evidence_for_instruction(&self, index: u64) -> Evidence {
        Evidence { index: Some(index), line: Some(self.ds.insn_line.get(index as usize)), note: None }
    }

    /// Instruction at `index` was condition-failed (`IS`, `CCFAIL`).
    pub fn condition_failed(&self, index: u64) -> bool {
        index < self.end() && self.ds.insn_flags.get(index as usize) & FLAG_FAILED != 0
    }

    pub fn pc(&self, index: u64) -> Result<Value<u64>, EngineError> {
        self.check(index)?;
        if index == self.end() {
            return Ok(Value::unknown_because(Evidence {
                note: Some("end of trace: the next PC is not recorded".into()),
                ..Default::default()
            }));
        }
        Ok(Value::known(self.ds.insn_pc.get(index as usize), Source::Trace, self.evidence_for_instruction(index)))
    }

    pub fn register_id(&self, name: &str) -> Result<RegId, EngineError> {
        self.isa
            .registers()
            .iter()
            .position(|r| r.name.eq_ignore_ascii_case(name))
            .map(|r| r as RegId)
            .ok_or_else(|| EngineError::Register(name.to_string()))
    }

    /// All registers at `index`.
    pub fn registers(&self, index: u64) -> Result<Vec<RegisterValue>, EngineError> {
        self.check(index)?;
        let (state, rows) = self.replay(index);
        let pc = self.isa.pc();
        let mut out = Vec::new();
        for (r, def) in self.isa.registers().iter().enumerate() {
            let value =
                if r as RegId == pc { self.pc(index)? } else { self.reg_value(&state, &rows, r as RegId, def.bits) };
            out.push(RegisterValue { name: def.name, value });
        }
        Ok(out)
    }

    pub fn register(&self, index: u64, reg: RegId) -> Result<Value<u64>, EngineError> {
        self.check(index)?;
        if reg == self.isa.pc() {
            return self.pc(index);
        }
        let (state, rows) = self.replay(index);
        Ok(self.reg_value(&state, &rows, reg, self.isa.registers()[reg as usize].bits))
    }

    fn reg_value(&self, state: &RegState, rows: &[u64], reg: RegId, bits: u32) -> Value<u64> {
        let r = reg as usize;
        let full = RegState::full_mask(bits);
        if state.known[r] == 0 {
            return Value::unknown();
        }
        let row = rows[r];
        let mut evidence = Evidence::default();
        if row != NONE {
            let at = self.ds.rw_at.get(row as usize);
            evidence.line = Some(self.ds.rw_line.get(row as usize));
            evidence.index = at.checked_sub(1);
            if self.ds.rw_derived.get(row as usize) != 0 {
                evidence.note = Some("follows a banked register write".into());
            }
            if at == 0 {
                evidence.note = Some("written before the first instruction".into());
            }
        }
        let mut v = Value::known(state.values[r], Source::Trace, evidence);
        if state.known[r] & full != full {
            v.known_mask = Some(state.known[r]);
        }
        v
    }

    /// Register state at `index` and, per register, the reg_writes row that
    /// set it: nearest checkpoint, then the writes visible up to `index`.
    fn replay(&self, index: u64) -> (RegState, Vec<u64>) {
        let ds = &self.ds;
        let n = ds.cp_index.len();
        let c = ds.cp_index.partition_point(0, n, |i| i <= index).saturating_sub(1);
        let nregs = ds.cp_regs.len();
        let mut state = RegState::new(nregs);
        let mut rows = vec![NONE; nregs];
        let mut row = 0usize;
        if n > 0 {
            for (r, (v, k, w)) in ds.cp_regs.iter().enumerate() {
                state.values[r] = v.get(c);
                state.known[r] = k.get(c);
                rows[r] = w.get(c);
            }
            row = ds.cp_rw_row.get(c) as usize;
        }
        let total = ds.rw_at.len();
        while row < total && ds.rw_at.get(row) <= index {
            let r = ds.rw_reg.get(row) as usize;
            let known = ds.rw_known.get(row);
            state.values[r] = ds.rw_value.get(row) & known;
            state.known[r] = known;
            rows[r] = row as u64;
            row += 1;
        }
        (state, rows)
    }

    /// Bytes `addr..addr+len` at `index`, one value per byte.
    pub fn memory(&self, addr: u64, len: u64, index: u64) -> Result<Vec<Value<u8>>, EngineError> {
        self.check(index)?;
        Ok((0..len).map(|k| self.byte(addr.wrapping_add(k), index)).collect())
    }

    fn volatile(&self, addr: u64) -> bool {
        self.ds.meta.volatile_regions.iter().any(|&(lo, hi)| addr >= lo && addr < hi)
    }

    fn row_evidence(&self, row: usize) -> Evidence {
        let at = self.ds.mem_at.get(row);
        let kind = self.ds.mem_kind.get(row);
        Evidence {
            index: if kind == MEM_READ { Some(at) } else { at.checked_sub(1) },
            line: Some(self.ds.mem_line.get(row)),
            note: None,
        }
    }

    /// Byte `k` of memory row `row`, if the trace shows it.
    fn row_byte(&self, row: usize, addr: u64) -> Option<u8> {
        let k = addr - self.ds.mem_addr.get(row);
        (self.ds.mem_known.get(row) & (1 << k) != 0).then(|| (self.ds.mem_value.get(row) >> (8 * k)) as u8)
    }

    fn initial_byte(&self, addr: u64) -> Option<u8> {
        let img = &self.ds.image;
        let i = img.partition_point(|(a, _)| *a <= addr);
        let (base, data) = img.get(i.checked_sub(1)?)?;
        data.get((addr - base) as usize).copied()
    }

    fn byte(&self, addr: u64, index: u64) -> Value<u8> {
        let ds = &self.ds;
        let n = ds.mi_addr.len();
        let lo = ds.mi_addr.partition_point(0, n, |a| a < addr);
        let hi = ds.mi_addr.partition_point(lo, n, |a| a <= addr);
        // Entries for this byte are sorted by `at`: the last one at or before
        // `index` is the latest record of it.
        let pos = ds.mi_at.partition_point(lo, hi, |at| at <= index);
        if self.volatile(addr) {
            let mut v = Value::unknown();
            if pos > lo {
                let mut e = self.row_evidence(ds.mi_row.get(pos - 1) as usize);
                e.note = Some("peripheral region: last access shown as evidence only".into());
                v.evidence.push(e);
            }
            return v;
        }
        if pos > lo {
            let row = ds.mi_row.get(pos - 1) as usize;
            let evidence = self.row_evidence(row);
            return match ds.mem_kind.get(row) {
                MEM_CLOBBER => Value::unknown_because(Evidence {
                    note: Some("exception entry may have overwritten this byte (frame push not in the trace)".into()),
                    ..evidence
                }),
                _ => match self.row_byte(row, addr) {
                    Some(b) => Value::known(b, Source::Trace, evidence),
                    None => Value::unknown_because(Evidence {
                        note: Some("trace shows the value as unknown".into()),
                        ..evidence
                    }),
                },
            };
        }
        // Nothing recorded up to `index`: the ELF image, or a later read with
        // no write in between.
        let initial = self.initial_byte(addr);
        let later = if pos < hi {
            let row = ds.mi_row.get(pos) as usize;
            (ds.mem_kind.get(row) == MEM_READ).then(|| self.row_byte(row, addr).map(|b| (b, row))).flatten()
        } else {
            None
        };
        let elf_evidence = || Evidence { note: Some("ELF load image".into()), ..Default::default() };
        match (initial, later) {
            (Some(i), Some((b, row))) if i == b => {
                let mut v = Value::known(i, Source::ElfInitial, elf_evidence());
                v.evidence.push(Evidence { note: Some("a later read agrees".into()), ..self.row_evidence(row) });
                v
            }
            (Some(i), Some((b, row))) => {
                let mut v = Value::known(
                    b,
                    Source::Inferred,
                    Evidence { note: Some("read later with no write before it".into()), ..self.row_evidence(row) },
                );
                v.conflicts.push(Conflict { value: i, source: Source::ElfInitial, evidence: elf_evidence() });
                v
            }
            (Some(i), None) => Value::known(i, Source::ElfInitial, elf_evidence()),
            (None, Some((b, row))) => Value::known(
                b,
                Source::Inferred,
                Evidence { note: Some("read later with no write before it".into()), ..self.row_evidence(row) },
            ),
            (None, None) => Value::unknown(),
        }
    }

    /// Indexes at which `pc` executed, ascending.
    pub fn executions(&self, pc: u64) -> impl Iterator<Item = u64> + '_ {
        let ds = &self.ds;
        let n = ds.pi_pc.len();
        let lo = ds.pi_pc.partition_point(0, n, |p| p < pc);
        let hi = ds.pi_pc.partition_point(lo, n, |p| p <= pc);
        (lo..hi).map(move |i| ds.pi_index.get(i))
    }

    /// Nearest execution of any of `pcs` strictly after (`forward`) or before
    /// `from`.
    pub fn next_execution(&self, pcs: &[u64], from: u64, forward: bool) -> Option<u64> {
        let ds = &self.ds;
        let n = ds.pi_pc.len();
        let mut best: Option<u64> = None;
        for &pc in pcs {
            let lo = ds.pi_pc.partition_point(0, n, |p| p < pc);
            let hi = ds.pi_pc.partition_point(lo, n, |p| p <= pc);
            let found = if forward {
                let j = ds.pi_index.partition_point(lo, hi, |i| i <= from);
                (j < hi).then(|| ds.pi_index.get(j))
            } else {
                let j = ds.pi_index.partition_point(lo, hi, |i| i < from);
                (j > lo).then(|| ds.pi_index.get(j - 1))
            };
            if let Some(f) = found {
                best = Some(match best {
                    Some(b) if forward => b.min(f),
                    Some(b) => b.max(f),
                    None => f,
                });
            }
        }
        best
    }

    /// Exception records visible at or before `index`: (at, number, line).
    pub fn exceptions(&self) -> Vec<(u64, Option<u32>, u64)> {
        let ds = &self.ds;
        (0..ds.exc_at.len())
            .map(|i| {
                let n = ds.exc_number.get(i);
                (ds.exc_at.get(i), (n != u32::MAX).then_some(n), ds.exc_line.get(i))
            })
            .collect()
    }

    /// Which memory row kinds exist (for tests and reports).
    pub fn memory_rows(&self) -> (u64, u64) {
        let ds = &self.ds;
        let mut writes = 0;
        let mut reads = 0;
        for c in ds.mem_kind.chunks() {
            for &k in c {
                match k {
                    MEM_WRITE => writes += 1,
                    MEM_READ => reads += 1,
                    _ => {}
                }
            }
        }
        (reads, writes)
    }
}
