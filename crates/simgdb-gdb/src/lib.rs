//! GDB remote serial protocol server (ARCHITECTURE §5).
//!
//! A thin front end: every answer comes from the engine. The server keeps
//! only a session position (the current point) and the breakpoint set.
//!
//! | packet | handling |
//! |---|---|
//! | `qXfer:features:read` | ISA target description |
//! | `g`, `p`, `m` | engine state at the current point; unknown → `xx` / error |
//! | `s`, `c`, `vCont` (incl. range step) | next point from the PC index |
//! | `bs`, `bc` | previous point (reverse execution) |
//! | `Z0`/`z0` | breakpoint set (any kind) |
//! | `G`, `P`, `M`, `X` | refused: the record is read-only |
//! | `qRcmd` | `monitor time`, `goto`, `info`, `help` |
//!
//! GDB's navigable range is `0..=end-1`: every point where the PC is known.
//! Running past either end stops with a replay-log stop reply, which GDB
//! reports as the end (or start) of the recorded history.

use std::collections::BTreeSet;
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::sync::OnceLock;

use gdbstub::arch::{Arch, RegId, Registers};
use gdbstub::common::Signal;
use gdbstub::conn::Connection;
use gdbstub::stub::{BaseStopReason, DisconnectReason, GdbStub, SingleThreadStopReason, run_blocking};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::reverse_exec::{
    ReplayLogPosition, ReverseCont, ReverseContOps, ReverseStep, ReverseStepOps,
};
use gdbstub::target::ext::base::single_register_access::{SingleRegisterAccess, SingleRegisterAccessOps};
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadRangeStepping, SingleThreadRangeSteppingOps, SingleThreadResume,
    SingleThreadResumeOps, SingleThreadSingleStep, SingleThreadSingleStepOps,
};
use gdbstub::target::ext::breakpoints::{Breakpoints, BreakpointsOps, SwBreakpoint, SwBreakpointOps};
use gdbstub::target::ext::monitor_cmd::{ConsoleOutput, MonitorCmd, MonitorCmdOps, outputln};
use gdbstub::target::{Target, TargetError, TargetResult};
use simgdb_engine::Engine;
use simgdb_model::TimeUnit;

/// Register file layout for the GDB connection, taken from the ISA once.
struct Layout {
    xml: &'static str,
    count: usize,
    /// GDB reads register bytes in the target's data byte order (it takes
    /// that from the ELF it loaded).
    big_endian: bool,
}

fn reg_bytes(v: u32) -> [u8; 4] {
    if layout().big_endian { v.to_be_bytes() } else { v.to_le_bytes() }
}

static LAYOUT: OnceLock<Layout> = OnceLock::new();

fn layout() -> &'static Layout {
    LAYOUT.get().expect("layout is set before the server starts")
}

/// 32-bit target whose registers and description come from the ISA layer.
pub enum Arch32 {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Regs {
    /// `None` = unknown at this point: sent as `xx` (GDB shows `<unavailable>`).
    pub values: Vec<Option<u32>>,
    pc: u32,
}

impl Registers for Regs {
    type ProgramCounter = u32;

    fn pc(&self) -> u32 {
        self.pc
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for v in &self.values {
            match v {
                Some(v) => reg_bytes(*v).iter().for_each(|b| write_byte(Some(*b))),
                None => (0..4).for_each(|_| write_byte(None)),
            }
        }
    }

    fn gdb_deserialize(&mut self, _bytes: &[u8]) -> Result<(), ()> {
        // Writes are refused before they reach here.
        Err(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Reg(pub usize);

impl RegId for Reg {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        (id < layout().count).then(|| (Reg(id), NonZeroUsize::new(4)))
    }
}

impl Arch for Arch32 {
    type Usize = u32;
    type Registers = Regs;
    type BreakpointKind = usize;
    type RegId = Reg;

    fn target_description_xml() -> Option<&'static str> {
        Some(layout().xml)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    Continue,
    Step,
    Range(u64, u64),
    ReverseStep,
    ReverseContinue,
}

/// One debugging session over an engine.
pub struct Session {
    engine: Engine,
    /// Current point.
    index: u64,
    breakpoints: BTreeSet<u64>,
    resume: Option<Resume>,
    /// Bookmarks shared with MCP clients (absent if it could not be opened).
    annotations: Option<simgdb_store::annotations::Annotations>,
    /// Report the ends of the trace as a plain stop instead of "no more history".
    end_as_pause: bool,
}

#[derive(Debug)]
pub enum Error {
    Engine(String),
}

/// Registers unknown at some point. Indexes built before the indexer
/// recorded this fall back to what is visible cheaply: registers never
/// written or not fully known at the first point (this misses a register
/// that is known at the start and becomes unknown later).
fn not_always_known(engine: &Engine) -> Vec<String> {
    let report = &engine.ds.meta.report;
    if let Some(list) = &report.registers_not_always_known {
        return list.clone();
    }
    let mut list = report.registers_never_written.clone();
    if let Ok(regs) = engine.registers(0) {
        for r in regs {
            let partial = r.value.value.is_none() || r.value.known_mask.is_some();
            if partial && !list.iter().any(|n| n == r.name) {
                list.push(r.name.to_string());
            }
        }
    }
    list
}

impl Session {
    pub fn new(engine: Engine) -> Session {
        Session::with_stack_registers(engine, false)
    }

    /// `avoid_unknown`: describe registers that are unknown at some point
    /// (not only those never written) so GDB does not depend on them. On
    /// Arm M-profile this keeps GDB from failing every stack query while
    /// MSP or PSP is unknown (before the first PSP write), which IDE front
    /// ends treat as fatal; the cost is that GDB no longer unwinds exception
    /// frames on the process stack correctly (see docs/USAGE.md).
    pub fn with_stack_registers(engine: Engine, avoid_unknown: bool) -> Session {
        let unknown = if avoid_unknown {
            not_always_known(&engine)
        } else {
            engine.ds.meta.report.registers_never_written.clone()
        };
        let xml: &'static str = Box::leak(engine.isa().target_xml(&unknown).into_boxed_str());
        let count = engine.isa().registers().len();
        let big_endian = engine.endian() == simgdb_isa::Endian::Big;
        let _ = LAYOUT.set(Layout { xml, count, big_endian });
        Session { engine, index: 0, breakpoints: BTreeSet::new(), resume: None, annotations: None, end_as_pause: false }
    }

    /// Report reaching either end of the trace as a plain stop (SIGTRAP, as
    /// for an interrupt request) instead of GDB's "no more history".
    pub fn with_end_as_pause(mut self, on: bool) -> Session {
        self.end_as_pause = on;
        self
    }

    /// Use this bookmark store for `monitor bookmark`.
    pub fn with_annotations(mut self, a: simgdb_store::annotations::Annotations) -> Session {
        self.annotations = Some(a);
        self
    }

    /// Last point GDB can stop at (the PC is known there).
    fn last(&self) -> u64 {
        self.engine.end().saturating_sub(1)
    }

    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn set_index(&mut self, index: u64) {
        self.index = index.min(self.last());
    }

    /// Carry out the pending resume request and say why we stopped.
    fn run(&mut self) -> SingleThreadStopReason<u32> {
        let at_end = BaseStopReason::ReplayLog { tid: None, pos: ReplayLogPosition::End };
        let at_begin = BaseStopReason::ReplayLog { tid: None, pos: ReplayLogPosition::Begin };
        // A plain stop for continue only: while stepping, GDB takes a trap
        // that did not leave the step range as "not done yet" and steps
        // again, for ever, at the end of the trace.
        let (cont_end, cont_begin) = if self.end_as_pause {
            (BaseStopReason::Signal(Signal::SIGTRAP), BaseStopReason::Signal(Signal::SIGTRAP))
        } else {
            (at_end, at_begin)
        };
        let bps: Vec<u64> = self.breakpoints.iter().copied().collect();
        match self.resume.take().unwrap_or(Resume::Step) {
            Resume::Step => {
                if self.index >= self.last() {
                    return at_end;
                }
                self.index += 1;
                BaseStopReason::DoneStep
            }
            Resume::ReverseStep => {
                if self.index == 0 {
                    return at_begin;
                }
                self.index -= 1;
                BaseStopReason::DoneStep
            }
            Resume::Range(start, end) => {
                let mut i = self.index;
                loop {
                    if i >= self.last() {
                        self.index = self.last();
                        return at_end;
                    }
                    i += 1;
                    let pc = self.engine.ds.insn_pc.get(i as usize);
                    let hit = self.breakpoints.contains(&pc);
                    if pc < start || pc >= end || hit {
                        self.index = i;
                        return if hit { BaseStopReason::SwBreak(()) } else { BaseStopReason::DoneStep };
                    }
                }
            }
            Resume::Continue => match self.engine.next_execution(&bps, self.index, true) {
                Some(i) if i <= self.last() => {
                    self.index = i;
                    BaseStopReason::SwBreak(())
                }
                _ => {
                    self.index = self.last();
                    cont_end
                }
            },
            Resume::ReverseContinue => match self.engine.next_execution(&bps, self.index, false) {
                Some(i) => {
                    self.index = i;
                    BaseStopReason::SwBreak(())
                }
                None => {
                    self.index = 0;
                    cont_begin
                }
            },
        }
    }

    fn describe(&self) -> String {
        let p = self.engine.point(self.index).ok();
        let pc = self.engine.pc(self.index).ok().and_then(|v| v.value);
        let time =
            p.and_then(|p| p.sim_time).map(|t| format!("{} {}", t.value, t.unit.name())).unwrap_or_else(|| "-".into());
        format!(
            "index {} of {} (last {}), time {}, pc {}{}",
            self.index,
            self.engine.end(),
            self.last(),
            time,
            pc.map(|p| format!("{p:#x}")).unwrap_or_else(|| "unknown".into()),
            if self.engine.condition_failed(self.index) { ", condition failed (not executed)" } else { "" }
        )
    }
}

fn engine_err<E: std::fmt::Display>(e: E) -> TargetError<Error> {
    TargetError::Fatal(Error::Engine(e.to_string()))
}

impl Target for Session {
    type Arch = Arch32;
    type Error = Error;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }

    fn support_monitor_cmd(&mut self) -> Option<MonitorCmdOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadBase for Session {
    fn read_registers(&mut self, regs: &mut Regs) -> TargetResult<(), Self> {
        let values = self.engine.registers(self.index).map_err(engine_err)?;
        regs.values = values.iter().map(|r| r.value.is_known().then(|| r.value.value.unwrap() as u32)).collect();
        regs.pc = regs.values.get(self.engine.isa().pc() as usize).copied().flatten().unwrap_or(0);
        Ok(())
    }

    fn write_registers(&mut self, _regs: &Regs) -> TargetResult<(), Self> {
        Err(TargetError::NonFatal)
    }

    fn support_single_register_access(&mut self) -> Option<SingleRegisterAccessOps<'_, (), Self>> {
        Some(self)
    }

    fn read_addrs(&mut self, start: u32, data: &mut [u8]) -> TargetResult<usize, Self> {
        let bytes = self.engine.memory(start as u64, data.len() as u64, self.index).map_err(engine_err)?;
        let mut n = 0;
        for (slot, b) in data.iter_mut().zip(&bytes) {
            match b.value {
                Some(v) => *slot = v,
                None => break,
            }
            n += 1;
        }
        // GDB reports "Cannot access memory" for a read with no known bytes.
        if n == 0 { Err(TargetError::NonFatal) } else { Ok(n) }
    }

    fn write_addrs(&mut self, _start: u32, _data: &[u8]) -> TargetResult<(), Self> {
        Err(TargetError::NonFatal)
    }

    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleRegisterAccess<()> for Session {
    fn read_register(&mut self, _tid: (), reg: Reg, buf: &mut [u8]) -> TargetResult<usize, Self> {
        let v = self.engine.register(self.index, reg.0 as u16).map_err(engine_err)?;
        if !v.is_known() {
            return Ok(0);
        }
        let bytes = reg_bytes(v.value.unwrap() as u32);
        buf[..4].copy_from_slice(&bytes);
        Ok(4)
    }

    fn write_register(&mut self, _tid: (), _reg: Reg, _val: &[u8]) -> TargetResult<(), Self> {
        Err(TargetError::NonFatal)
    }
}

impl SingleThreadResume for Session {
    fn resume(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.resume = Some(Resume::Continue);
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }

    fn support_range_step(&mut self) -> Option<SingleThreadRangeSteppingOps<'_, Self>> {
        Some(self)
    }

    fn support_reverse_step(&mut self) -> Option<ReverseStepOps<'_, (), Self>> {
        Some(self)
    }

    fn support_reverse_cont(&mut self) -> Option<ReverseContOps<'_, (), Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for Session {
    fn step(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.resume = Some(Resume::Step);
        Ok(())
    }
}

impl SingleThreadRangeStepping for Session {
    fn resume_range_step(&mut self, start: u32, end: u32) -> Result<(), Self::Error> {
        self.resume = Some(Resume::Range(start as u64, end as u64));
        Ok(())
    }
}

impl ReverseStep<()> for Session {
    fn reverse_step(&mut self, _tid: ()) -> Result<(), Self::Error> {
        self.resume = Some(Resume::ReverseStep);
        Ok(())
    }
}

impl ReverseCont<()> for Session {
    fn reverse_cont(&mut self) -> Result<(), Self::Error> {
        self.resume = Some(Resume::ReverseContinue);
        Ok(())
    }
}

impl Breakpoints for Session {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for Session {
    fn add_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        self.breakpoints.insert(addr as u64);
        Ok(true)
    }

    fn remove_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.breakpoints.remove(&(addr as u64)))
    }
}

const HELP: &str = "\
SimGDB monitor commands:
  monitor time                 current point: index, trace time, pc
  monitor goto <index>         move to an instruction index
  monitor goto index <index>   same
  monitor goto time <t>        move to the first instruction at or after trace time t
  monitor info                 dataset capabilities and limitations
  monitor info source          where the current values come from (registers, location)
  monitor info source <var|addr> [len]   bytes with source and the trace line that set them
  monitor where                location and call stack from the trace flow
  monitor hits <file:line|function|pc> [n]   how often it ran; first n entry points
  monitor find <condition>     next point where the condition holds (see docs/USAGE.md)
  monitor find -back <condition>   previous such point
  monitor bookmark <note>      bookmark the current point (shared with AI clients)
  monitor bookmarks            list bookmarks
  monitor reset                go to the first point (for IDEs); `monitor halt` does nothing
After `goto`, `find` or `reset`, refresh GDB's view: `maintenance flush register-cache` and
`frame` (or use the simgdb-* commands from tools/simgdb.gdb).";

impl MonitorCmd for Session {
    fn handle_monitor_cmd(&mut self, cmd: &[u8], mut out: ConsoleOutput<'_>) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(cmd);
        let words: Vec<&str> = cmd.split_whitespace().collect();
        match words.as_slice() {
            ["time"] => outputln!(out, "{}", self.describe()),
            ["goto", rest @ ..] => {
                let target = match rest {
                    [n] | ["index", n] => parse_number(n).ok_or_else(|| format!("not a number: {n}")),
                    ["time", t] => parse_number(t).ok_or_else(|| format!("not a number: {t}")).and_then(|t| {
                        self.engine.index_at_time(t).ok_or_else(|| format!("no instruction at or after time {t}"))
                    }),
                    _ => Err("usage: monitor goto <index> | goto index <n> | goto time <t>".to_string()),
                };
                match target {
                    Ok(i) if i <= self.last() => {
                        self.index = i;
                        outputln!(out, "{}", self.describe());
                    }
                    Ok(i) => outputln!(out, "index {i} is past the last point {}", self.last()),
                    Err(e) => outputln!(out, "{e}"),
                }
            }
            ["info"] => {
                let info = self.engine.info();
                outputln!(out, "trace: {}", info.trace);
                outputln!(out, "elf: {}", info.elf.as_deref().unwrap_or("-"));
                outputln!(out, "instructions: {}", info.instructions);
                let unit = match &info.time_unit {
                    Some(TimeUnit::Time(u)) => u.to_string(),
                    Some(TimeUnit::Cycle(u)) => format!("{u} (cycles)"),
                    _ => "-".into(),
                };
                outputln!(out, "time: {} .. {} {unit}", opt(info.first_time), opt(info.last_time));
                let c = &info.capabilities;
                outputln!(
                    out,
                    "records: registers {}, memory values {}, memory writes {}, exceptions {}, exception stacking stores {}",
                    yes(c.register_writes),
                    yes(c.memory_values),
                    yes(c.memory_writes),
                    yes(c.exceptions),
                    yes(c.exception_stacking_writes)
                );
                for l in &info.limitations {
                    outputln!(out, "limitation: {l}");
                }
            }
            ["info", "source"] => {
                let regs = self.engine.registers(self.index).map_err(|e| Error::Engine(e.to_string()))?;
                let mut by: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
                for r in &regs {
                    let key = match (r.value.value, r.value.known_mask) {
                        (_, Some(_)) => "partly known".to_string(),
                        (Some(_), None) => format!("{:?}", r.value.source).to_lowercase(),
                        (None, _) => "unknown".to_string(),
                    };
                    by.entry(key).or_default().push(r.name);
                }
                outputln!(out, "{}", self.describe());
                for (k, names) in by {
                    outputln!(out, "registers from {k}: {}", names.join(" "));
                }
                let c = &self.engine.info().capabilities;
                outputln!(
                    out,
                    "memory: last trace record of each byte; else the ELF image; else a later read (inferred); else unknown{}",
                    if c.exception_stacking_writes {
                        ""
                    } else {
                        " (exception frames are unknown: no stacking stores)"
                    }
                );
            }
            ["info", "source", what, rest @ ..] => {
                let (addr, len) = match self.engine.variable(what) {
                    Ok(p) => (p.addr, p.size.unwrap_or(4)),
                    Err(_) => match parse_number(what) {
                        Some(a) => (a, 4),
                        None => {
                            outputln!(out, "not a variable or address: {what}");
                            return Ok(());
                        }
                    },
                };
                let len = rest.first().and_then(|l| parse_number(l)).unwrap_or(len).min(256);
                let bytes = self.engine.memory(addr, len, self.index).map_err(|e| Error::Engine(e.to_string()))?;
                for (k, b) in bytes.iter().enumerate() {
                    let v = b.value.map(|v| format!("{v:02x}")).unwrap_or_else(|| "??".into());
                    let ev = b
                        .evidence
                        .first()
                        .map(|e| {
                            let mut s = String::new();
                            if let Some(i) = e.index {
                                s += &format!(" index {i}");
                            }
                            if let Some(l) = e.line {
                                s += &format!(" line {l}");
                            }
                            if let Some(n) = &e.note {
                                s += &format!(" ({n})");
                            }
                            s
                        })
                        .unwrap_or_default();
                    outputln!(out, "{:#010x}: {v} {:?}{ev}", addr + k as u64, b.source);
                }
            }
            ["where"] | ["locate"] => {
                let loc = self.engine.locate(self.index).map_err(|e| Error::Engine(e.to_string()))?;
                outputln!(out, "{}", self.describe());
                for (k, f) in loc.call_stack.iter().enumerate() {
                    let at = match &f.source {
                        Some(s) => {
                            format!("{} ({}:{})", s.function, s.file.rsplit('/').next().unwrap_or(&s.file), s.line)
                        }
                        None => f.pc.map(|p| format!("{p:#x}")).unwrap_or_else(|| "?".into()),
                    };
                    outputln!(
                        out,
                        "#{k} {at} [{}{}]",
                        f.kind,
                        f.exception.map(|n| format!(" {n}")).unwrap_or_default()
                    );
                }
            }
            ["hits", target, rest @ ..] => {
                let n = rest.first().and_then(|l| parse_number(l)).unwrap_or(10) as usize;
                let all = self.engine.range(None, None).map_err(|e| Error::Engine(e.to_string()))?;
                match self.engine.find_executions(&simgdb_engine::Target::parse(target), all, n, 0) {
                    Ok(x) => {
                        outputln!(
                            out,
                            "{target}: {} instructions executed, {} entries",
                            x.instructions,
                            x.entries.total
                        );
                        for i in &x.entries.items {
                            let mark = if *i == self.index { "  <- here" } else { "" };
                            outputln!(out, "  index {i}{mark}");
                        }
                        if x.entries.next_cursor.is_some() {
                            outputln!(out, "  ... (monitor hits {target} <n> for more)");
                        }
                    }
                    Err(e) => outputln!(out, "{e}"),
                }
            }
            ["find", rest @ ..] if !rest.is_empty() => {
                let (back, cond) = match rest {
                    ["-back", c @ ..] => (true, c.join(" ")),
                    c => (false, c.join(" ")),
                };
                let range = if back {
                    self.engine.range(Some(0), Some(self.index.saturating_sub(1)))
                } else {
                    self.engine.range(Some((self.index + 1).min(self.last())), Some(self.last()))
                }
                .map_err(|e| Error::Engine(e.to_string()))?;
                let dir = if back { simgdb_engine::Direction::Backward } else { simgdb_engine::Direction::Forward };
                match self.engine.find_first(&cond, range, dir) {
                    Ok(f) => match f.found {
                        Some(p) => {
                            outputln!(out, "found at index {} (monitor goto {} to go there)", p.index, p.index);
                            if let Some(w) = &f.source {
                                outputln!(
                                    out,
                                    "  in {} ({}:{})",
                                    w.function,
                                    w.file.rsplit('/').next().unwrap_or(&w.file),
                                    w.line
                                );
                            }
                        }
                        None => outputln!(out, "not found ({} points checked, {} undecided)", f.evaluated, f.undecided),
                    },
                    Err(e) => outputln!(out, "{e}"),
                }
            }
            ["bookmark", note @ ..] if !note.is_empty() => match &self.annotations {
                Some(a) => match a.add_bookmark(self.index, &note.join(" "), "gdb") {
                    Ok(b) => outputln!(out, "bookmark {} at index {}", b.id, b.index),
                    Err(e) => outputln!(out, "{e}"),
                },
                None => outputln!(out, "bookmarks are not available (no annotations database)"),
            },
            ["bookmarks"] => match &self.annotations {
                Some(a) => match a.bookmarks(0, u64::MAX) {
                    Ok(list) if list.is_empty() => outputln!(out, "no bookmarks"),
                    Ok(list) => {
                        for b in list {
                            outputln!(out, "index {}: {} ({})", b.index, b.note, b.author);
                        }
                    }
                    Err(e) => outputln!(out, "{e}"),
                },
                None => outputln!(out, "bookmarks are not available (no annotations database)"),
            },
            ["halt"] => outputln!(out, "SimGDB is a recording: it is always halted"),
            ["reset", ..] => {
                self.index = 0;
                outputln!(out, "{}", self.describe());
            }
            [] | ["help"] => outputln!(out, "{HELP}"),
            _ => outputln!(out, "unknown command: {cmd}\n{HELP}"),
        }
        Ok(())
    }
}

fn parse_number(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn opt(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

struct EventLoop;

impl run_blocking::BlockingEventLoop for EventLoop {
    type Target = Session;
    type Connection = TcpStream;
    type StopReason = SingleThreadStopReason<u32>;

    #[allow(clippy::type_complexity)]
    fn wait_for_stop_reason(
        target: &mut Session,
        _conn: &mut TcpStream,
    ) -> Result<
        run_blocking::Event<Self::StopReason>,
        run_blocking::WaitForStopReasonError<<Session as Target>::Error, <TcpStream as Connection>::Error>,
    > {
        // Every resume is answered from the index at once: nothing runs.
        Ok(run_blocking::Event::TargetStopped(target.run()))
    }

    fn on_interrupt(_target: &mut Session) -> Result<Option<Self::StopReason>, <Session as Target>::Error> {
        Ok(Some(BaseStopReason::Signal(Signal::SIGINT)))
    }
}

/// Serve one GDB connection; returns why it ended.
pub fn serve_connection(session: &mut Session, stream: TcpStream) -> Result<DisconnectReason, String> {
    let _ = stream.set_nodelay(true);
    let stub = GdbStub::new(stream);
    stub.run_blocking::<EventLoop>(session).map_err(|e| format!("{e:?}"))
}

/// Accept GDB connections one after another, keeping the session position.
pub fn serve(session: &mut Session, listener: TcpListener, once: bool) -> Result<(), String> {
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| e.to_string())?;
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        eprintln!("gdb connected from {peer}; {}", session.describe());
        match serve_connection(session, stream) {
            Ok(reason) => eprintln!("gdb disconnected: {reason:?}"),
            Err(e) => eprintln!("gdb connection ended with an error: {e}"),
        }
        if once {
            break;
        }
    }
    Ok(())
}
