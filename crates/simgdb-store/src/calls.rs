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
//!   inferred);
//! - an exception taken right after a call frame's return-type instruction
//!   executed, whose handler returns to that frame's return address, came
//!   after the call returned: the call frame closes at the exception entry
//!   and the exception frame belongs to the caller. (The return target is
//!   only seen when the exception returns, so this is settled then.)

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use simgdb_isa::Flow;

use crate::table::{Column, TableWriter};
use crate::{NONE, StoreError};

pub const FRAME_CALL: u8 = 0;
pub const FRAME_EXCEPTION: u8 = 1;
/// No frame: the code the trace started in.
pub const TOP_LEVEL: u32 = u32::MAX;

/// Search depth for a matching return address.
const RETURN_SEARCH: usize = 1024;

/// One row of the call table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    /// Point of the call instruction, or the point the exception entered at.
    pub call_index: u64,
    /// First point inside the frame.
    pub entry_index: u64,
    /// First point after the frame returned (`NONE` if it never did).
    pub return_index: u64,
    /// PC of the call instruction (`NONE` for exceptions).
    pub call_pc: u64,
    pub return_pc: u64,
    /// Exception number (`u32::MAX` for calls or when not recorded).
    pub number: u32,
    pub parent: u32,
    pub depth: u32,
}

const REC: usize = 1 + 5 * 8 + 3 * 4;
const RETURN_OFFSET: usize = 1 + 2 * 8;

impl Frame {
    fn bytes(&self) -> [u8; REC] {
        let mut b = [0u8; REC];
        b[0] = self.kind;
        let mut o = 1;
        for v in [self.call_index, self.entry_index, self.return_index, self.call_pc, self.return_pc] {
            b[o..o + 8].copy_from_slice(&v.to_le_bytes());
            o += 8;
        }
        for v in [self.number, self.parent, self.depth] {
            b[o..o + 4].copy_from_slice(&v.to_le_bytes());
            o += 4;
        }
        b
    }

    fn from_bytes(b: &[u8]) -> Frame {
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Frame {
            kind: b[0],
            call_index: u64_at(1),
            entry_index: u64_at(9),
            return_index: u64_at(17),
            call_pc: u64_at(25),
            return_pc: u64_at(33),
            number: u32_at(41),
            parent: u32_at(45),
            depth: u32_at(49),
        }
    }
}

/// Where frame rows live while indexing: all in memory, or (for long
/// traces) in a file with only recent rows in memory. A frame's return
/// point is filled in when it closes, which for a long-lived frame may be
/// long after its row went to the file.
enum Store {
    Memory(Vec<Frame>),
    File { file: File, path: PathBuf, base: u32, tail: Vec<Frame> },
}

/// Rows kept in memory before the older half goes to the file.
const TAIL: usize = 1 << 16;

/// A frame still open while indexing.
#[derive(Clone, Copy)]
struct Open {
    id: u32,
    kind: u8,
    return_pc: u64,
    number: u32,
}

pub struct Calls {
    store: Store,
    count: u32,
    stack: Vec<Open>,
    /// Return addresses of the open call frames, with counts: most
    /// instructions are no return target, and need no stack search.
    open_returns: HashMap<u64, u32>,
    exception_return: bool,
    /// The last executed instruction was a return-type instruction in a call
    /// frame (its target is seen at the next instruction).
    call_return: bool,
    /// An exception entered right after `call_return`: settled when the
    /// exceptions return.
    pending: Option<ReturnedCall>,
    /// Exception frames opened without an exception record (tail-chaining
    /// seen from the exception level alone).
    pub inferred: Vec<u32>,
}

/// A call frame that may have returned just before an exception entry.
#[derive(Clone, Copy)]
struct ReturnedCall {
    call: u32,
    return_pc: u64,
    /// First frame opened for the exception, and the entry point.
    first: u32,
    entry: u64,
}

impl Default for Calls {
    fn default() -> Self {
        Calls {
            store: Store::Memory(Vec::new()),
            count: 0,
            stack: Vec::new(),
            open_returns: HashMap::new(),
            exception_return: false,
            call_return: false,
            pending: None,
            inferred: Vec::new(),
        }
    }
}

impl Calls {
    /// Rows go to `path` (a scratch file) except for recent ones.
    pub fn with_file(path: &Path) -> Result<Calls, StoreError> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| StoreError::io(path, e))?;
        Ok(Calls {
            store: Store::File { file, path: path.to_path_buf(), base: 0, tail: Vec::new() },
            ..Default::default()
        })
    }

    /// Frames opened so far.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// A frame's row (memory store only; for tests).
    pub fn frame(&self, id: u32) -> Frame {
        match &self.store {
            Store::Memory(v) => v[id as usize],
            Store::File { base, tail, .. } => tail[(id - base) as usize],
        }
    }

    fn top(&self) -> u32 {
        self.stack.last().map(|o| o.id).unwrap_or(TOP_LEVEL)
    }

    fn top_is_exception(&self) -> bool {
        self.stack.last().is_some_and(|o| o.kind == FRAME_EXCEPTION)
    }

    fn push(&mut self, f: Frame) -> Result<(), StoreError> {
        match &mut self.store {
            Store::Memory(v) => v.push(f),
            Store::File { file, path, base, tail } => {
                tail.push(f);
                if tail.len() >= 2 * TAIL {
                    let mut buf = Vec::with_capacity(TAIL * REC);
                    for r in tail.drain(..TAIL) {
                        buf.extend_from_slice(&r.bytes());
                    }
                    file.seek(SeekFrom::End(0)).map_err(|e| StoreError::io(path, e))?;
                    file.write_all(&buf).map_err(|e| StoreError::io(path, e))?;
                    *base += TAIL as u32;
                }
            }
        }
        Ok(())
    }

    /// Change a stored row.
    fn patch(&mut self, id: u32, change: impl FnOnce(&mut Frame)) -> Result<(), StoreError> {
        match &mut self.store {
            Store::Memory(v) => change(&mut v[id as usize]),
            Store::File { file, path, base, tail } => {
                if id >= *base {
                    change(&mut tail[(id - *base) as usize]);
                } else {
                    let off = id as u64 * REC as u64;
                    let mut b = [0u8; REC];
                    file.seek(SeekFrom::Start(off)).map_err(|e| StoreError::io(path, e))?;
                    file.read_exact(&mut b).map_err(|e| StoreError::io(path, e))?;
                    let mut f = Frame::from_bytes(&b);
                    change(&mut f);
                    file.seek(SeekFrom::Start(off)).map_err(|e| StoreError::io(path, e))?;
                    file.write_all(&f.bytes()).map_err(|e| StoreError::io(path, e))?;
                }
            }
        }
        Ok(())
    }

    /// The pending call frame returned before the exception entry: it closes
    /// there, and the frames opened since (the exception's) move up a level.
    fn settle_returned_call(&mut self, p: ReturnedCall) -> Result<(), StoreError> {
        let caller = match &self.store {
            Store::Memory(v) => v[p.call as usize].parent,
            Store::File { .. } => {
                let mut parent = TOP_LEVEL;
                self.patch(p.call, |f| parent = f.parent)?;
                parent
            }
        };
        for id in p.first..self.count {
            self.patch(id, |f| {
                f.depth -= 1;
                if f.parent == p.call {
                    f.parent = caller;
                }
            })?;
        }
        self.close_top(p.entry)
    }

    fn set_return(&mut self, id: u32, at: u64) -> Result<(), StoreError> {
        match &mut self.store {
            Store::Memory(v) => v[id as usize].return_index = at,
            Store::File { file, path, base, tail } => {
                if id >= *base {
                    tail[(id - *base) as usize].return_index = at;
                } else {
                    let off = id as u64 * REC as u64 + RETURN_OFFSET as u64;
                    file.seek(SeekFrom::Start(off)).map_err(|e| StoreError::io(path, e))?;
                    file.write_all(&at.to_le_bytes()).map_err(|e| StoreError::io(path, e))?;
                }
            }
        }
        Ok(())
    }

    fn open(
        &mut self,
        kind: u8,
        call_index: u64,
        entry_index: u64,
        call_pc: u64,
        return_pc: u64,
        number: u32,
    ) -> Result<(), StoreError> {
        let id = self.count;
        let f = Frame {
            kind,
            call_index,
            entry_index,
            return_index: NONE,
            call_pc,
            return_pc,
            number,
            parent: self.top(),
            depth: self.stack.len() as u32 + 1,
        };
        self.push(f)?;
        self.count += 1;
        self.stack.push(Open { id, kind, return_pc, number });
        if kind == FRAME_CALL {
            *self.open_returns.entry(return_pc).or_default() += 1;
        }
        Ok(())
    }

    fn close_top(&mut self, at: u64) -> Result<(), StoreError> {
        if let Some(o) = self.stack.pop() {
            self.set_return(o.id, at)?;
            if o.kind == FRAME_CALL
                && let Some(n) = self.open_returns.get_mut(&o.return_pc)
            {
                *n -= 1;
                if *n == 0 {
                    self.open_returns.remove(&o.return_pc);
                }
            }
        }
        Ok(())
    }

    /// Instruction record `index` at `pc` is about to be indexed; `level` is
    /// the exception number at point `index` (0 in thread mode) if known.
    /// Returns the innermost frame active at point `index`.
    pub fn at_instruction(&mut self, index: u64, pc: u64, level: Option<u32>) -> Result<u32, StoreError> {
        if self.exception_return && self.top_is_exception() {
            self.close_top(index)?;
            let outer = self.stack.iter().rev().find(|o| o.kind == FRAME_EXCEPTION).map(|o| o.number).unwrap_or(0);
            if let Some(n) = level
                && n != 0
                && outer != u32::MAX
                && n != outer
            {
                self.open(FRAME_EXCEPTION, index, index, NONE, NONE, n)?;
                self.inferred.push(self.count - 1);
                self.exception_return = false;
                return Ok(self.top());
            }
        }
        self.exception_return = false;
        self.call_return = false;
        // Settled once the frames above the call frame (the exception's,
        // and calls the handler makes) are gone.
        if let Some(p) = self.pending {
            if self.top() == p.call {
                self.pending = None;
                if pc == p.return_pc {
                    self.settle_returned_call(p)?;
                    return Ok(self.top());
                }
            } else if !self.stack.iter().any(|o| o.id == p.call) {
                self.pending = None;
            }
        }
        // Return to a call frame's return address, searching down to the
        // nearest exception frame.
        if !self.open_returns.contains_key(&pc) {
            return Ok(self.top());
        }
        let mut found = None;
        for (k, o) in self.stack.iter().enumerate().rev().take(RETURN_SEARCH) {
            if o.kind == FRAME_EXCEPTION {
                break;
            }
            if o.return_pc == pc {
                found = Some(k);
                break;
            }
        }
        if let Some(k) = found {
            while self.stack.len() > k {
                self.close_top(index)?;
            }
        }
        Ok(self.top())
    }

    /// Instruction `index` executed (not condition-failed) with this flow.
    pub fn after_instruction(&mut self, index: u64, pc: u64, width: u8, flow: Flow) -> Result<(), StoreError> {
        match flow {
            Flow::Call => self.open(FRAME_CALL, index, index + 1, pc, pc + width as u64, u32::MAX)?,
            Flow::Return if self.top_is_exception() => self.exception_return = true,
            Flow::Return => self.call_return = !self.stack.is_empty(),
            _ => {}
        }
        Ok(())
    }

    /// Reset: everything before it is gone.
    pub fn reset(&mut self, at: u64) -> Result<(), StoreError> {
        while !self.stack.is_empty() {
            self.close_top(at)?;
        }
        self.exception_return = false;
        self.call_return = false;
        self.pending = None;
        Ok(())
    }

    /// Exception record visible from point `at`.
    pub fn exception(&mut self, at: u64, number: Option<u32>) -> Result<(), StoreError> {
        if self.exception_return && self.top_is_exception() {
            // Tail-chaining: the previous handler returned straight into this one.
            self.close_top(at)?;
        }
        self.exception_return = false;
        if self.call_return
            && self.pending.is_none()
            && let Some(top) = self.stack.last()
            && top.kind == FRAME_CALL
        {
            self.pending = Some(ReturnedCall { call: top.id, return_pc: top.return_pc, first: self.count, entry: at });
        }
        self.open(FRAME_EXCEPTION, at, at, NONE, NONE, number.unwrap_or(u32::MAX))
    }

    /// Write the call table (`calls.arrow`).
    pub fn write(self, path: &Path, batch: usize) -> Result<(), StoreError> {
        let mut w = TableWriter::create(
            path,
            vec![
                ("kind", Column::U8(vec![])),
                ("call_index", Column::U64(vec![])),
                ("entry_index", Column::U64(vec![])),
                ("return_index", Column::U64(vec![])),
                ("call_pc", Column::U64(vec![])),
                ("return_pc", Column::U64(vec![])),
                ("number", Column::U32(vec![])),
                ("parent", Column::U32(vec![])),
                ("depth", Column::U32(vec![])),
            ],
            batch,
        )?;
        let mut emit = |f: &Frame| -> Result<(), StoreError> {
            let c = &mut w.cols;
            c[0].u8s().push(f.kind);
            c[1].u64s().push(f.call_index);
            c[2].u64s().push(f.entry_index);
            c[3].u64s().push(f.return_index);
            c[4].u64s().push(f.call_pc);
            c[5].u64s().push(f.return_pc);
            c[6].u32s().push(f.number);
            c[7].u32s().push(f.parent);
            c[8].u32s().push(f.depth);
            w.flush(0)
        };
        match self.store {
            Store::Memory(v) => v.iter().try_for_each(&mut emit)?,
            Store::File { mut file, path: scratch, tail, .. } => {
                file.seek(SeekFrom::Start(0)).map_err(|e| StoreError::io(&scratch, e))?;
                let mut r = std::io::BufReader::with_capacity(1 << 20, file);
                let mut rec = [0u8; REC];
                loop {
                    match r.read_exact(&mut rec) {
                        Ok(()) => emit(&Frame::from_bytes(&rec))?,
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(e) => return Err(StoreError::io(&scratch, e)),
                    }
                }
                tail.iter().try_for_each(&mut emit)?;
                let _ = std::fs::remove_file(&scratch);
            }
        }
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls_ok(c: &mut Calls, i: u64, pc: u64, l: Option<u32>) -> u32 {
        c.at_instruction(i, pc, l).unwrap()
    }

    #[test]
    fn calls_and_exceptions() {
        let mut c = Calls::default();
        assert_eq!(calls_ok(&mut c, 0, 0x100, Some(0)), TOP_LEVEL);
        c.after_instruction(0, 0x100, 4, Flow::Call).unwrap(); // bl 0x200
        assert_eq!(calls_ok(&mut c, 1, 0x200, Some(0)), 0);
        c.after_instruction(1, 0x200, 2, Flow::Next).unwrap();
        c.exception(2, Some(15)).unwrap();
        assert_eq!(calls_ok(&mut c, 2, 0x300, Some(15)), 1); // handler
        c.after_instruction(2, 0x300, 2, Flow::Return).unwrap(); // bx lr (EXC_RETURN)
        assert_eq!(calls_ok(&mut c, 3, 0x202, Some(0)), 0); // back in the callee
        c.after_instruction(3, 0x202, 2, Flow::Return).unwrap(); // bx lr
        assert_eq!(calls_ok(&mut c, 4, 0x104, Some(0)), TOP_LEVEL); // back at the caller
        assert_eq!((c.frame(0).return_index, c.frame(1).return_index), (4, 3));
        assert_eq!(c.frame(1).depth, 2);
    }

    #[test]
    fn tail_chain_replaces_the_exception_frame() {
        let mut c = Calls::default();
        calls_ok(&mut c, 0, 0x100, None);
        c.exception(1, Some(15)).unwrap();
        calls_ok(&mut c, 1, 0x300, None);
        c.after_instruction(1, 0x300, 2, Flow::Return).unwrap();
        c.exception(2, Some(11)).unwrap();
        assert_eq!(calls_ok(&mut c, 2, 0x400, None), 1);
        assert_eq!(c.frame(1).parent, TOP_LEVEL);
        assert_eq!(c.frame(0).return_index, 2);
        assert!(c.inferred.is_empty());
    }

    #[test]
    fn tail_chain_without_a_record_is_inferred_from_the_level() {
        let mut c = Calls::default();
        calls_ok(&mut c, 0, 0x100, Some(0));
        c.exception(1, Some(11)).unwrap(); // SVC
        calls_ok(&mut c, 1, 0x300, Some(11));
        c.exception(2, Some(15)).unwrap(); // SysTick nested into it
        calls_ok(&mut c, 2, 0x500, Some(15));
        c.after_instruction(2, 0x500, 2, Flow::Return).unwrap();
        // Back in the SVC handler: a nested return, not a tail chain.
        assert_eq!(calls_ok(&mut c, 3, 0x302, Some(11)), 0);
        c.after_instruction(3, 0x302, 2, Flow::Return).unwrap();
        // Still handler mode, level 14 (PendSV): tail-chained, no record.
        assert_eq!(calls_ok(&mut c, 4, 0x600, Some(14)), 2);
        assert_eq!((c.frame(2).number, c.frame(2).parent, c.frame(0).return_index), (14, TOP_LEVEL, 4));
        assert_eq!(c.inferred, [2]);
        c.after_instruction(4, 0x600, 2, Flow::Return).unwrap();
        assert_eq!(calls_ok(&mut c, 5, 0x102, Some(0)), TOP_LEVEL);
        // Unknown level: closed, nothing inferred.
        c.exception(6, None).unwrap();
        calls_ok(&mut c, 6, 0x300, None);
        c.after_instruction(6, 0x300, 2, Flow::Return).unwrap();
        assert_eq!(calls_ok(&mut c, 7, 0x104, None), TOP_LEVEL);
    }

    #[test]
    fn file_store_patches_rows_already_written() {
        use arrow_array::types::{UInt32Type, UInt64Type};
        let dir = tempfile::tempdir().unwrap();
        let mut c = Calls::with_file(&dir.path().join("frames")).unwrap();
        // A long-lived call (main), then many short calls under it.
        calls_ok(&mut c, 0, 0x100, Some(0));
        c.after_instruction(0, 0x100, 4, Flow::Call).unwrap();
        let n = 3 * TAIL as u64;
        for k in 0..n {
            let i = 1 + 2 * k;
            calls_ok(&mut c, i, 0x200, Some(0));
            c.after_instruction(i, 0x200, 4, Flow::Call).unwrap();
            calls_ok(&mut c, i + 1, 0x204, Some(0)); // returned
        }
        let end = 1 + 2 * n;
        assert_eq!(calls_ok(&mut c, end, 0x104, Some(0)), TOP_LEVEL); // main returns
        assert_eq!(c.len() as u64, n + 1);
        let out = dir.path().join("calls.arrow");
        c.write(&out, 1 << 12).unwrap();
        let t = crate::table::Table::open(&out).unwrap();
        let ret = t.col::<UInt64Type>("return_index").unwrap();
        let parent = t.col::<UInt32Type>("parent").unwrap();
        assert_eq!(ret.len() as u64, n + 1);
        assert_eq!(ret.get(0), end, "patched in the file");
        assert_eq!(ret.get(1), 2);
        assert_eq!(ret.get(n as usize), 2 * n);
        assert_eq!(parent.get(n as usize), 0);
        assert!(!dir.path().join("frames").exists());
    }
}
