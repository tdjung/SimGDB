//! Arm M-profile (Armv7-M, Armv8-M Mainline): Cortex-M4, Cortex-M35P.

use crate::{Fields, GdbRegister, Isa, RegDef, RegId, RegState, RegWrite, TraceRegName};

pub struct ArmM;

pub const R0: RegId = 0;
pub const SP: RegId = 13;
pub const LR: RegId = 14;
pub const PC: RegId = 15;
pub const XPSR: RegId = 16;
pub const MSP: RegId = 17;
pub const PSP: RegId = 18;
pub const PRIMASK: RegId = 19;
pub const BASEPRI: RegId = 20;
pub const FAULTMASK: RegId = 21;
pub const CONTROL: RegId = 22;
pub const MSPLIM: RegId = 23;
pub const PSPLIM: RegId = 24;
/// S0..S31 are `S0 + n`; D<n> is S<2n+1>:S<2n>.
pub const S0: RegId = 25;
pub const FPSCR: RegId = 57;

const CORE: &str = "org.gnu.gdb.arm.m-profile";
/// Feature of MSP, PSP and the special registers. With this standard name
/// GDB (13 and later) unwinds exception frames on either stack, but reads
/// both MSP and PSP whenever it builds a frame without CFI and for every
/// exception frame; one of them unavailable fails the whole backtrace
/// (`Register 92 is not available`), which IDE front ends treat as fatal.
const SYSTEM: &str = "org.gnu.gdb.arm.m-system";
/// Used instead when MSP or PSP is unknown at some point the session cares
/// about: PSP is UNKNOWN after reset, bare-metal firmware often never sets
/// it, and an RTOS sets it only when the scheduler starts. GDB then needs
/// neither, and the registers still show in `info registers`. Its cost: GDB
/// no longer switches to the process stack when it unwinds an exception
/// frame, so a backtrace from a handler that interrupted a thread on PSP
/// goes wrong below that frame (SimGDB's own call stack, `monitor where`,
/// is not affected). Without any PSP write such frames cannot occur.
const SYSTEM_NO_PSP: &str = "org.simgdb.arm.m-system";

/// Floating-point registers (FPv4-SP, FPv5: S0-S31 and FPSCR). Stored as
/// the single-precision registers because traces write either form and a
/// `D` record may write one half only (`R D2 --------37800000`), so each
/// half is known on its own (queries, `monitor`, MCP).
///
/// GDB gets the standard `org.gnu.gdb.arm.vfp` feature, D0-D15 and FPSCR,
/// and makes S0-S31 from them itself. GDB needs the D registers: compilers
/// describe `vpush {d8-d9}` in the call frame information by D register,
/// and GDB cannot unwind such a frame without them (nor show callers'
/// S16-S31). The cost: a D register whose halves are not both known is
/// unavailable to GDB, and so are both its S registers.
const FP: &str = "org.gnu.gdb.arm.vfp";

const D_NAMES: [&str; 16] =
    ["d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10", "d11", "d12", "d13", "d14", "d15"];

macro_rules! reg {
    ($name:expr, $feature:expr, $ty:expr) => {
        RegDef { name: $name, bits: 32, feature: $feature, gdb_type: $ty }
    };
}

static REGISTERS: [RegDef; 58] = [
    reg!("r0", CORE, "int"),
    reg!("r1", CORE, "int"),
    reg!("r2", CORE, "int"),
    reg!("r3", CORE, "int"),
    reg!("r4", CORE, "int"),
    reg!("r5", CORE, "int"),
    reg!("r6", CORE, "int"),
    reg!("r7", CORE, "int"),
    reg!("r8", CORE, "int"),
    reg!("r9", CORE, "int"),
    reg!("r10", CORE, "int"),
    reg!("r11", CORE, "int"),
    reg!("r12", CORE, "int"),
    reg!("sp", CORE, "data_ptr"),
    reg!("lr", CORE, "int"),
    reg!("pc", CORE, "code_ptr"),
    reg!("xpsr", CORE, "int"),
    reg!("msp", SYSTEM, "int"),
    reg!("psp", SYSTEM, "int"),
    reg!("primask", SYSTEM, "int"),
    reg!("basepri", SYSTEM, "int"),
    reg!("faultmask", SYSTEM, "int"),
    reg!("control", SYSTEM, "int"),
    // Armv8-M stack limits; never written on Armv7-M.
    reg!("msplim", SYSTEM, "int"),
    reg!("psplim", SYSTEM, "int"),
    reg!("s0", FP, "ieee_single"),
    reg!("s1", FP, "ieee_single"),
    reg!("s2", FP, "ieee_single"),
    reg!("s3", FP, "ieee_single"),
    reg!("s4", FP, "ieee_single"),
    reg!("s5", FP, "ieee_single"),
    reg!("s6", FP, "ieee_single"),
    reg!("s7", FP, "ieee_single"),
    reg!("s8", FP, "ieee_single"),
    reg!("s9", FP, "ieee_single"),
    reg!("s10", FP, "ieee_single"),
    reg!("s11", FP, "ieee_single"),
    reg!("s12", FP, "ieee_single"),
    reg!("s13", FP, "ieee_single"),
    reg!("s14", FP, "ieee_single"),
    reg!("s15", FP, "ieee_single"),
    reg!("s16", FP, "ieee_single"),
    reg!("s17", FP, "ieee_single"),
    reg!("s18", FP, "ieee_single"),
    reg!("s19", FP, "ieee_single"),
    reg!("s20", FP, "ieee_single"),
    reg!("s21", FP, "ieee_single"),
    reg!("s22", FP, "ieee_single"),
    reg!("s23", FP, "ieee_single"),
    reg!("s24", FP, "ieee_single"),
    reg!("s25", FP, "ieee_single"),
    reg!("s26", FP, "ieee_single"),
    reg!("s27", FP, "ieee_single"),
    reg!("s28", FP, "ieee_single"),
    reg!("s29", FP, "ieee_single"),
    reg!("s30", FP, "ieee_single"),
    reg!("s31", FP, "ieee_single"),
    reg!("fpscr", FP, "int"),
];

/// GDB types follow the descriptions QEMU and GDB ship for M-profile, so
/// `info registers` prints the same way against SimGDB and a real target.
///
/// Lower-case trace names (Fast Models, RTL tracers, ES dialect) → register.
/// Secure/Non-secure banked names map to the single bank SimGDB models.
fn alias(name: &str) -> Option<RegId> {
    let r = match name {
        "r13" | "sp" => SP,
        "r14" | "lr" => LR,
        "r15" | "pc" => PC,
        "xpsr" | "psr" | "cpsr" => XPSR,
        // Secure (`_s`) names are the banks SimGDB models. Non-secure banks
        // (`_ns`) are separate registers on Armv8-M with TrustZone and are
        // not modelled yet: they are reported, never merged into these.
        // `r13_mn`: Cortex-M4 RTL (`R r13_mn 20038000 IntInvoke:0 BE8:1`).
        "msp" | "sp_main" | "r13_main" | "r13_mn" | "msp_s" | "r13_main_s" => MSP,
        "psp" | "sp_process" | "r13_process" | "psp_s" | "r13_process_s" => PSP,
        "primask" | "primask_s" => PRIMASK,
        "basepri" | "basepri_s" => BASEPRI,
        "faultmask" | "faultmask_s" => FAULTMASK,
        "control" | "control_s" => CONTROL,
        "msplim" | "msplim_s" => MSPLIM,
        "psplim" | "psplim_s" => PSPLIM,
        "fpscr" => FPSCR,
        _ => {
            let (limit, base) = match name.as_bytes().first()? {
                b'r' => (12, 0),
                b's' => (31, S0),
                _ => return None,
            };
            base + number(&name[1..]).filter(|&n| n <= limit)?
        }
    };
    Some(r)
}

/// Decimal register number (digits only: `r1a` and `r+1` are not registers).
fn number(s: &str) -> Option<u16> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl ArmM {
    /// Which banked SP is active: MSP in Handler mode or when CONTROL.SPSEL
    /// is 0, else PSP. `None` when IPSR or CONTROL is not known.
    pub fn active_bank(state: &RegState) -> Option<RegId> {
        let xpsr_known = state.known[XPSR as usize] & 0x1ff == 0x1ff;
        let control_known = state.known[CONTROL as usize] & 0x2 == 0x2;
        if !xpsr_known {
            return None;
        }
        if state.values[XPSR as usize] & 0x1ff != 0 {
            return Some(MSP);
        }
        if !control_known {
            return None;
        }
        Some(if state.values[CONTROL as usize] & 0x2 != 0 { PSP } else { MSP })
    }
}

impl Isa for ArmM {
    fn name(&self) -> &'static str {
        "arm-m"
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
        let lower = name.name.to_ascii_lowercase();
        if (lower == "r13" || lower == "sp")
            && let Some(bank) = name.bank
        {
            match bank.to_ascii_lowercase().as_str() {
                "msp" | "main" => return Some(MSP),
                "psp" | "process" => return Some(PSP),
                _ => {}
            }
        }
        alias(&lower)
    }

    fn lookup_fields(&self, name: &TraceRegName<'_>) -> Option<Fields> {
        if let Some(n) = name.name.strip_prefix(['d', 'D']).and_then(number).filter(|&n| n <= 15) {
            return Some(Fields::pair(S0 + 2 * n, S0 + 2 * n + 1, 32));
        }
        self.lookup(name).map(Fields::one)
    }

    fn resolve(&self, state: &RegState, written: &[bool]) -> Vec<RegWrite> {
        let (sp, msp, psp) = (SP as usize, MSP as usize, PSP as usize);
        let copy = |reg: RegId, from: usize| RegWrite {
            reg,
            value: state.values[from],
            known: state.known[from],
            derived: true,
        };
        let unknown = |reg: RegId| RegWrite { reg, value: 0, known: 0, derived: true };
        let mut out = Vec::new();
        match ArmM::active_bank(state) {
            Some(bank) => {
                let b = bank as usize;
                if written[sp] && !written[b] {
                    // SP was written: that is the active bank (unknown bits too).
                    out.push(copy(bank, sp));
                } else if !written[sp] {
                    // The bank, the mode or SPSEL changed: SP is the active
                    // bank, unknown if that bank is.
                    out.push(copy(SP, b));
                }
            }
            None => {
                // Mode or SPSEL unknown: which bank SP is cannot be decided.
                // Anything that may have gone stale becomes unknown.
                if written[sp] {
                    for bank in [MSP, PSP] {
                        if !written[bank as usize] {
                            out.push(unknown(bank));
                        }
                    }
                } else if written[msp] || written[psp] || written[XPSR as usize] || written[CONTROL as usize] {
                    out.push(unknown(SP));
                }
            }
        }
        out.retain(|w| state.values[w.reg as usize] != w.value || state.known[w.reg as usize] != w.known);
        out
    }

    fn target_xml(&self, not_always_known: &[String]) -> String {
        let system = if not_always_known.iter().any(|r| r == "psp" || r == "msp") { SYSTEM_NO_PSP } else { SYSTEM };
        let mut xml = String::from(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE target SYSTEM \"gdb-target.dtd\">\n<target version=\"1.0\">\n<architecture>arm</architecture>\n",
        );
        let mut feature = "";
        for (n, g) in self.gdb_registers().iter().enumerate() {
            let r = &REGISTERS[g.first as usize];
            if r.feature != feature {
                if !feature.is_empty() {
                    xml.push_str("</feature>\n");
                }
                feature = r.feature;
                let name = if feature == SYSTEM { system } else { feature };
                xml.push_str(&format!("<feature name=\"{name}\">\n"));
            }
            // As GDB's own arm/vfp descriptions: D registers as doubles in
            // the float group by type, FPSCR an integer in the float group.
            let (bits, ty, group) = if g.count == 2 {
                (64, "ieee_double", "float")
            } else {
                (r.bits, r.gdb_type, if r.feature == FP { "float" } else { "general" })
            };
            xml.push_str(&format!(
                "<reg name=\"{}\" bitsize=\"{bits}\" regnum=\"{n}\" type=\"{ty}\" group=\"{group}\"/>\n",
                g.name
            ));
        }
        xml.push_str("</feature>\n</target>\n");
        xml
    }

    fn gdb_registers(&self) -> Vec<GdbRegister> {
        let one = |n: RegId| GdbRegister { name: REGISTERS[n as usize].name, first: n, count: 1 };
        (0..S0)
            .map(one)
            .chain(D_NAMES.iter().enumerate().map(|(n, &name)| GdbRegister {
                name,
                first: S0 + 2 * n as RegId,
                count: 2,
            }))
            .chain([one(FPSCR)])
            .collect()
    }

    fn volatile_regions(&self) -> &'static [(u64, u64)] {
        // Armv7-M/v8-M default memory map: Peripheral, Device, PPB/vendor.
        &[(0x4000_0000, 0x6000_0000), (0xA000_0000, 0x1_0000_0000)]
    }

    fn exception_frame_max(&self) -> u64 {
        // Armv8-M with TrustZone: additional state (0x28) + basic (0x20) +
        // FP (0x48) + Secure FP (0x40) = 0xd0, plus a 4-byte alignment pad.
        // Bytes below the pre-entry SP are not live, so over-covering is safe.
        0xd8
    }

    fn exception_frame(&self, after: &RegState) -> Option<(u64, u64)> {
        // EXC_RETURN in LR: bit 2 SPSEL (frame on PSP), bit 4 FType (0 = FP
        // state stacked), bit 5 DCRS and bit 6 S (Armv8-M: 0 DCRS with S set
        // means the callee registers were stacked as additional state).
        let lr = after.get(LR, 32)?;
        if lr >> 24 != 0xff {
            return None;
        }
        let bank = if lr & 4 != 0 { PSP } else { MSP };
        let base = after.get(bank, 32)?;
        let mut size = 0x20;
        if lr & (1 << 4) == 0 {
            // FP context. Secure FP state (FPCCR.TS) is not visible and bit 6
            // is also 1 on Armv7-M, so it is assumed stacked: this may hide
            // valid bytes above a smaller frame but never shows stale ones.
            size += 0x48 + if lr & (1 << 6) != 0 { 0x40 } else { 0 };
        }
        if lr & (1 << 6) != 0 && lr & (1 << 5) == 0 {
            size += 0x28;
        }
        Some((base, size))
    }

    fn exception_return_slot(&self, after: &RegState) -> Option<(u64, u64)> {
        // The additional state context (Armv8-M) sits below the basic frame,
        // whose ReturnAddress is the seventh word.
        let lr = after.get(LR, 32)?;
        let (base, _) = self.exception_frame(after)?;
        let additional = if lr & (1 << 6) != 0 && lr & (1 << 5) == 0 { 0x28 } else { 0 };
        Some((base + additional + 0x18, 4))
    }

    fn address_limit(&self) -> u64 {
        1 << 32
    }

    fn exception_number(&self, state: &RegState) -> Option<u32> {
        // IPSR, xPSR[8:0].
        state.get(XPSR, 9).map(|v| (v & 0x1ff) as u32)
    }

    fn exception_name(&self, number: u32) -> Option<String> {
        Some(
            match number {
                1 => "Reset",
                2 => "NMI",
                3 => "HardFault",
                4 => "MemManage",
                5 => "BusFault",
                6 => "UsageFault",
                7 => "SecureFault",
                11 => "SVCall",
                12 => "DebugMonitor",
                14 => "PendSV",
                15 => "SysTick",
                n if n >= 16 => return Some(format!("IRQ{}", n - 16)),
                _ => return None,
            }
            .to_string(),
        )
    }

    fn little_endian_regions(&self) -> &'static [(u64, u64)] {
        // Armv7-M/v8-M: accesses to the Private Peripheral Bus are always
        // little-endian, also on a big-endian (BE8) core.
        &[(0xE000_0000, 0xE010_0000)]
    }

    fn flow(&self, opcode: u32, width: u8) -> crate::Flow {
        crate::thumb::classify_opcode(opcode, width)
    }

    fn system_register_writes(&self, opcode: u32, width: u8) -> &'static [RegId] {
        match width {
            // CPSIE/CPSID i/f: 1011 0110 011 im 0 0 I F.
            2 if opcode & 0xffec == 0xb660 => match opcode & 3 {
                1 => &[FAULTMASK],
                2 => &[PRIMASK],
                3 => &[PRIMASK, FAULTMASK],
                _ => &[],
            },
            // MSR spec_reg, Rn: 11110 0 1110 0 0 Rn | 10 0 0 mask 0 0 SYSm.
            4 if opcode & 0xfff0_f300 == 0xf380_8000 => match opcode & 0xff {
                0x08 => &[MSP],
                0x09 => &[PSP],
                0x0a => &[MSPLIM],
                0x0b => &[PSPLIM],
                0x10 => &[PRIMASK],
                0x11 | 0x12 => &[BASEPRI],
                0x13 => &[FAULTMASK],
                0x14 => &[CONTROL],
                _ => &[],
            },
            // VMSR FPSCR, Rt: 1110 1110 1110 0001 | Rt 1010 0001 0000.
            4 if opcode & 0xffff_0fff == 0xeee1_0a10 => &[FPSCR],
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: u64 = 0xffff_ffff;

    fn lookup(name: &str, bank: Option<&str>) -> Option<RegId> {
        ArmM.lookup(&TraceRegName { name, bank })
    }

    #[test]
    fn names() {
        assert_eq!(lookup("R0", None), Some(0));
        assert_eq!(lookup("r12", None), Some(12));
        assert_eq!(lookup("r13", Some("MSP")), Some(MSP));
        assert_eq!(lookup("r13", None), Some(SP));
        assert_eq!(lookup("SP_process", None), Some(PSP));
        assert_eq!(lookup("r13_mn", None), Some(MSP));
        assert_eq!(lookup("XPSR", None), Some(XPSR));
        assert_eq!(lookup("psr", None), Some(XPSR));
        assert_eq!(lookup("MSP_S", None), Some(MSP));
        assert_eq!(lookup("r16", None), None);
        assert_eq!(lookup("sp_process", None), Some(PSP));
        assert_eq!(lookup("S0", None), Some(S0));
        assert_eq!(lookup("s31", None), Some(S0 + 31));
        assert_eq!(lookup("s32", None), None);
        assert_eq!(lookup("FPSCR", None), Some(FPSCR));
        assert_eq!(lookup("MSPLIM_S", None), Some(MSPLIM));
        // A D register is two S registers, not one register.
        assert_eq!(lookup("d0", None), None);
        let fields = |n: &str| ArmM.lookup_fields(&TraceRegName { name: n, bank: None });
        assert_eq!(fields("D2"), Some(Fields::pair(S0 + 4, S0 + 5, 32)));
        assert_eq!(fields("d15").unwrap().iter().collect::<Vec<_>>(), vec![(S0 + 30, 0), (S0 + 31, 32)]);
        assert_eq!(fields("d16"), None);
        assert_eq!(fields("r3"), Some(Fields::one(3)));
        assert_eq!(REGISTERS[FPSCR as usize].name, "fpscr");
        assert_eq!(REGISTERS[(S0 + 31) as usize].name, "s31");
    }

    fn state(writes: &[(RegId, u64)]) -> RegState {
        let mut s = RegState::new(REGISTERS.len());
        for &(r, v) in writes {
            s.apply(&RegWrite { reg: r, value: v, known: FULL, derived: false });
        }
        s
    }

    fn written(regs: &[RegId]) -> Vec<bool> {
        let mut w = vec![false; REGISTERS.len()];
        for &r in regs {
            w[r as usize] = true;
        }
        w
    }

    #[test]
    fn banked_sp_follows_control_and_mode() {
        let s = state(&[(XPSR, 0x0100_0000), (CONTROL, 0), (MSP, 0x2000_1000), (PSP, 0x2000_0800), (SP, 0x2000_1000)]);
        assert_eq!(ArmM::active_bank(&s), Some(MSP));
        // CONTROL.SPSEL set in thread mode: SP follows PSP.
        let mut t = s.clone();
        t.apply(&RegWrite { reg: CONTROL, value: 2, known: FULL, derived: false });
        assert_eq!(
            ArmM.resolve(&t, &written(&[CONTROL])),
            vec![RegWrite { reg: SP, value: 0x2000_0800, known: FULL, derived: true }]
        );
        // In handler mode SP is MSP whatever CONTROL says; an SP write lands in MSP.
        let mut h = state(&[(XPSR, 0x0100_000f), (CONTROL, 2), (MSP, 0x2000_1000), (PSP, 0x2000_0800)]);
        h.apply(&RegWrite { reg: SP, value: 0x2000_0ff0, known: FULL, derived: false });
        assert_eq!(
            ArmM.resolve(&h, &written(&[SP])),
            vec![RegWrite { reg: MSP, value: 0x2000_0ff0, known: FULL, derived: true }]
        );
    }

    #[test]
    fn exception_return_in_any_record_order() {
        // Handler (IPSR 15, MSP) returns to thread mode on PSP. The block
        // writes SP (= PSP after unstacking), xPSR and PSP; MSP must not change.
        let mut s =
            state(&[(XPSR, 0x0100_000f), (CONTROL, 2), (MSP, 0x2000_ffe0), (PSP, 0x2000_04d0), (SP, 0x2000_ffe0)]);
        for (r, v) in [(SP, 0x2000_04f0), (XPSR, 0x0100_0000), (PSP, 0x2000_04f0)] {
            s.apply(&RegWrite { reg: r, value: v, known: FULL, derived: false });
        }
        assert_eq!(ArmM.resolve(&s, &written(&[SP, XPSR, PSP])), vec![]);
        assert_eq!(s.get(MSP, 32), Some(0x2000_ffe0));
    }

    #[test]
    fn unknown_mode_makes_stale_banks_unknown() {
        // SP written, mode unknown: MSP may be stale, so it becomes unknown.
        let mut s = state(&[(MSP, 0x2000_1000)]);
        s.apply(&RegWrite { reg: SP, value: 0x2000_0f00, known: FULL, derived: false });
        assert_eq!(ArmM.resolve(&s, &written(&[SP])), vec![RegWrite { reg: MSP, value: 0, known: 0, derived: true }]);
        // A bank written, mode unknown: SP may be stale.
        let mut s = state(&[(SP, 0x2000_1000)]);
        s.apply(&RegWrite { reg: MSP, value: 0x2000_0f00, known: FULL, derived: false });
        assert_eq!(ArmM.resolve(&s, &written(&[MSP])), vec![RegWrite { reg: SP, value: 0, known: 0, derived: true }]);
    }

    #[test]
    fn switch_to_unknown_psp_makes_sp_unknown() {
        let mut s = state(&[(XPSR, 0x0100_0000), (CONTROL, 0), (MSP, 0x2000_1000), (SP, 0x2000_1000)]);
        s.apply(&RegWrite { reg: CONTROL, value: 2, known: FULL, derived: false });
        assert_eq!(
            ArmM.resolve(&s, &written(&[CONTROL])),
            vec![RegWrite { reg: SP, value: 0, known: 0, derived: true }]
        );
    }

    #[test]
    fn unknown_sp_value_reaches_the_bank() {
        let mut s = state(&[(XPSR, 0x0100_0000), (CONTROL, 0), (MSP, 0x2000_1000), (SP, 0x2000_1000)]);
        s.apply(&RegWrite { reg: SP, value: 0, known: 0, derived: false });
        assert_eq!(ArmM.resolve(&s, &written(&[SP])), vec![RegWrite { reg: MSP, value: 0, known: 0, derived: true }]);
    }

    #[test]
    fn non_secure_banks_are_not_merged() {
        assert_eq!(lookup("MSP_NS", None), None);
        assert_eq!(lookup("MSP_S", None), Some(MSP));
    }

    #[test]
    fn frame_from_exc_return() {
        let s = state(&[(LR, 0xffff_fff9), (MSP, 0x2000_0fa0)]);
        assert_eq!(ArmM.exception_frame(&s), Some((0x2000_0fa0, 0x20)));
        // PSP, FP frame. Bit 6 is 1 on Armv7-M too, so the possible Secure
        // FP state (0x40) is included: hiding valid bytes, never stale ones.
        let s = state(&[(LR, 0xffff_ffed), (PSP, 0x2000_0400)]);
        assert_eq!(ArmM.exception_frame(&s), Some((0x2000_0400, 0x68 + 0x40)));
        let s = state(&[(LR, 0xffff_ff48), (MSP, 0x3000_0f00)]); // v8-M Secure, FP, additional state
        assert_eq!(ArmM.exception_frame(&s), Some((0x3000_0f00, 0x20 + 0x48 + 0x40 + 0x28)));
        assert_eq!(ArmM.exception_frame(&state(&[(MSP, 0x2000_0fa0)])), None);
    }

    #[test]
    fn system_register_writes() {
        // msr CONTROL, r0 / msr PRIMASK, r3 / msr PSP, r1 / cpsid i / cpsie if
        assert_eq!(ArmM.system_register_writes(0xf380_8814, 4), &[CONTROL]);
        assert_eq!(ArmM.system_register_writes(0xf383_8810, 4), &[PRIMASK]);
        assert_eq!(ArmM.system_register_writes(0xf381_8809, 4), &[PSP]);
        assert_eq!(ArmM.system_register_writes(0xb672, 2), &[PRIMASK]);
        assert_eq!(ArmM.system_register_writes(0xb663, 2), &[PRIMASK, FAULTMASK]);
        // mrs r0, CONTROL reads it; ldr r0,[r1] is no MSR.
        assert!(ArmM.system_register_writes(0xf3ef_8014, 4).is_empty());
        assert!(ArmM.system_register_writes(0x6808, 2).is_empty());
        // msr MSPLIM, r0 / vmsr fpscr, r3; vmrs r3, fpscr reads it.
        assert_eq!(ArmM.system_register_writes(0xf380_880a, 4), &[MSPLIM]);
        assert_eq!(ArmM.system_register_writes(0xeee1_3a10, 4), &[FPSCR]);
        assert!(ArmM.system_register_writes(0xeef1_3a10, 4).is_empty());
    }

    #[test]
    fn data_byte_order() {
        use crate::Endian;
        let bytes = [0x11, 0x22, 0x33, 0x44];
        assert_eq!(Endian::Little.value(&bytes), 0x4433_2211);
        assert_eq!(Endian::Big.value(&bytes), 0x1122_3344);
        assert_eq!((Endian::Little.shift(0, 4), Endian::Big.shift(0, 4)), (0, 24));
        // The PPB is little-endian on a big-endian core; RAM is not.
        assert_eq!(Endian::Big.at(&ArmM, 0xE000_E014), Endian::Little);
        assert_eq!(Endian::Big.at(&ArmM, 0x2000_0000), Endian::Big);
        assert_eq!(Endian::Little.at(&ArmM, 0x2000_0000), Endian::Little);
        assert_eq!(Endian::parse("BE8"), Some(Endian::Big));
    }

    #[test]
    fn target_xml_lists_all_registers() {
        let xml = ArmM.target_xml(&[]);
        assert!(xml.contains("org.gnu.gdb.arm.m-profile"));
        assert!(xml.contains("org.gnu.gdb.arm.m-system"));
        // When PSP or MSP is unknown anywhere GDB must not be told to depend on them.
        for reg in ["psp", "msp"] {
            let xml = ArmM.target_xml(&[reg.to_string()]);
            assert!(xml.contains("org.simgdb.arm.m-system") && !xml.contains("org.gnu.gdb.arm.m-system"));
        }
        assert!(ArmM.target_xml(&["r0".to_string()]).contains("org.gnu.gdb.arm.m-system"));
        assert!(xml.contains("name=\"xpsr\" bitsize=\"32\" regnum=\"16\""));
        assert!(xml.contains("name=\"control\" bitsize=\"32\" regnum=\"22\""));
        // GDB's VFP feature: D0-D15 over S0-S31, then FPSCR; no S registers.
        assert!(xml.contains("<feature name=\"org.gnu.gdb.arm.vfp\">"));
        assert!(xml.contains("name=\"d2\" bitsize=\"64\" regnum=\"27\" type=\"ieee_double\" group=\"float\""));
        assert!(xml.contains("name=\"fpscr\" bitsize=\"32\" regnum=\"41\" type=\"int\" group=\"float\""));
        assert!(!xml.contains("name=\"s4\""));
        let g = ArmM.gdb_registers();
        assert_eq!((g.len(), g[27]), (42, GdbRegister { name: "d2", first: S0 + 4, count: 2 }));
        assert_eq!(xml.matches("<reg ").count(), g.len());
    }
}
