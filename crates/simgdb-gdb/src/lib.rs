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
                Some(v) => v.to_le_bytes().iter().for_each(|b| write_byte(Some(*b))),
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
}

#[derive(Debug)]
pub enum Error {
    Engine(String),
}

impl Session {
    pub fn new(engine: Engine) -> Session {
        let never = &engine.ds.meta.report.registers_never_written;
        let xml: &'static str = Box::leak(engine.isa().target_xml(never).into_boxed_str());
        let count = engine.isa().registers().len();
        let _ = LAYOUT.set(Layout { xml, count });
        Session { engine, index: 0, breakpoints: BTreeSet::new(), resume: None }
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
                    if pc < start || pc >= end || self.breakpoints.contains(&pc) {
                        self.index = i;
                        return if self.breakpoints.contains(&pc) && (pc < start || pc >= end) {
                            BaseStopReason::SwBreak(())
                        } else {
                            BaseStopReason::DoneStep
                        };
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
                    at_end
                }
            },
            Resume::ReverseContinue => match self.engine.next_execution(&bps, self.index, false) {
                Some(i) => {
                    self.index = i;
                    BaseStopReason::SwBreak(())
                }
                None => {
                    self.index = 0;
                    at_begin
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
        let bytes = (v.value.unwrap() as u32).to_le_bytes();
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
After `goto`, refresh GDB's view: `maintenance flush register-cache` and
`frame` (or use simgdb-goto from tools/simgdb.gdb).";

impl MonitorCmd for Session {
    fn handle_monitor_cmd(&mut self, cmd: &[u8], mut out: ConsoleOutput<'_>) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(cmd);
        let words: Vec<&str> = cmd.split_whitespace().collect();
        match words.as_slice() {
            ["time"] | ["where"] => outputln!(out, "{}", self.describe()),
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
                    Some(TimeUnit::Time(u)) => u.clone(),
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
