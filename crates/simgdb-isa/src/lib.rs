//! ISA definition layer (D6, ARCHITECTURE §2.9).
//!
//! Engine and front ends know registers only through `Isa`: the register
//! list, how trace names map onto it, rules that keep aliases consistent
//! (banked stack pointers), and the GDB target description.

pub mod arm_m;
pub mod riscv;
pub mod thumb;

pub use thumb::Flow;

use serde::{Deserialize, Serialize};

/// Index into `Isa::registers()`.
pub type RegId = u16;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RegDef {
    pub name: &'static str,
    pub bits: u32,
    /// GDB target-description feature this register belongs to.
    pub feature: &'static str,
    /// GDB type name in the target description.
    pub gdb_type: &'static str,
}

/// How a trace record names a register, after parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRegName<'a> {
    pub name: &'a str,
    /// Bank annotation printed next to the value, e.g. `(MSP)` in
    /// `R r13 20001fff (MSP)`.
    pub bank: Option<&'a str>,
}

/// A register write after normalisation: a definite register, value, and
/// which bits are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegWrite {
    pub reg: RegId,
    pub value: u64,
    pub known: u64,
    /// True when the write was derived by an alias rule rather than written
    /// in the trace (e.g. SP following MSP).
    pub derived: bool,
}

/// Register file during indexing or replay: value and known-bit mask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegState {
    pub values: Vec<u64>,
    pub known: Vec<u64>,
}

impl RegState {
    pub fn new(n: usize) -> Self {
        RegState { values: vec![0; n], known: vec![0; n] }
    }

    /// Value when every bit of the register is known.
    pub fn get(&self, reg: RegId, bits: u32) -> Option<u64> {
        let r = reg as usize;
        let full = RegState::full_mask(bits);
        (self.known[r] & full == full).then_some(self.values[r])
    }

    pub fn full_mask(bits: u32) -> u64 {
        if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
    }

    /// A write replaces the whole register: bits it marks unknown (`x` digits
    /// in the trace) become unknown.
    pub fn apply(&mut self, w: &RegWrite) {
        let r = w.reg as usize;
        self.values[r] = w.value & w.known;
        self.known[r] = w.known;
    }
}

/// Byte order of data in memory. Instruction fetches follow the ISA (Arm
/// BE8 and RISC-V code is little-endian either way); this is for data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    #[default]
    Little,
    Big,
}

impl Endian {
    pub fn parse(s: &str) -> Option<Endian> {
        match s.to_ascii_lowercase().as_str() {
            "little" | "le" => Some(Endian::Little),
            "big" | "be" | "be8" => Some(Endian::Big),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Endian::Little => "little",
            Endian::Big => "big",
        }
    }

    /// Byte order of a data access at `addr` on `isa`.
    pub fn at(self, isa: &dyn Isa, addr: u64) -> Endian {
        if self == Endian::Big && isa.little_endian_regions().iter().any(|&(lo, hi)| addr >= lo && addr < hi) {
            Endian::Little
        } else {
            self
        }
    }

    /// `bytes` (by address, at most 8) as a number.
    pub fn value(self, bytes: &[u8]) -> u64 {
        let it = bytes.iter().map(|&b| b as u64);
        match self {
            Endian::Little => it.rev().fold(0, |a, b| (a << 8) | b),
            Endian::Big => it.fold(0, |a, b| (a << 8) | b),
        }
    }

    /// Shift of the byte at offset `k` of an `n`-byte number.
    pub fn shift(self, k: usize, n: usize) -> u32 {
        8 * match self {
            Endian::Little => k,
            Endian::Big => n - 1 - k,
        } as u32
    }
}

pub trait Isa: Send + Sync {
    fn name(&self) -> &'static str;
    fn registers(&self) -> &'static [RegDef];
    /// Program counter register (taken from instruction records, not writes).
    fn pc(&self) -> RegId;
    /// Stack pointer the frame of an exception entry is pushed on, given the
    /// register state just before entry.
    fn stack_pointer(&self) -> RegId;
    /// Map a trace register name onto the register file. `None` for names
    /// this ISA does not model (they are reported, not dropped silently).
    fn lookup(&self, name: &TraceRegName<'_>) -> Option<RegId>;
    /// Writes that keep aliases consistent (e.g. SP and the active banked
    /// SP) after a block of trace writes: everything recorded between two
    /// instructions, or before an exception record. `state` already has the
    /// block's writes applied; `written[r]` says which registers the block
    /// wrote. Resolving per block, not per write, matters because traces
    /// order a block's records arbitrarily (SP before xPSR on exception
    /// return, for example).
    fn resolve(&self, state: &RegState, written: &[bool]) -> Vec<RegWrite>;
    /// GDB target description XML. `not_always_known` lists registers that
    /// are unknown at some point of the trace (never written, or unknown
    /// before their first write); an ISA may describe them differently so
    /// GDB does not depend on values it cannot always get.
    fn target_xml(&self, not_always_known: &[String]) -> String;
    /// Default memory map: regions whose contents change without core
    /// accesses (peripherals), for which a remembered value is not trusted.
    fn volatile_regions(&self) -> &'static [(u64, u64)];
    /// Most bytes an exception entry may write below the stack pointer in
    /// use before entry (any frame format, with alignment padding).
    fn exception_frame_max(&self) -> u64;
    /// Where the frame of an exception entry lies, from the register state
    /// right after entry: `(base, size)`. `None` when that state does not
    /// say (e.g. the stack selector is unknown).
    fn exception_frame(&self, after_entry: &RegState) -> Option<(u64, u64)>;
    /// Where an exception entry stacked the interrupted PC, and its size in
    /// bytes, from the register state right after entry.
    fn exception_return_slot(&self, after_entry: &RegState) -> Option<(u64, u64)>;
    /// Addresses are below this limit (e.g. 2^32 for a 32-bit ISA).
    fn address_limit(&self) -> u64;
    /// Data byte order when neither the ELF nor the user says.
    fn default_endian(&self) -> Endian {
        Endian::Little
    }
    /// Address ranges `[lo, hi)` whose data accesses are little-endian
    /// whatever the core's data byte order (Arm M-profile: the PPB).
    fn little_endian_regions(&self) -> &'static [(u64, u64)] {
        &[]
    }
    /// Exception being handled in this register state (0: none, thread
    /// mode), if the state shows it.
    fn exception_number(&self, state: &RegState) -> Option<u32>;
    /// Architectural name of an exception number (e.g. "SysTick", "IRQ3").
    fn exception_name(&self, number: u32) -> Option<String>;
    /// Control-flow class of an executed instruction from its trace encoding.
    fn flow(&self, opcode: u32, width: u8) -> Flow;
    /// System registers an instruction writes by its encoding (Arm `MSR`,
    /// `CPS`), so a trace that never records such a register can be told
    /// apart from firmware that never changes it.
    fn system_register_writes(&self, _opcode: u32, _width: u8) -> &'static [RegId] {
        &[]
    }
}

pub fn by_name(name: &str) -> Option<Box<dyn Isa>> {
    match name {
        "arm-m" | "armv7-m" | "armv8-m" | "cortex-m4" | "cortex-m35p" | "cortex-m33" => Some(Box::new(arm_m::ArmM)),
        "riscv32" | "rv32" => Some(Box::new(riscv::RiscV32)),
        _ => None,
    }
}
