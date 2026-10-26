//! Arm M-profile (Armv7-M, Armv8-M Mainline): Cortex-M4, Cortex-M35P.

use crate::{Isa, RegDef, RegId, RegState, RegWrite, TraceRegName};

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

const CORE: &str = "org.gnu.gdb.arm.m-profile";
/// Feature of MSP, PSP and the special registers. With this standard name
/// GDB (13 and later) unwinds exception frames on either stack, but reads
/// both MSP and PSP for *every* exception frame.
const SYSTEM: &str = "org.gnu.gdb.arm.m-system";
/// Used instead when the trace never writes PSP (PSP is UNKNOWN after reset;
/// bare-metal firmware often never sets it). GDB then uses its generic
/// unwinder, which needs no PSP for frames on the main stack, and the
/// registers still show in `info registers`. Frames on the process stack
/// cannot occur without a PSP write.
const SYSTEM_NO_PSP: &str = "org.simgdb.arm.m-system";

macro_rules! reg {
    ($name:expr, $feature:expr, $ty:expr) => {
        RegDef { name: $name, bits: 32, feature: $feature, gdb_type: $ty }
    };
}

static REGISTERS: [RegDef; 23] = [
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
        "msp" | "sp_main" | "r13_main" | "msp_s" | "r13_main_s" => MSP,
        "psp" | "sp_process" | "r13_process" | "psp_s" | "r13_process_s" => PSP,
        "primask" | "primask_s" => PRIMASK,
        "basepri" | "basepri_s" => BASEPRI,
        "faultmask" | "faultmask_s" => FAULTMASK,
        "control" | "control_s" => CONTROL,
        _ => {
            let n = name.strip_prefix('r')?.parse::<u16>().ok()?;
            if n <= 12 { n } else { return None }
        }
    };
    Some(r)
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
        if lower == "r13" || lower == "sp" {
            if let Some(bank) = name.bank {
                match bank.to_ascii_lowercase().as_str() {
                    "msp" | "main" => return Some(MSP),
                    "psp" | "process" => return Some(PSP),
                    _ => {}
                }
            }
        }
        alias(&lower)
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

    fn target_xml(&self, never_written: &[String]) -> String {
        let system = if never_written.iter().any(|r| r == "psp") { SYSTEM_NO_PSP } else { SYSTEM };
        let mut xml = String::from(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE target SYSTEM \"gdb-target.dtd\">\n<target version=\"1.0\">\n<architecture>arm</architecture>\n",
        );
        let mut feature = "";
        for (n, r) in REGISTERS.iter().enumerate() {
            if r.feature != feature {
                if !feature.is_empty() {
                    xml.push_str("</feature>\n");
                }
                feature = r.feature;
                let name = if feature == SYSTEM { system } else { feature };
                xml.push_str(&format!("<feature name=\"{name}\">\n"));
            }
            xml.push_str(&format!(
                "<reg name=\"{}\" bitsize=\"{}\" regnum=\"{}\" type=\"{}\" group=\"general\"/>\n",
                r.name, r.bits, n, r.gdb_type
            ));
        }
        xml.push_str("</feature>\n</target>\n");
        xml
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

    fn little_endian(&self) -> bool {
        // BE8 M-profile parts exist but are rare; not supported yet.
        true
    }

    fn flow(&self, opcode: u32, width: u8) -> crate::Flow {
        crate::thumb::classify_opcode(opcode, width)
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
        assert_eq!(lookup("XPSR", None), Some(XPSR));
        assert_eq!(lookup("psr", None), Some(XPSR));
        assert_eq!(lookup("MSP_S", None), Some(MSP));
        assert_eq!(lookup("d0", None), None);
        assert_eq!(lookup("r16", None), None);
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
    fn target_xml_lists_all_registers() {
        let xml = ArmM.target_xml(&[]);
        assert!(xml.contains("org.gnu.gdb.arm.m-profile"));
        assert!(xml.contains("org.gnu.gdb.arm.m-system"));
        // Without any PSP write GDB must not be told to depend on PSP.
        let xml = ArmM.target_xml(&["psp".to_string()]);
        assert!(xml.contains("org.simgdb.arm.m-system") && !xml.contains("org.gnu.gdb.arm.m-system"));
        assert!(xml.contains("name=\"xpsr\" bitsize=\"32\" regnum=\"16\""));
        assert!(xml.contains("name=\"control\" bitsize=\"32\" regnum=\"22\""));
    }
}
