//! Call table derived from the trace flow (ARCHITECTURE §3 `calls`).
//!
//! A shadow call stack is kept while indexing; it needs no stack memory, so it
//! works even where the stack contents are unknown:
//!
//! - a call instruction (`bl`, `blx`) that executes opens a call frame whose
//!   return address is the next instruction;
//! - reaching a frame's return address closes it (and any frames above it,
//!   e.g. after longjmp), without crossing an exception frame;
//! - an exception record opens an exception frame;
//! - a return-type instruction executed at exception level (the handler's
//!   own `bx lr` or `pop {pc}`, also through tail calls) closes the exception
//!   frame at the next instruction, or at the next exception record when the
//!   exception tail-chains;
//! - if the instruction after such a return still runs in handler mode, at
//!   an exception level other than the one returned to, the trace omitted a
//!   tail-chained entry: an exception frame is opened for it (counted as
//!   inferred).

use std::collections::HashMap;

use simgdb_isa::Flow;

use crate::NONE;

pub const FRAME_CALL: u8 = 0;
pub const FRAME_EXCEPTION: u8 = 1;
/// No frame: the code the trace started in.
pub const TOP_LEVEL: u32 = u32::MAX;

/// Search depth for a matching return address.
const RETURN_SEARCH: usize = 1024;

#[derive(Default)]
pub struct Calls {
    pub kind: Vec<u8>,
    /// Point of the call instruction, or the point the exception entered at.
    pub call_index: Vec<u64>,
    /// First point inside the frame.
    pub entry_index: Vec<u64>,
    /// First point after the frame returned (`NONE` if it never did).
    pub return_index: Vec<u64>,
    /// PC of the call instruction (`NONE` for exceptions).
    pub call_pc: Vec<u64>,
    pub return_pc: Vec<u64>,
    /// Exception number (`u32::MAX` for calls or when not recorded).
    pub number: Vec<u32>,
    pub parent: Vec<u32>,
    pub depth: Vec<u32>,
    stack: Vec<u32>,
    /// Return addresses of the open call frames, with counts: most
    /// instructions are no return target, and need no stack search.
    open_returns: HashMap<u64, u32>,
    exception_return: bool,
    /// Exception frames opened without an exception record (tail-chaining
    /// seen from the exception level alone).
    pub inferred: Vec<u32>,
}

impl Calls {
    fn top(&self) -> u32 {
        self.stack.last().copied().unwrap_or(TOP_LEVEL)
    }

    fn top_is_exception(&self) -> bool {
        self.stack.last().is_some_and(|&f| self.kind[f as usize] == FRAME_EXCEPTION)
    }

    fn open(&mut self, kind: u8, call_index: u64, entry_index: u64, call_pc: u64, return_pc: u64, number: u32) {
        let id = self.kind.len() as u32;
        self.kind.push(kind);
        self.call_index.push(call_index);
        self.entry_index.push(entry_index);
        self.return_index.push(NONE);
        self.call_pc.push(call_pc);
        self.return_pc.push(return_pc);
        self.number.push(number);
        self.parent.push(self.top());
        self.depth.push(self.stack.len() as u32 + 1);
        self.stack.push(id);
        if kind == FRAME_CALL {
            *self.open_returns.entry(return_pc).or_default() += 1;
        }
    }

    fn close_top(&mut self, at: u64) {
        if let Some(f) = self.stack.pop() {
            self.return_index[f as usize] = at;
            if self.kind[f as usize] == FRAME_CALL {
                let pc = self.return_pc[f as usize];
                if let Some(n) = self.open_returns.get_mut(&pc) {
                    *n -= 1;
                    if *n == 0 {
                        self.open_returns.remove(&pc);
                    }
                }
            }
        }
    }

    /// Instruction record `index` at `pc` is about to be indexed; `level` is
    /// the exception number at point `index` (0 in thread mode) if known.
    /// Returns the innermost frame active at point `index`.
    pub fn at_instruction(&mut self, index: u64, pc: u64, level: Option<u32>) -> u32 {
        if self.exception_return && self.top_is_exception() {
            self.close_top(index);
            let outer = self
                .stack
                .iter()
                .rev()
                .find(|&&f| self.kind[f as usize] == FRAME_EXCEPTION)
                .map(|&f| self.number[f as usize])
                .unwrap_or(0);
            if let Some(n) = level {
                if n != 0 && outer != u32::MAX && n != outer {
                    self.open(FRAME_EXCEPTION, index, index, NONE, NONE, n);
                    self.inferred.push(self.kind.len() as u32 - 1);
                    self.exception_return = false;
                    return self.top();
                }
            }
        }
        self.exception_return = false;
        // Return to a call frame's return address, searching down to the
        // nearest exception frame.
        let mut found = None;
        if !self.open_returns.contains_key(&pc) {
            return self.top();
        }
        for (k, &f) in self.stack.iter().enumerate().rev().take(RETURN_SEARCH) {
            if self.kind[f as usize] == FRAME_EXCEPTION {
                break;
            }
            if self.return_pc[f as usize] == pc {
                found = Some(k);
                break;
            }
        }
        if let Some(k) = found {
            while self.stack.len() > k {
                self.close_top(index);
            }
        }
        self.top()
    }

    /// Instruction `index` executed (not condition-failed) with this flow.
    pub fn after_instruction(&mut self, index: u64, pc: u64, width: u8, flow: Flow) {
        match flow {
            Flow::Call => self.open(FRAME_CALL, index, index + 1, pc, pc + width as u64, u32::MAX),
            Flow::Return if self.top_is_exception() => self.exception_return = true,
            _ => {}
        }
    }

    /// Reset: everything before it is gone.
    pub fn reset(&mut self, at: u64) {
        while !self.stack.is_empty() {
            self.close_top(at);
        }
        self.exception_return = false;
    }

    /// Exception record visible from point `at`.
    pub fn exception(&mut self, at: u64, number: Option<u32>) {
        if self.exception_return && self.top_is_exception() {
            // Tail-chaining: the previous handler returned straight into this one.
            self.close_top(at);
        }
        self.exception_return = false;
        self.open(FRAME_EXCEPTION, at, at, NONE, NONE, number.unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_and_exceptions() {
        let mut c = Calls::default();
        assert_eq!(c.at_instruction(0, 0x100, Some(0)), TOP_LEVEL);
        c.after_instruction(0, 0x100, 4, Flow::Call); // bl 0x200
        assert_eq!(c.at_instruction(1, 0x200, Some(0)), 0);
        c.after_instruction(1, 0x200, 2, Flow::Next);
        c.exception(2, Some(15));
        assert_eq!(c.at_instruction(2, 0x300, Some(15)), 1); // handler
        c.after_instruction(2, 0x300, 2, Flow::Return); // bx lr (EXC_RETURN)
        assert_eq!(c.at_instruction(3, 0x202, Some(0)), 0); // back in the callee
        c.after_instruction(3, 0x202, 2, Flow::Return); // bx lr
        assert_eq!(c.at_instruction(4, 0x104, Some(0)), TOP_LEVEL); // back at the caller
        assert_eq!((c.return_index[0], c.return_index[1]), (4, 3));
        assert_eq!(c.depth[1], 2);
    }

    #[test]
    fn tail_chain_replaces_the_exception_frame() {
        let mut c = Calls::default();
        c.at_instruction(0, 0x100, None);
        c.exception(1, Some(15));
        c.at_instruction(1, 0x300, None);
        c.after_instruction(1, 0x300, 2, Flow::Return);
        c.exception(2, Some(11));
        assert_eq!(c.at_instruction(2, 0x400, None), 1);
        assert_eq!(c.parent[1], TOP_LEVEL);
        assert_eq!(c.return_index[0], 2);
        assert!(c.inferred.is_empty());
    }

    #[test]
    fn tail_chain_without_a_record_is_inferred_from_the_level() {
        let mut c = Calls::default();
        c.at_instruction(0, 0x100, Some(0));
        c.exception(1, Some(11)); // SVC
        c.at_instruction(1, 0x300, Some(11));
        c.exception(2, Some(15)); // SysTick nested into it
        c.at_instruction(2, 0x500, Some(15));
        c.after_instruction(2, 0x500, 2, Flow::Return);
        // Back in the SVC handler: a nested return, not a tail chain.
        assert_eq!(c.at_instruction(3, 0x302, Some(11)), 0);
        c.after_instruction(3, 0x302, 2, Flow::Return);
        // Still handler mode, level 14 (PendSV): tail-chained, no record.
        assert_eq!(c.at_instruction(4, 0x600, Some(14)), 2);
        assert_eq!((c.number[2], c.parent[2], c.return_index[0]), (14, TOP_LEVEL, 4));
        assert_eq!(c.inferred, [2]);
        c.after_instruction(4, 0x600, 2, Flow::Return);
        assert_eq!(c.at_instruction(5, 0x102, Some(0)), TOP_LEVEL);
        // Unknown level: closed, nothing inferred.
        c.exception(6, None);
        c.at_instruction(6, 0x300, None);
        c.after_instruction(6, 0x300, 2, Flow::Return);
        assert_eq!(c.at_instruction(7, 0x104, None), TOP_LEVEL);
    }
}
