//! SimGDB index: built once from a trace, then read through mmap.
//!
//! A dataset directory holds one run of one core (Step 1):
//!
//! | file | rows | columns |
//! |---|---|---|
//! | `instructions.arrow` | instruction records | pc, opcode, width, flags, time, line |
//! | `reg_writes.arrow` | register writes, in trace order | at, reg, value, known, line, derived |
//! | `mem.arrow` | memory accesses and exception clobbers | at, addr, size, kind, value, known, line |
//! | `mem_index.arrow` | one row per byte of each access, sorted | addr, at, row |
//! | `pc_index.arrow` | one row per instruction, sorted | pc, index |
//! | `checkpoints.arrow` | every `checkpoint_interval` points | index, rw_row, per register v/k/row |
//! | `exceptions.arrow` | exception records | at, number, kind, target, line |
//! | `image.arrow` | ELF load image segments | addr, data |
//! | `meta.sqlite` | metadata, capabilities, diagnostics | key, value (JSON) |
//!
//! `at` is the point from which a record is visible: records after
//! instruction i are visible from point i+1; records before the first
//! instruction from point 0; memory reads from the point of the reading
//! instruction (the value was there before it read it).

pub mod annotations;
pub mod calls;
pub mod config;
mod index;
pub mod sort;
pub mod sources;
mod table;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arrow_array::types::{UInt8Type, UInt16Type, UInt32Type, UInt64Type};
use serde::{Deserialize, Serialize};
use simgdb_model::TimeUnit;

pub use index::{IndexOptions, build};
pub use table::{BATCH, Col, Table};

pub const FORMAT_VERSION: u32 = 3;

/// Instruction flags.
pub const FLAG_FAILED: u8 = 1;
pub const FLAG_FETCH_FAILED: u8 = 2;

/// Memory row kinds.
pub const MEM_READ: u8 = 0;
pub const MEM_WRITE: u8 = 1;
/// Bytes an exception entry may have overwritten without a trace record.
pub const MEM_CLOBBER: u8 = 2;
/// Bytes another core wrote (multi-core traces indexed one core at a time).
pub const MEM_FOREIGN: u8 = 3;

pub const NONE: u64 = u64::MAX;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("elf: {0}")]
    Elf(#[from] simgdb_elf::ElfError),
    #[error("index is corrupt or from another version: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Trace(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl StoreError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        StoreError::Io { path: path.to_path_buf(), source }
    }
}

/// What this dataset can answer (ARCHITECTURE §2.6).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub register_writes: bool,
    pub memory_values: bool,
    pub memory_writes: bool,
    pub exceptions: bool,
    /// The trace records the stores of exception entry (stacking). Without
    /// them the frame is reconstructed as unknown, not guessed.
    pub exception_stacking_writes: bool,
    pub time_unit: Option<TimeUnit>,
    pub elf: bool,
    /// Source lines from DWARF.
    pub source_lines: bool,
}

/// One kind of line the parser did not interpret, with sample lines.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unparsed {
    pub count: u64,
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sample {
    pub line: u64,
    pub text: String,
}

/// Diagnostics for `index --report` (ARCHITECTURE §2.14).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub lines: u64,
    pub instructions: u64,
    pub condition_failed: u64,
    pub fetch_failed: u64,
    /// `IF` records repeating the previous instruction, not indexed.
    pub if_duplicates: u64,
    pub register_writes: u64,
    /// Writes added to keep aliases consistent (SP and its bank).
    pub derived_register_writes: u64,
    pub memory_reads: u64,
    pub memory_writes: u64,
    pub exceptions: u64,
    /// Register names the ISA does not model, with counts.
    pub unmodelled_registers: BTreeMap<String, u64>,
    /// Records the parser knows have no effect on state.
    pub ignored: BTreeMap<String, u64>,
    /// Record types the parser does not know.
    pub unrecognised: BTreeMap<String, Unparsed>,
    pub parse_errors: Unparsed,
    /// Lines from cores other than the indexed one.
    pub other_cores: BTreeMap<u32, u64>,
    /// Memory writes by other cores: those bytes become unknown for this core.
    pub other_core_writes: u64,
    /// Exception entries whose frame could not be placed (stack pointer or
    /// EXC_RETURN unknown): stack values after them may be stale.
    pub exception_frames_unplaced: u64,
    /// Tail-chained exception entries the trace has no record for, inferred
    /// from the exception level after a handler returned.
    #[serde(default)]
    pub exceptions_inferred: u64,
    pub time_unit: Option<TimeUnit>,
    pub first_time: Option<u64>,
    pub last_time: Option<u64>,
    /// Instruction time stamps never decrease.
    pub time_monotonic: bool,
    /// Registers (by name) that were never written.
    pub registers_never_written: Vec<String>,
    /// Registers (by name) not fully known at some point: never written,
    /// unknown before their first write, or written with unknown bits.
    /// `None` in indexes built before this was recorded.
    #[serde(default)]
    pub registers_not_always_known: Option<Vec<String>>,
    /// Source attribution of the ELF code.
    pub sources: sources::SourceSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub format_version: u32,
    pub isa: String,
    pub run: u32,
    pub core: u32,
    pub trace: String,
    pub elf: Option<String>,
    pub instructions: u64,
    pub checkpoint_interval: u64,
    pub registers: Vec<String>,
    pub volatile_regions: Vec<(u64, u64)>,
    pub capabilities: Capabilities,
    pub report: Report,
    /// The site configuration the index was built with.
    #[serde(default)]
    pub config: Option<config::SiteConfig>,
}

/// A dataset opened for reading.
pub struct Dataset {
    pub dir: PathBuf,
    pub meta: Meta,
    pub insn_pc: Col<UInt64Type>,
    pub insn_opcode: Col<UInt32Type>,
    pub insn_width: Col<UInt8Type>,
    pub insn_flags: Col<UInt8Type>,
    pub insn_time: Col<UInt64Type>,
    pub insn_line: Col<UInt64Type>,
    pub rw_at: Col<UInt64Type>,
    pub rw_reg: Col<UInt16Type>,
    pub rw_value: Col<UInt64Type>,
    pub rw_known: Col<UInt64Type>,
    pub rw_line: Col<UInt64Type>,
    pub rw_derived: Col<UInt8Type>,
    pub mem_at: Col<UInt64Type>,
    pub mem_addr: Col<UInt64Type>,
    pub mem_size: Col<UInt32Type>,
    pub mem_kind: Col<UInt8Type>,
    pub mem_value: Col<UInt64Type>,
    pub mem_known: Col<UInt8Type>,
    pub mem_line: Col<UInt64Type>,
    pub mi_addr: Col<UInt64Type>,
    pub mi_at: Col<UInt64Type>,
    pub mi_row: Col<UInt32Type>,
    pub pi_pc: Col<UInt64Type>,
    pub pi_index: Col<UInt64Type>,
    pub cp_index: Col<UInt64Type>,
    pub cp_rw_row: Col<UInt64Type>,
    /// Per register: value, known mask, reg_writes row of the last write.
    pub cp_regs: Vec<(Col<UInt64Type>, Col<UInt64Type>, Col<UInt64Type>)>,
    pub exc_at: Col<UInt64Type>,
    pub exc_number: Col<UInt32Type>,
    pub exc_kind: Col<UInt8Type>,
    pub exc_target: Col<UInt64Type>,
    pub exc_line: Col<UInt64Type>,
    /// ELF load image, sorted by address.
    pub image: Vec<(u64, Vec<u8>)>,
    pub insn_frame: Col<UInt32Type>,
    pub call_kind: Col<UInt8Type>,
    pub call_index: Col<UInt64Type>,
    pub call_entry: Col<UInt64Type>,
    pub call_return: Col<UInt64Type>,
    pub call_pc: Col<UInt64Type>,
    pub call_return_pc: Col<UInt64Type>,
    pub call_number: Col<UInt32Type>,
    pub call_parent: Col<UInt32Type>,
    pub call_depth: Col<UInt32Type>,
    pub src_pc: Col<UInt64Type>,
    pub src_file: Col<UInt32Type>,
    pub src_function: Col<UInt32Type>,
    pub src_line: Col<UInt32Type>,
    pub src_placement: Col<UInt8Type>,
    pub src_frame_start: Col<UInt32Type>,
    pub src_frame_count: Col<UInt32Type>,
    pub fr_function: Col<UInt32Type>,
    pub fr_file: Col<UInt32Type>,
    pub fr_line: Col<UInt32Type>,
    pub strings: Vec<String>,
    pub variables: simgdb_elf::variables::Variables,
    pub symbols: Vec<sources::SymbolEntry>,
}

impl Dataset {
    pub fn open(dir: &Path) -> Result<Dataset, StoreError> {
        let db =
            rusqlite::Connection::open_with_flags(dir.join("meta.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let text: String = db.query_row("SELECT value FROM meta WHERE key = 'meta'", [], |r| r.get(0))?;
        let meta: Meta = serde_json::from_str(&text)?;
        if meta.format_version != FORMAT_VERSION {
            return Err(StoreError::Corrupt(format!(
                "format version {} (this SimGDB reads {FORMAT_VERSION}); re-run `simgdb index`",
                meta.format_version
            )));
        }
        let t = |name: &str| Table::open(&dir.join(format!("{name}.arrow")));
        let insn = t("instructions")?;
        let rw = t("reg_writes")?;
        let mem = t("mem")?;
        let mi = t("mem_index")?;
        let pi = t("pc_index")?;
        let cp = t("checkpoints")?;
        let exc = t("exceptions")?;
        let img = t("image")?;
        let mut cp_regs = Vec::new();
        for k in 0..meta.registers.len() {
            cp_regs.push((cp.col(&format!("v{k}"))?, cp.col(&format!("k{k}"))?, cp.col(&format!("w{k}"))?));
        }
        let calls = t("calls")?;
        let src = t("sources")?;
        let fr = t("frames")?;
        let strings =
            t("strings")?.binary("s")?.into_iter().map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
        let image_addr = img.col::<UInt64Type>("addr")?;
        let image_data = img.binary("data")?;
        let image = (0..image_addr.len()).map(|i| image_addr.get(i)).zip(image_data).collect();
        Ok(Dataset {
            dir: dir.to_path_buf(),
            insn_pc: insn.col("pc")?,
            insn_opcode: insn.col("opcode")?,
            insn_width: insn.col("width")?,
            insn_flags: insn.col("flags")?,
            insn_time: insn.col("time")?,
            insn_line: insn.col("line")?,
            rw_at: rw.col("at")?,
            rw_reg: rw.col("reg")?,
            rw_value: rw.col("value")?,
            rw_known: rw.col("known")?,
            rw_line: rw.col("line")?,
            rw_derived: rw.col("derived")?,
            mem_at: mem.col("at")?,
            mem_addr: mem.col("addr")?,
            mem_size: mem.col("size")?,
            mem_kind: mem.col("kind")?,
            mem_value: mem.col("value")?,
            mem_known: mem.col("known")?,
            mem_line: mem.col("line")?,
            mi_addr: mi.col("addr")?,
            mi_at: mi.col("at")?,
            mi_row: mi.col("row")?,
            pi_pc: pi.col("pc")?,
            pi_index: pi.col("index")?,
            cp_index: cp.col("index")?,
            cp_rw_row: cp.col("rw_row")?,
            cp_regs,
            exc_at: exc.col("at")?,
            exc_number: exc.col("number")?,
            exc_kind: exc.col("kind")?,
            exc_target: exc.col("target")?,
            exc_line: exc.col("line")?,
            image,
            insn_frame: insn.col("frame")?,
            call_kind: calls.col("kind")?,
            call_index: calls.col("call_index")?,
            call_entry: calls.col("entry_index")?,
            call_return: calls.col("return_index")?,
            call_pc: calls.col("call_pc")?,
            call_return_pc: calls.col("return_pc")?,
            call_number: calls.col("number")?,
            call_parent: calls.col("parent")?,
            call_depth: calls.col("depth")?,
            src_pc: src.col("pc")?,
            src_file: src.col("file")?,
            src_function: src.col("function")?,
            src_line: src.col("line")?,
            src_placement: src.col("placement")?,
            src_frame_start: src.col("frame_start")?,
            src_frame_count: src.col("frame_count")?,
            fr_function: fr.col("function")?,
            fr_file: fr.col("file")?,
            fr_line: fr.col("line")?,
            strings,
            variables: sources::read_variables(dir)?,
            symbols: sources::read_symbols(dir)?,
            meta,
        })
    }

    pub fn instructions(&self) -> u64 {
        self.meta.instructions
    }
}
