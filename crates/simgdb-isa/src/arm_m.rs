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
        "msp" | "sp_main" | "r13_main" | "msp_s" | "msp_ns" | "r13_main_s" | "r13_main_ns" => MSP,
        "psp" | "sp_process" | "r13_process" | "psp_s" | "psp_ns" | "r13_process_s" | "r13_process_ns" => PSP,
        "primask" | "primask_s" | "primask_ns" => PRIMASK,
        "basepri" | "basepri_s" | "basepri_ns" => BASEPRI,
        "faultmask" | "faultmask_s" | "faultmask_ns" => FAULTMASK,
        "control" | "control_s" | "control_ns" => CONTROL,
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
        let Some(bank) = ArmM::active_bank(state) else { return vec![] };
        let (sp, b) = (SP as usize, bank as usize);
        if written[sp] && !written[b] {
            // SP was written: it is the active bank.
            if state.known[sp] != 0 && (state.values[sp] != state.values[b] || state.known[sp] != state.known[b]) {
                return vec![RegWrite { reg: bank, value: state.values[sp], known: state.known[sp], derived: true }];
            }
        } else if !written[sp] && state.known[b] != 0 {
            // The bank, the mode or SPSEL changed: SP follows the active bank.
            if state.values[sp] != state.values[b] || state.known[sp] != state.known[b] {
                return vec![RegWrite { reg: SP, value: state.values[b], known: state.known[b], derived: true }];
            }
        }
        vec![]
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

    fn exception_frame_bytes(&self) -> u64 {
        // Extended frame with FP state (0x68) plus a 4-byte alignment pad.
        0x6c
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
        assert_eq!(s.get(MSP), Some(0x2000_ffe0));
    }

    #[test]
    fn unknown_mode_does_not_guess_bank() {
        let mut s = state(&[(MSP, 0x2000_1000)]);
        s.apply(&RegWrite { reg: SP, value: 0x2000_0f00, known: FULL, derived: false });
        assert_eq!(ArmM.resolve(&s, &written(&[SP])), vec![]);
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
