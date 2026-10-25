//! ISA definition layer (D6, ARCHITECTURE §2.9).
//!
//! Engine and front ends know registers only through `Isa`: the register
//! list, how trace names map onto it, rules that keep aliases consistent
//! (banked stack pointers), and the GDB target description.

pub mod arm_m;

use serde::Serialize;

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

    pub fn get(&self, reg: RegId) -> Option<u64> {
        let r = reg as usize;
        (self.known[r] != 0).then_some(self.values[r])
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
    /// GDB target description XML.
    fn target_xml(&self) -> String;
    /// Default memory map: regions whose contents change without core
    /// accesses (peripherals), for which a remembered value is not trusted.
    fn volatile_regions(&self) -> &'static [(u64, u64)];
    /// Bytes an exception entry may write below the stack pointer.
    fn exception_frame_bytes(&self) -> u64;
}

pub fn by_name(name: &str) -> Option<Box<dyn Isa>> {
    match name {
        "arm-m" | "armv7-m" | "armv8-m" | "cortex-m4" | "cortex-m35p" | "cortex-m33" => Some(Box::new(arm_m::ArmM)),
        _ => None,
    }
}
