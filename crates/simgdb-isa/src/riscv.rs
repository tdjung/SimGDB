//! RISC-V RV32 (Step 1: the ISA definition layer only; PLAN Step 1, ISA).
//!
//! Registers x0-x31 and pc with ABI names, how traces may name them, the GDB
//! target description, and control-flow classification of RV32I/C jumps and
//! branches. No trace provider reads RISC-V traces yet (Step 3); exception
//! handling needs CSRs (mcause, mepc) that are not modelled here.

use crate::{Flow, Isa, RegDef, RegId, RegState, RegWrite, TraceRegName};

const CPU: &str = "org.gnu.gdb.riscv.cpu";

const fn r(name: &'static str, gdb_type: &'static str) -> RegDef {
    RegDef { name, bits: 32, feature: CPU, gdb_type }
}

static REGISTERS: [RegDef; 33] = [
    r("zero", "int"),
    r("ra", "code_ptr"),
    r("sp", "data_ptr"),
    r("gp", "data_ptr"),
    r("tp", "data_ptr"),
    r("t0", "int"),
    r("t1", "int"),
    r("t2", "int"),
    r("fp", "data_ptr"),
    r("s1", "int"),
    r("a0", "int"),
    r("a1", "int"),
    r("a2", "int"),
    r("a3", "int"),
    r("a4", "int"),
    r("a5", "int"),
    r("a6", "int"),
    r("a7", "int"),
    r("s2", "int"),
    r("s3", "int"),
    r("s4", "int"),
    r("s5", "int"),
    r("s6", "int"),
    r("s7", "int"),
    r("s8", "int"),
    r("s9", "int"),
    r("s10", "int"),
    r("s11", "int"),
    r("t3", "int"),
    r("t4", "int"),
    r("t5", "int"),
    r("t6", "int"),
    r("pc", "code_ptr"),
];

pub const PC: RegId = 32;
pub const SP: RegId = 2;
const RA: u32 = 1;

pub struct RiscV32;

impl Isa for RiscV32 {
    fn name(&self) -> &'static str {
        "riscv32"
    }

    fn registers(&self) -> &'static [RegDef] {
        &REGISTERS
    }

    fn pc(&self) -> RegId {
        PC
    }

    fn stack_pointer(&self) -> RegId {
        SP
    }

    fn lookup(&self, name: &TraceRegName<'_>) -> Option<RegId> {
        let n = name.name.to_ascii_lowercase();
        if let Some(k) = n.strip_prefix('x').and_then(|d| d.parse::<u16>().ok()) {
            return (k < 32).then_some(k);
        }
        match n.as_str() {
            "s0" => Some(8),
            _ => REGISTERS.iter().position(|r| r.name == n).map(|k| k as RegId),
        }
    }

    fn resolve(&self, _state: &RegState, _written: &[bool]) -> Vec<RegWrite> {
        // No banked aliases. x0 writes are recorded as written (traces show
        // the attempted value) but x0 reads as zero in hardware; a provider
        // must drop them.
        Vec::new()
    }

    fn target_xml(&self, _never_written: &[String]) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE target SYSTEM \"gdb-target.dtd\">\n<target version=\"1.0\">\n<architecture>riscv:rv32</architecture>\n",
        );
        xml.push_str(&format!("<feature name=\"{CPU}\">\n"));
        for (n, r) in REGISTERS.iter().enumerate() {
            xml.push_str(&format!(
                "<reg name=\"{}\" bitsize=\"32\" regnum=\"{}\" type=\"{}\" group=\"general\"/>\n",
                r.name, n, r.gdb_type
            ));
        }
        xml.push_str("</feature>\n</target>\n");
        xml
    }

    fn volatile_regions(&self) -> &'static [(u64, u64)] {
        // No architectural memory map: platforms give theirs in the config file.
        &[]
    }

    fn exception_frame_max(&self) -> u64 {
        // Traps do not push a frame in hardware.
        0
    }

    fn exception_frame(&self, _after_entry: &RegState) -> Option<(u64, u64)> {
        None
    }

    fn exception_return_slot(&self, _after_entry: &RegState) -> Option<(u64, u64)> {
        None
    }

    fn address_limit(&self) -> u64 {
        1 << 32
    }

    fn little_endian(&self) -> bool {
        true
    }

    fn exception_number(&self, _state: &RegState) -> Option<u32> {
        // mcause is a CSR, not in this register file yet.
        None
    }

    fn exception_name(&self, _number: u32) -> Option<String> {
        None
    }

    fn flow(&self, opcode: u32, width: u8) -> Flow {
        classify(opcode, width)
    }
}

/// Control flow of one RV32I/RV32C instruction (calls and returns by the
/// standard ABI use of ra).
pub fn classify(op: u32, width: u8) -> Flow {
    if width == 2 || op & 3 != 3 {
        let c = op as u16;
        let funct3 = c >> 13;
        let quadrant = c & 3;
        return match (quadrant, funct3) {
            (1, 0b001) => Flow::Call,                     // c.jal (RV32)
            (1, 0b101) => Flow::Jump,                     // c.j
            (1, 0b110) | (1, 0b111) => Flow::Conditional, // c.beqz, c.bnez
            (2, 0b100) if c & 0x7c != 0 => Flow::Next,    // c.mv / c.add
            (2, 0b100) => {
                let rs1 = ((c >> 7) & 0x1f) as u32;
                match (c >> 12 & 1, rs1) {
                    (_, 0) => Flow::Next,    // reserved / c.ebreak
                    (0, RA) => Flow::Return, // c.jr ra
                    (0, _) => Flow::Jump,    // c.jr
                    (1, _) => Flow::Call,    // c.jalr
                    _ => Flow::Next,
                }
            }
            _ => Flow::Next,
        };
    }
    let rd = (op >> 7) & 0x1f;
    let rs1 = (op >> 15) & 0x1f;
    match op & 0x7f {
        0x6f => {
            // jal
            if rd == 0 { Flow::Jump } else { Flow::Call }
        }
        0x67 => {
            // jalr
            match (rd, rs1) {
                (0, RA) => Flow::Return,
                (0, _) => Flow::Jump,
                _ => Flow::Call,
            }
        }
        0x63 => Flow::Conditional,
        _ => Flow::Next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_names() {
        let isa = RiscV32;
        let l = |n: &str| isa.lookup(&TraceRegName { name: n, bank: None });
        assert_eq!(
            (l("x0"), l("zero"), l("X2"), l("sp"), l("s0"), l("fp"), l("x8")),
            (Some(0), Some(0), Some(2), Some(2), Some(8), Some(8), Some(8))
        );
        assert_eq!((l("a0"), l("t6"), l("pc"), l("x32"), l("mcause")), (Some(10), Some(31), Some(32), None, None));
        let xml = isa.target_xml(&[]);
        assert!(xml.contains("org.gnu.gdb.riscv.cpu") && xml.contains("name=\"pc\" bitsize=\"32\" regnum=\"32\""));
    }

    #[test]
    fn control_flow() {
        assert_eq!(classify(0x0080_00ef, 4), Flow::Call); // jal ra, +8
        assert_eq!(classify(0x0080_006f, 4), Flow::Jump); // j +8
        assert_eq!(classify(0x0000_8067, 4), Flow::Return); // ret (jalr x0, 0(ra))
        assert_eq!(classify(0x0002_8067, 4), Flow::Jump); // jr t0
        assert_eq!(classify(0x0002_80e7, 4), Flow::Call); // jalr ra, 0(t0)
        assert_eq!(classify(0x00b5_0463, 4), Flow::Conditional); // beq a0, a1, +8
        assert_eq!(classify(0x0015_0513, 4), Flow::Next); // addi a0, a0, 1
        assert_eq!(classify(0x8082, 2), Flow::Return); // c.jr ra (ret)
        assert_eq!(classify(0x8282, 2), Flow::Jump); // c.jr t0
        assert_eq!(classify(0x9282, 2), Flow::Call); // c.jalr t0
        assert_eq!(classify(0xa001, 2), Flow::Jump); // c.j
        assert_eq!(classify(0xc111, 2), Flow::Conditional); // c.beqz a0
        assert_eq!(classify(0x852e, 2), Flow::Next); // c.mv a0, a1
        assert_eq!(classify(0x9002, 2), Flow::Next); // c.ebreak
    }
}
