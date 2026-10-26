//! Thumb / Thumb-2 instruction classification (Armv7-M, Armv8-M Mainline).
//!
//! Only what SimGDB needs: encoding length, and how an instruction affects
//! control flow. "Ends flow" follows the source-attribution rules
//! (ARCHITECTURE §6) and tarmac_to_cachegrind: unconditional `b`, `bx`,
//! `bxns`, `tbb`/`tbh`, `pop`/`ldm` with pc in the list, `ldr pc, ...` and
//! `mov pc, ...`. Inside an IT block these are conditional and fall through.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// Continues with the next instruction.
    Next,
    /// Unconditional jump that is not a return: `b`, `bx Rm` (Rm != lr),
    /// `tbb`/`tbh`, `mov pc, Rm`.
    Jump,
    /// Return: `bx lr`, `pop {.., pc}`, `ldm {.., pc}`, `ldr pc, ...`.
    Return,
    /// Call: `bl`, `blx`, `blxns` (execution continues after it on return).
    Call,
    /// Conditional branch (`b<cc>`, `cbz`, `cbnz`) or a flow-ending
    /// instruction made conditional by an IT block.
    Conditional,
}

impl Flow {
    /// Execution can never continue to the next instruction.
    pub fn ends_flow(self) -> bool {
        matches!(self, Flow::Jump | Flow::Return)
    }
}

/// First halfword starts a 32-bit encoding.
pub fn is_32bit(hw1: u16) -> bool {
    (hw1 >> 11) >= 0b11101
}

/// Number of instructions an `IT` controls, or 0 if `hw` is not an IT.
pub fn it_count(hw: u16) -> u8 {
    if hw & 0xff00 == 0xbf00 && hw & 0xf != 0 { 4 - (hw & 0xf).trailing_zeros() as u8 } else { 0 }
}

/// Classify one instruction outside any IT block. `hw2` is ignored for
/// 16-bit encodings.
pub fn classify(hw1: u16, hw2: u16) -> Flow {
    if !is_32bit(hw1) {
        return classify16(hw1);
    }
    // Branches and miscellaneous control: 11110xxx xxxxxxxx 1xxxxxxx xxxxxxxx
    if hw1 >> 11 == 0b11110 && hw2 & 0x8000 != 0 {
        return match hw2 & 0xd000 {
            0xd000 => Flow::Call,                                  // BL
            0xc000 if hw2 & 1 == 0 => Flow::Call,                  // BLX (immediate)
            0x9000 => Flow::Jump,                                  // B.W (T4)
            0x8000 if (hw1 >> 6) & 0xf < 0xe => Flow::Conditional, // B<cc>.W (T3)
            _ => Flow::Next,
        };
    }
    // TBB / TBH
    if hw1 & 0xfff0 == 0xe8d0 && hw2 & 0xffe0 == 0xf000 {
        return Flow::Jump;
    }
    // LDM (IA, incl. POP.W) and LDMDB with pc in the register list
    if (hw1 & 0xffd0 == 0xe890 || hw1 & 0xffd0 == 0xe910) && hw2 & 0x8000 != 0 {
        return Flow::Return;
    }
    // LDR (word, any addressing mode) with Rt = pc
    if (hw1 & 0xfff0 == 0xf850 || hw1 & 0xfff0 == 0xf8d0) && hw2 >> 12 == 0xf {
        return Flow::Return;
    }
    Flow::Next
}

fn classify16(hw: u16) -> Flow {
    match hw {
        // B (T2, unconditional)
        _ if hw & 0xf800 == 0xe000 => Flow::Jump,
        // B<cc> (T1); cond 1110 is UDF and 1111 is SVC
        _ if hw & 0xf000 == 0xd000 && (hw >> 8) & 0xf < 0xe => Flow::Conditional,
        // CBZ / CBNZ
        _ if hw & 0xf500 == 0xb100 => Flow::Conditional,
        // BX / BXNS Rm (bit 7 clear), BLX / BLXNS Rm (bit 7 set)
        _ if hw & 0xff80 == 0x4700 => {
            if (hw >> 3) & 0xf == 14 {
                Flow::Return
            } else {
                Flow::Jump
            }
        }
        _ if hw & 0xff80 == 0x4780 => Flow::Call,
        // POP {.., pc}
        _ if hw & 0xff00 == 0xbd00 => Flow::Return,
        // MOV pc, Rm (high register form, Rd = D:Rd = 15)
        _ if hw & 0xff87 == 0x4687 => Flow::Jump,
        _ => Flow::Next,
    }
}

/// One decoded instruction of a code region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub addr: u64,
    pub len: u8,
    pub flow: Flow,
}

/// Decode a code region from its start (linear sweep), tracking IT blocks:
/// a flow-ending instruction inside an IT block is `Conditional`.
pub fn sweep(code: &[u8], base: u64) -> Vec<Insn> {
    let mut out = Vec::new();
    let mut off = 0usize;
    let mut it_left = 0u8;
    while off + 2 <= code.len() {
        let hw1 = u16::from_le_bytes([code[off], code[off + 1]]);
        let (len, hw2) = if is_32bit(hw1) {
            if off + 4 > code.len() {
                break;
            }
            (4, u16::from_le_bytes([code[off + 2], code[off + 3]]))
        } else {
            (2, 0)
        };
        let mut flow = classify(hw1, hw2);
        if it_left > 0 {
            if flow != Flow::Next && flow != Flow::Call {
                flow = Flow::Conditional;
            }
            it_left -= 1;
        } else if len == 2 {
            it_left = it_count(hw1);
        }
        out.push(Insn { addr: base + off as u64, len, flow });
        off += len as usize;
    }
    out
}

/// Classify an instruction as printed in a trace (`opcode` with the first
/// halfword in the high bits for 32-bit encodings).
pub fn classify_opcode(opcode: u32, width: u8) -> Flow {
    if width == 4 || opcode > 0xffff {
        classify((opcode >> 16) as u16, opcode as u16)
    } else {
        classify16(opcode as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(classify16(0xe7f1), Flow::Jump); // b.n
        assert_eq!(classify16(0xd001), Flow::Conditional); // beq.n
        assert_eq!(classify16(0xdf01), Flow::Next); // svc 1
        assert_eq!(classify16(0xb15b), Flow::Conditional); // cbz
        assert_eq!(classify16(0x4770), Flow::Return); // bx lr
        assert_eq!(classify16(0x4718), Flow::Jump); // bx r3
        assert_eq!(classify16(0x4798), Flow::Call); // blx r3
        assert_eq!(classify16(0xbd70), Flow::Return); // pop {r4, r5, r6, pc}
        assert_eq!(classify16(0xb570), Flow::Next); // push
        assert_eq!(classify16(0x46f7), Flow::Jump); // mov pc, lr
        assert_eq!(classify(0xf7ff, 0xffbc), Flow::Call); // bl
        assert_eq!(classify(0xf000, 0xb800), Flow::Jump); // b.w
        assert_eq!(classify(0xf040, 0x8001), Flow::Conditional); // bne.w
        assert_eq!(classify(0xe8d0, 0xf001), Flow::Jump); // tbb [r0, r1]
        assert_eq!(classify(0xe8bd, 0x8ff0), Flow::Return); // pop.w {r4-r11, pc}
        assert_eq!(classify(0xe8bd, 0x0ff0), Flow::Next); // pop.w {r4-r11}
        assert_eq!(classify(0xf85d, 0xfb04), Flow::Return); // ldr.w pc, [sp], #4
        assert_eq!(classify(0xf8df, 0xf004), Flow::Return); // ldr.w pc, [pc, #4]
        assert_eq!(classify(0xf8df, 0x907c), Flow::Next); // ldr.w r9, [pc, #124]
        assert_eq!(classify(0xf853, 0x0020), Flow::Next); // ldr.w r0, [r3, r0, lsl #2]
    }

    #[test]
    fn it_block_makes_returns_conditional() {
        // it eq; bxeq lr; bx lr
        let code = [0x08, 0xbf, 0x70, 0x47, 0x70, 0x47];
        let insns = sweep(&code, 0x100);
        assert_eq!(insns.iter().map(|i| i.flow).collect::<Vec<_>>(), [Flow::Next, Flow::Conditional, Flow::Return]);
        assert_eq!(it_count(0xbf08), 1);
        assert_eq!(it_count(0xbf1c), 2); // itt ne
        assert_eq!(it_count(0xbf00), 0); // nop
    }
}
