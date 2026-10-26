//! Set layer (ARCHITECTURE §4): points, counts and summaries over a range,
//! for scanning whole traces (D15). Results are capped and paged with a
//! cursor (an offset into the full, ordered result); every query is
//! stateless.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Serialize;
use simgdb_model::{Evidence, Point, Value};
use simgdb_store::calls::{FRAME_CALL, FRAME_EXCEPTION, TOP_LEVEL};
use simgdb_store::{MEM_CLOBBER, MEM_FOREIGN, MEM_READ, MEM_WRITE, NONE};

use crate::{Engine, EngineError, RegisterValue};

/// Points `from..=to`. Instruction `i` is in the range when `from <= i <= to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Range {
    pub from: u64,
    pub to: u64,
}

/// A page of an ordered result.
#[derive(Clone, Debug, Serialize)]
pub struct Page<T> {
    /// Size of the whole result.
    pub total: u64,
    pub items: Vec<T>,
    /// Pass back as `cursor` for the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

impl<T: Clone> Page<T> {
    fn of(all: &[T], limit: usize, cursor: u64) -> Page<T> {
        let start = (cursor as usize).min(all.len());
        let end = start.saturating_add(limit).min(all.len());
        Page {
            total: all.len() as u64,
            items: all[start..end].to_vec(),
            next_cursor: (end < all.len()).then_some(end as u64),
        }
    }
}

/// What to find executions of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pc(u64),
    /// `file` matches the path, or its last components (`state.c`,
    /// `src/state.c`).
    Line {
        file: String,
        line: u32,
    },
    Function(String),
}

impl Target {
    /// `0x1f4` or `500` (a PC), `file.c:123` (a line), anything else a function.
    pub fn parse(s: &str) -> Target {
        let s = s.trim();
        if let Some(n) = parse_number(s) {
            return Target::Pc(n);
        }
        if let Some((file, line)) = s.rsplit_once(':')
            && let Ok(line) = line.trim().parse()
        {
            return Target::Line { file: file.trim().to_string(), line };
        }
        Target::Function(s.to_string())
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Target::Pc(pc) => write!(f, "{pc:#x}"),
            Target::Line { file, line } => write!(f, "{file}:{line}"),
            Target::Function(name) => write!(f, "{name}"),
        }
    }
}

pub(crate) fn parse_number(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

pub(crate) fn file_matches(path: &str, file: &str) -> bool {
    path == file || path.ends_with(&format!("/{file}")) || path.ends_with(&format!("\\{file}"))
}

#[derive(Clone, Debug, Serialize)]
pub struct Executions {
    pub target: String,
    /// `pc`, `line` or `function`.
    pub kind: &'static str,
    /// Code addresses that make up the target (for a function: all of its
    /// own code; inlined copies elsewhere are not included).
    pub pcs: Vec<u64>,
    /// Executions of those addresses in the range (instruction count).
    pub instructions: u64,
    /// Points where execution reached the target: each execution of a PC;
    /// for a line, each entry into it (an execution whose previous
    /// instruction in the same context — skipping a handler that interrupted
    /// the line — is not on the line); for a function, each execution of its
    /// entry address (calls, also by tail branch).
    pub entries: Page<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_pc: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExceptionEntry {
    /// First point inside the handler.
    pub entered_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Function at the handler's first instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    /// First point after the handler returned (`None` if it did not within
    /// the trace).
    pub returned_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<u64>,
    /// PC the exception interrupted, and where that comes from: `stacked`
    /// (the return address in the stacked frame), `returned` (where it
    /// returned to, when the trace has no stacking stores: after an RTOS
    /// context switch that is another task's PC), `unknown`.
    pub interrupted_pc: Option<u64>,
    pub interrupted_pc_from: &'static str,
    /// Exception it nested into (its `entered_at`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_in: Option<u64>,
    /// The last instruction before the handler (`entered_at - 1`): for SVC
    /// or a fault, the instruction that raised it; for an interrupt, the
    /// last one completed; for a tail chain, the previous handler's return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_after: Option<InstructionRef>,
    /// The trace's exception record, or (no record: a tail-chained entry
    /// inferred from the exception level) a note saying so.
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstructionRef {
    pub index: u64,
    pub pc: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExceptionList {
    pub range: Range,
    /// Count per exception name (or number).
    pub by_exception: BTreeMap<String, u64>,
    pub exceptions: Page<ExceptionEntry>,
}

/// A node of an aggregated call tree: all calls along the same path of
/// functions.
#[derive(Clone, Debug, Serialize)]
pub struct CallNode {
    pub function: String,
    /// `call` or `exception`.
    pub kind: &'static str,
    pub calls: u64,
    /// Calls that returned within the trace.
    pub returned: u64,
    /// Inclusive instructions, clipped to the range.
    pub instructions: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<CallNode>,
    /// Calls below the depth limit, not shown.
    #[serde(skip_serializing_if = "is_zero")]
    pub deeper_calls: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Per function (or exception handler), over every call or exception
/// entered in the range (recursive calls each count).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FunctionCalls {
    pub function: String,
    /// `call` or `exception` (an exception handler).
    pub kind: &'static str,
    pub calls: u64,
    pub returned: u64,
    /// Sum of (return point - entry point) over the calls that returned:
    /// everything that ran meanwhile, callees and interrupts included.
    pub inclusive_instructions: u64,
    /// For calls that did not return within the trace: instructions from
    /// entry to the end of the range.
    pub unreturned_instructions: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallTree {
    pub range: Range,
    pub depth: u32,
    /// Calls and exceptions entered in the range, by path. A frame whose
    /// caller entered before the range is a root.
    pub roots: Vec<CallNode>,
    pub functions: Vec<FunctionCalls>,
}

/// One memory record that wrote bytes.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WriteRecord {
    /// Instruction whose records hold the write (for an exception entry,
    /// the instruction before it).
    pub written_by: u64,
    /// First point that sees it.
    pub visible_from: u64,
    /// `write`, or `clobber` (an exception entry may have written; the trace
    /// has no store) or `foreign` (another core wrote).
    pub kind: &'static str,
    pub addr: u64,
    pub size: u32,
    /// The value written, as a number in the data byte order at `addr`
    /// (the index's byte order; little-endian in the Arm PPB), `None` if not
    /// shown or not a write.
    pub value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_mask: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pc: Option<u64>,
    /// Function of the writing instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Set when the record also covers bytes outside the queried range:
    /// the part inside it (`??` where not shown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inside: Option<Inside>,
    /// Trace file line.
    pub line: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Inside {
    pub addr: u64,
    pub len: u64,
    pub hex: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LastWrite {
    pub point: Point,
    pub addr: u64,
    pub len: u64,
    /// Distinct records that last wrote each byte, newest first.
    pub writes: Vec<WriteRecord>,
    /// For each byte (in address order), the index into `writes`, or `None`
    /// if no record wrote it before the point.
    pub byte_writer: Vec<Option<usize>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WritesTo {
    pub addr: u64,
    pub len: u64,
    pub range: Range,
    /// Records (of instructions in the range) touching the bytes, in trace order.
    pub writes: Page<WriteRecord>,
}

/// A register write record.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RegisterWrite {
    /// Instruction whose records hold the write.
    pub written_by: u64,
    pub visible_from: u64,
    /// `None` when the trace shows the value as unknown.
    pub value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_mask: Option<u64>,
    /// Not in the trace as such: derived (e.g. the banked SP that SP was).
    pub derived: bool,
    /// Trace file line.
    pub line: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegisterChange {
    pub name: String,
    pub a: Value<u64>,
    pub b: Value<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryChange {
    pub addr: u64,
    pub len: u64,
    /// Bytes at `a` and at `b`, `??` where unknown.
    pub a: String,
    pub b: String,
    /// `changed` (known at both, different), `became_known`,
    /// `became_unknown`.
    pub kind: &'static str,
    /// Variable the run is in (runs never cross a variable boundary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    /// Element or member at the run's first byte, e.g. `pool[1].len`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// ... and at its last byte, when different.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_end: Option<String>,
}

/// Per variable: how many of its bytes differ.
#[derive(Clone, Debug, Serialize)]
pub struct VariableChange {
    pub variable: String,
    pub changed: u64,
    pub became_known: u64,
    pub became_unknown: u64,
    /// Elements or members that changed value (first 32).
    pub paths: Vec<String>,
    /// Decoded value at each point, for a scalar variable known at both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Diff {
    pub a: Point,
    pub b: Point,
    pub registers: Vec<RegisterChange>,
    /// Variables with differing bytes, in address order.
    pub variables: Vec<VariableChange>,
    /// Runs of changed bytes, in address order.
    pub memory: Page<MemoryChange>,
    /// Bytes written between the points whose value is the same at both.
    pub rewritten_same: u64,
    /// Bytes written between the points that are unknown at both.
    pub unknown_both: u64,
}

impl Engine {
    /// The range `from..=to`, defaulting to the whole trace.
    pub fn range(&self, from: Option<u64>, to: Option<u64>) -> Result<Range, EngineError> {
        let (from, to) = (from.unwrap_or(0), to.unwrap_or(self.end()));
        self.check(from)?;
        self.check(to)?;
        if from > to {
            return Err(EngineError::Query(format!("range {from}..={to} is empty")));
        }
        Ok(Range { from, to })
    }

    /// Function symbol address.
    pub fn symbol(&self, name: &str) -> Option<u64> {
        self.ds.symbols.iter().find(|s| s.name == name).map(|s| s.addr)
    }

    /// Code addresses of a target, sorted.
    pub fn target_pcs(&self, target: &Target) -> Result<Vec<u64>, EngineError> {
        let ds = &self.ds;
        let n = ds.src_pc.len();
        let s = |id: u32| ds.strings.get(id as usize).map(String::as_str).unwrap_or("");
        let pcs: Vec<u64> = match target {
            Target::Pc(pc) => vec![*pc],
            Target::Line { file, line } => (0..n)
                .filter(|&i| ds.src_line.get(i) == *line && file_matches(s(ds.src_file.get(i)), file))
                .map(|i| ds.src_pc.get(i))
                .collect(),
            Target::Function(name) => {
                (0..n).filter(|&i| s(ds.src_function.get(i)) == name).map(|i| ds.src_pc.get(i)).collect()
            }
        };
        if pcs.is_empty() {
            return Err(EngineError::Query(format!("no code for {target}")));
        }
        Ok(pcs)
    }

    /// Positions in the PC index of `pc`'s executions within the range.
    fn exec_span(&self, pc: u64, r: Range) -> (usize, usize) {
        let ds = &self.ds;
        let n = ds.pi_pc.len();
        let lo = ds.pi_pc.partition_point(0, n, |p| p < pc);
        let hi = ds.pi_pc.partition_point(lo, n, |p| p <= pc);
        let a = ds.pi_index.partition_point(lo, hi, |i| i < r.from);
        let b = ds.pi_index.partition_point(a, hi, |i| i <= r.to);
        (a, b)
    }

    /// Instruction before `i` in the same context: past an exception that
    /// returned at `i` (and any it tail-chained from), the instruction that
    /// ran before it was taken.
    pub(crate) fn previous_in_context(&self, i: u64) -> Option<u64> {
        let ds = &self.ds;
        let mut j = i.checked_sub(1)?;
        for _ in 0..4096 {
            let f = ds.insn_frame.get(j as usize);
            if f == TOP_LEVEL
                || ds.call_kind.get(f as usize) != FRAME_EXCEPTION
                || ds.call_return.get(f as usize) != j + 1
            {
                break;
            }
            j = ds.call_index.get(f as usize).checked_sub(1)?;
        }
        Some(j)
    }

    pub fn find_executions(
        &self,
        target: &Target,
        range: Range,
        limit: usize,
        cursor: u64,
    ) -> Result<Executions, EngineError> {
        let ds = &self.ds;
        let pcs = self.target_pcs(target)?;
        let mut instructions = 0;
        let mut all: Vec<u64> = Vec::new();
        for &pc in &pcs {
            let (a, b) = self.exec_span(pc, range);
            instructions += (b - a) as u64;
            if !matches!(target, Target::Function(_)) {
                all.extend((a..b).map(|k| ds.pi_index.get(k)));
            }
        }
        let (kind, entry_pc) = match target {
            Target::Pc(_) => ("pc", None),
            Target::Line { .. } => {
                all.sort_unstable();
                let set: HashSet<u64> = pcs.iter().copied().collect();
                all.retain(|&i| match self.previous_in_context(i) {
                    Some(j) => !set.contains(&ds.insn_pc.get(j as usize)),
                    None => true,
                });
                ("line", None)
            }
            Target::Function(name) => {
                let entry = self.symbol(name).ok_or_else(|| EngineError::Query(format!("no symbol {name}")))?;
                let (a, b) = self.exec_span(entry, range);
                all.extend((a..b).map(|k| ds.pi_index.get(k)));
                ("function", Some(entry))
            }
        };
        Ok(Executions {
            target: target.to_string(),
            kind,
            pcs,
            instructions,
            entries: Page::of(&all, limit, cursor),
            entry_pc,
        })
    }

    fn frames_entered(&self, range: Range) -> std::ops::Range<usize> {
        let ds = &self.ds;
        let n = ds.call_entry.len();
        let a = ds.call_entry.partition_point(0, n, |e| e < range.from);
        let b = ds.call_entry.partition_point(a, n, |e| e <= range.to);
        a..b
    }

    /// Function a frame runs: at its first instruction, which for a call
    /// preempted right away comes after the exception returned.
    fn frame_function(&self, f: usize) -> String {
        match self.frame_first_pc(f) {
            Some(pc) => self.source(pc).map(|s| s.function).unwrap_or_else(|| format!("{pc:#x}")),
            None => "???".into(),
        }
    }

    /// Exceptions entered in the range; `kind` keeps only those whose name
    /// (`SysTick`, `IRQ3`, case-insensitive) or number matches.
    pub fn exception_list(
        &self,
        range: Range,
        kind: Option<&str>,
        limit: usize,
        cursor: u64,
    ) -> Result<ExceptionList, EngineError> {
        let ds = &self.ds;
        let mut frames = Vec::new();
        let mut by_exception = BTreeMap::new();
        for f in self.frames_entered(range) {
            if ds.call_kind.get(f) != FRAME_EXCEPTION {
                continue;
            }
            let n = ds.call_number.get(f);
            let key = match n {
                u32::MAX => "unknown".to_string(),
                n => self.isa.exception_name(n).unwrap_or_else(|| n.to_string()),
            };
            if let Some(k) = kind
                && !k.eq_ignore_ascii_case(&key)
                && k.trim() != n.to_string()
            {
                continue;
            }
            *by_exception.entry(key).or_insert(0u64) += 1;
            frames.push(f);
        }
        // Details only for the page asked for.
        let page = Page::of(&frames, limit, cursor);
        let items = page.items.iter().map(|&f| self.exception_entry(f)).collect();
        Ok(ExceptionList {
            range,
            by_exception,
            exceptions: Page { total: page.total, items, next_cursor: page.next_cursor },
        })
    }

    fn exception_entry(&self, f: usize) -> ExceptionEntry {
        let ds = &self.ds;
        let at = ds.call_entry.get(f);
        let n = ds.call_number.get(f);
        let number = (n != u32::MAX).then_some(n);
        let r = ds.call_return.get(f);
        let returned_at = (r != NONE).then_some(r);
        let (interrupted_pc, interrupted_pc_from) = self.interrupted_pc(f);
        // Nearest enclosing exception frame.
        let mut p = ds.call_parent.get(f);
        while p != TOP_LEVEL && ds.call_kind.get(p as usize) != FRAME_EXCEPTION {
            p = ds.call_parent.get(p as usize);
        }
        // Several entries at one point (late arrival): the n-th exception
        // frame entered there goes with the n-th record there.
        let n_before = (0..f)
            .rev()
            .take_while(|&g| ds.call_entry.get(g) == at)
            .filter(|&g| ds.call_kind.get(g) == FRAME_EXCEPTION)
            .count();
        let k = ds.exc_at.partition_point(0, ds.exc_at.len(), |x| x < at);
        let mut j = k;
        let mut skip = n_before;
        while j < ds.exc_at.len() && ds.exc_at.get(j) == at && (skip > 0 || ds.exc_kind.get(j) != 0) {
            if ds.exc_kind.get(j) == 0 {
                skip -= 1;
            }
            j += 1;
        }
        let line = (j < ds.exc_at.len() && ds.exc_at.get(j) == at).then(|| ds.exc_line.get(j));
        ExceptionEntry {
            entered_at: at,
            number,
            name: number.and_then(|n| self.isa.exception_name(n)),
            handler: (at < self.end()).then(|| self.frame_function(f)),
            returned_at,
            instructions: returned_at.map(|r| r - at),
            interrupted_pc,
            interrupted_pc_from,
            nested_in: (p != TOP_LEVEL).then(|| ds.call_entry.get(p as usize)),
            taken_after: (at >= 1 && at - 1 < self.end()).then(|| self.instruction_ref(at - 1)),
            evidence: Evidence {
                index: Some(at),
                line,
                note: line
                    .is_none()
                    .then(|| "no exception record: tail-chained entry inferred from the exception level".to_string()),
            },
        }
    }

    pub(crate) fn instruction_ref(&self, index: u64) -> InstructionRef {
        let pc = self.ds.insn_pc.get(index as usize);
        InstructionRef {
            index,
            pc,
            function: self.function_id(pc).and_then(|id| self.ds.strings.get(id as usize).cloned()),
        }
    }

    /// Function name id (into `strings`) at `pc`.
    fn function_id(&self, pc: u64) -> Option<u32> {
        let ds = &self.ds;
        let n = ds.src_pc.len();
        let i = ds.src_pc.partition_point(0, n, |p| p < pc);
        (i < n && ds.src_pc.get(i) == pc).then(|| ds.src_function.get(i))
    }

    /// First instruction a frame runs (past exceptions taken right at entry).
    fn frame_first_pc(&self, f: usize) -> Option<u64> {
        let ds = &self.ds;
        let mut k = ds.call_entry.get(f);
        for _ in 0..4096 {
            if k >= self.end() {
                return None;
            }
            let g = ds.insn_frame.get(k as usize);
            if g == f as u32 || g == TOP_LEVEL || ds.call_parent.get(g as usize) != f as u32 {
                break;
            }
            let r = ds.call_return.get(g as usize);
            if r == NONE {
                return None;
            }
            k = r;
        }
        Some(ds.insn_pc.get(k as usize))
    }

    pub fn call_tree(&self, range: Range, depth: u32) -> Result<CallTree, EngineError> {
        use std::collections::HashMap;
        let depth = depth.max(1);
        let ds = &self.ds;
        let frames = self.frames_entered(range);
        let first = frames.start;
        let stop = (range.to + 1).min(self.end());
        // Names interned per PC.
        let mut by_pc: HashMap<u64, u32> = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        let mut name_of = |names: &mut Vec<String>, pc: Option<u64>| -> u32 {
            let key = pc.unwrap_or(u64::MAX);
            if let Some(&id) = by_pc.get(&key) {
                return id;
            }
            let name = match pc {
                Some(pc) => match self.function_id(pc) {
                    Some(id) => ds.strings.get(id as usize).cloned().unwrap_or_else(|| "???".into()),
                    None => format!("{pc:#x}"),
                },
                None => "???".into(),
            };
            let id = match names.iter().position(|n| *n == name) {
                Some(i) => i as u32,
                None => {
                    names.push(name);
                    names.len() as u32 - 1
                }
            };
            by_pc.insert(key, id);
            id
        };
        // Arena of tree nodes; node 0 is the virtual root.
        struct Node {
            name: u32,
            call: bool,
            depth: u32,
            calls: u64,
            returned: u64,
            instructions: u64,
            deeper: u64,
            children: HashMap<(u32, bool), usize>,
        }
        let new = |name, call, depth| Node {
            name,
            call,
            depth,
            calls: 0,
            returned: 0,
            instructions: 0,
            deeper: 0,
            children: HashMap::new(),
        };
        let mut nodes = vec![new(u32::MAX, true, 0)];
        let mut node_of: Vec<usize> = Vec::with_capacity(frames.len());
        let mut functions: HashMap<(u32, bool), FunctionCalls> = HashMap::new();
        for f in frames.clone() {
            let call = ds.call_kind.get(f) == FRAME_CALL;
            let name = name_of(&mut names, self.frame_first_pc(f));
            let entry = ds.call_entry.get(f);
            let r = ds.call_return.get(f);
            let returned = r != NONE;
            {
                let s = functions.entry((name, call)).or_insert_with(|| FunctionCalls {
                    function: names[name as usize].clone(),
                    kind: if call { "call" } else { "exception" },
                    calls: 0,
                    returned: 0,
                    inclusive_instructions: 0,
                    unreturned_instructions: 0,
                });
                s.calls += 1;
                if returned {
                    s.returned += 1;
                    s.inclusive_instructions += r - entry;
                } else {
                    s.unreturned_instructions += stop.saturating_sub(entry);
                }
            }
            // Parents are opened before children, so theirs is known.
            let p = ds.call_parent.get(f);
            let parent = if p != TOP_LEVEL && p as usize >= first { node_of[p as usize - first] } else { 0 };
            let node = if nodes[parent].depth >= depth {
                // Below the depth limit: counted on the deepest shown node.
                nodes[parent].deeper += 1;
                parent
            } else {
                let key = (name, call);
                match nodes[parent].children.get(&key) {
                    Some(&c) => c,
                    None => {
                        let d = nodes[parent].depth + 1;
                        nodes.push(new(name, call, d));
                        let c = nodes.len() - 1;
                        nodes[parent].children.insert(key, c);
                        c
                    }
                }
            };
            if node != parent || nodes[parent].depth < depth {
                let n = &mut nodes[node];
                n.calls += 1;
                n.returned += returned as u64;
                n.instructions += if returned { r.min(stop) } else { stop }.saturating_sub(entry);
            }
            node_of.push(node);
        }
        fn build(nodes: &[Node], names: &[String], i: usize) -> Vec<CallNode> {
            let mut out: Vec<CallNode> = nodes[i]
                .children
                .values()
                .map(|&c| {
                    let n = &nodes[c];
                    CallNode {
                        function: names[n.name as usize].clone(),
                        kind: if n.call { "call" } else { "exception" },
                        calls: n.calls,
                        returned: n.returned,
                        instructions: n.instructions,
                        children: build(nodes, names, c),
                        deeper_calls: n.deeper,
                    }
                })
                .collect();
            out.sort_by(|a, b| b.instructions.cmp(&a.instructions).then(a.function.cmp(&b.function)));
            out
        }
        let roots = build(&nodes, &names, 0);
        let mut functions: Vec<FunctionCalls> = functions.into_values().collect();
        functions
            .sort_by(|a, b| b.inclusive_instructions.cmp(&a.inclusive_instructions).then(a.function.cmp(&b.function)));
        Ok(CallTree { range, depth, roots, functions })
    }

    fn write_record(&self, row: usize) -> WriteRecord {
        let ds = &self.ds;
        let at = ds.mem_at.get(row);
        let kind = ds.mem_kind.get(row);
        let size = ds.mem_size.get(row);
        let known = ds.mem_known.get(row) as u64;
        let full = if size >= 8 { u8::MAX as u64 } else { (1u64 << size) - 1 };
        let written = kind == MEM_WRITE && known != 0;
        // Stored by address; present the value in the data byte order there.
        let n = size.min(8) as usize;
        let order = self.endian().at(self.isa.as_ref(), ds.mem_addr.get(row));
        let raw = ds.mem_value.get(row);
        let value = written.then(|| {
            let bytes: Vec<u8> = (0..n).map(|k| (raw >> (8 * k)) as u8).collect();
            order.value(&bytes)
        });
        let mask = (written && (known & full) != full)
            .then(|| (0..n).filter(|&k| known & (1 << k) != 0).fold(0u64, |m, k| m | (0xff << order.shift(k, n))));
        WriteRecord {
            written_by: at.saturating_sub(1),
            visible_from: at,
            kind: match kind {
                MEM_CLOBBER => "clobber",
                MEM_FOREIGN => "foreign",
                _ => "write",
            },
            addr: ds.mem_addr.get(row),
            size,
            value,
            known_mask: mask,
            pc: (at >= 1 && at - 1 < self.end()).then(|| ds.insn_pc.get(at as usize - 1)),
            function: (at >= 1 && at - 1 < self.end())
                .then(|| self.function_id(ds.insn_pc.get(at as usize - 1)))
                .flatten()
                .and_then(|id| ds.strings.get(id as usize).cloned()),
            line: ds.mem_line.get(row),
            inside: None,
        }
    }

    /// The record, marking the part inside `addr..addr+len` if it reaches outside.
    fn write_record_in(&self, row: usize, addr: u64, len: u64) -> WriteRecord {
        let mut w = self.write_record(row);
        let (lo, hi) = (w.addr, w.addr + w.size as u64);
        if lo < addr || hi > addr + len {
            let (a, b) = (lo.max(addr), hi.min(addr + len));
            let hex = (a..b)
                .map(|x| self.row_byte(row, x).map(|v| format!("{v:02x}")).unwrap_or_else(|| "??".into()))
                .collect();
            w.inside = Some(Inside { addr: a, len: b - a, hex });
        }
        w
    }

    /// Memory-index positions of `addr`'s records.
    fn byte_span(&self, addr: u64) -> (usize, usize) {
        let ds = &self.ds;
        let n = ds.mi_addr.len();
        let lo = ds.mi_addr.partition_point(0, n, |a| a < addr);
        (lo, ds.mi_addr.partition_point(lo, n, |a| a <= addr))
    }

    /// Which records last wrote each byte of `addr..addr+len` before `index`.
    pub fn last_write(&self, addr: u64, len: u64, index: u64) -> Result<LastWrite, EngineError> {
        self.check(index)?;
        if len > crate::MAX_MEMORY_REQUEST {
            return Err(EngineError::TooLarge(len));
        }
        self.check_span(addr, len)?;
        let ds = &self.ds;
        let mut rows: Vec<usize> = Vec::new();
        let mut byte_row = Vec::new();
        for k in 0..len {
            let (lo, hi) = self.byte_span(addr + k);
            let mut pos = ds.mi_at.partition_point(lo, hi, |at| at <= index);
            let mut found = None;
            while pos > lo {
                pos -= 1;
                let row = ds.mi_row.get(pos) as usize;
                if ds.mem_kind.get(row) != MEM_READ {
                    found = Some(row);
                    break;
                }
            }
            byte_row.push(found);
            if let Some(r) = found
                && !rows.contains(&r)
            {
                rows.push(r);
            }
        }
        rows.sort_by(|a, b| ds.mem_at.get(*b).cmp(&ds.mem_at.get(*a)).then(b.cmp(a)));
        let byte_writer = byte_row.iter().map(|r| r.map(|r| rows.iter().position(|&x| x == r).unwrap())).collect();
        Ok(LastWrite {
            point: self.point(index)?,
            addr,
            len,
            writes: rows.iter().map(|&r| self.write_record_in(r, addr, len)).collect(),
            byte_writer,
        })
    }

    /// Records of instructions in the range that write any byte of
    /// `addr..addr+len`.
    pub fn writes_to(
        &self,
        addr: u64,
        len: u64,
        range: Range,
        limit: usize,
        cursor: u64,
    ) -> Result<WritesTo, EngineError> {
        if len > crate::MAX_MEMORY_REQUEST {
            return Err(EngineError::TooLarge(len));
        }
        self.check_span(addr, len)?;
        let ds = &self.ds;
        let mut rows = BTreeSet::new();
        for k in 0..len {
            let (lo, hi) = self.byte_span(addr + k);
            // Written by instruction i: visible from i + 1.
            let a = ds.mi_at.partition_point(lo, hi, |at| at < range.from + 1);
            let b = ds.mi_at.partition_point(a, hi, |at| at <= range.to + 1);
            for pos in a..b {
                let row = ds.mi_row.get(pos) as usize;
                if ds.mem_kind.get(row) != MEM_READ {
                    rows.insert((ds.mem_at.get(row), row));
                }
            }
        }
        // Writes visible from the end point are by the last instruction.
        let all: Vec<WriteRecord> = rows.into_iter().map(|(_, r)| self.write_record_in(r, addr, len)).collect();
        let all: Vec<WriteRecord> =
            all.into_iter().filter(|w| w.written_by >= range.from && w.written_by <= range.to).collect();
        Ok(WritesTo { addr, len, range, writes: Page::of(&all, limit, cursor) })
    }

    /// Memory-table rows with `at` in `(a, b]` (rows are in trace order;
    /// `at` is the instruction count so far, or one more).
    fn rows_between(&self, a: u64, b: u64) -> std::ops::Range<usize> {
        let ds = &self.ds;
        let n = ds.mem_at.len();
        let t = a.saturating_sub(2);
        let mut lo = ds.mem_at.partition_point(0, n, |at| at < t);
        while lo > 0 && ds.mem_at.get(lo - 1) >= t {
            lo -= 1;
        }
        let mut hi = lo;
        while hi < n && ds.mem_at.get(hi) <= b + 1 {
            hi += 1;
        }
        lo..hi
    }

    /// What differs between points `a` and `b` (registers, and memory bytes
    /// with records between them).
    pub fn diff(&self, a: u64, b: u64, limit: usize, cursor: u64) -> Result<Diff, EngineError> {
        self.check(a)?;
        self.check(b)?;
        let (lo, hi) = (a.min(b), a.max(b));
        let ra = self.registers(a)?;
        let rb = self.registers(b)?;
        let registers = ra
            .into_iter()
            .zip(rb)
            .filter(|(x, y)| x.value.value != y.value.value || x.value.known_mask != y.value.known_mask)
            .map(|(x, y): (RegisterValue, RegisterValue)| RegisterChange {
                name: x.name.to_string(),
                a: x.value,
                b: y.value,
            })
            .collect();
        let ds = &self.ds;
        let mut addrs = BTreeSet::new();
        for row in self.rows_between(lo, hi) {
            let at = ds.mem_at.get(row);
            if at <= lo || at > hi {
                continue;
            }
            let base = ds.mem_addr.get(row);
            let size = ds.mem_size.get(row) as u64;
            if addrs.len() as u64 + size > 16 * crate::MAX_MEMORY_REQUEST {
                return Err(EngineError::Query(format!(
                    "more than {} bytes touched between the points; narrow the range",
                    16 * crate::MAX_MEMORY_REQUEST
                )));
            }
            addrs.extend(base..base.saturating_add(size));
        }
        let mut changes: Vec<MemoryChange> = Vec::new();
        let mut variables: Vec<VariableChange> = Vec::new();
        let mut rewritten_same = 0;
        let mut unknown_both = 0;
        let hex = |v: &Value<u8>| v.value.map(|x| format!("{x:02x}")).unwrap_or_else(|| "??".into());
        let vars = &self.ds.variables;
        let mut last: Option<(u64, &'static str, Option<u64>)> = None;
        for addr in addrs {
            let x = self.byte(addr, a);
            let y = self.byte(addr, b);
            let kind = match (x.value, y.value) {
                (Some(p), Some(q)) if p != q => "changed",
                (None, Some(_)) => "became_known",
                (Some(_), None) => "became_unknown",
                (None, None) => {
                    unknown_both += 1;
                    last = None;
                    continue;
                }
                _ => {
                    rewritten_same += 1;
                    last = None;
                    continue;
                }
            };
            let var = vars.containing(addr);
            let var_addr = var.map(|v| v.addr);
            if let Some(v) = var {
                if variables.last().is_none_or(|c| c.variable != v.name) {
                    variables.push(VariableChange {
                        variable: v.name.clone(),
                        changed: 0,
                        became_known: 0,
                        became_unknown: 0,
                        paths: vec![],
                        a: self.scalar_value(v, a),
                        b: self.scalar_value(v, b),
                    });
                }
                let c = variables.last_mut().unwrap();
                match kind {
                    "changed" => {
                        c.changed += 1;
                        if let Some(p) = vars.path_at(addr) {
                            let p = p.split('+').next().unwrap().to_string();
                            if c.paths.len() < 32 && !c.paths.contains(&p) {
                                c.paths.push(p);
                            }
                        }
                    }
                    "became_known" => c.became_known += 1,
                    _ => c.became_unknown += 1,
                }
            }
            match (last, changes.last_mut()) {
                (Some((prev, k, v)), Some(c)) if prev + 1 == addr && k == kind && v == var_addr => {
                    c.len += 1;
                    c.a.push_str(&hex(&x));
                    c.b.push_str(&hex(&y));
                    let end = vars.path_at(addr);
                    c.path_end = if end == c.path { None } else { end };
                }
                _ => changes.push(MemoryChange {
                    addr,
                    len: 1,
                    a: hex(&x),
                    b: hex(&y),
                    kind,
                    variable: var.map(|v| v.name.clone()),
                    path: vars.path_at(addr),
                    path_end: None,
                }),
            }
            last = Some((addr, kind, var_addr));
        }
        Ok(Diff {
            a: self.point(a)?,
            b: self.point(b)?,
            registers,
            variables,
            memory: Page::of(&changes, limit, cursor),
            rewritten_same,
            unknown_both,
        })
    }

    /// Decoded value of a scalar variable at `index`, if fully known.
    fn scalar_value(&self, v: &simgdb_elf::variables::Variable, index: u64) -> Option<serde_json::Value> {
        let size = v.size.filter(|&s| s <= 8)?;
        let raw = self.combine(v.addr, &self.memory(v.addr, size, index).ok()?);
        match (raw.value, raw.known_mask) {
            (Some(x), None) => self.decode(v.ty, x, size),
            _ => None,
        }
    }

    /// Writes of one register in the range, in trace order.
    pub fn register_history(
        &self,
        name: &str,
        range: Range,
        limit: usize,
        cursor: u64,
    ) -> Result<Page<RegisterWrite>, EngineError> {
        let reg = self.register_id(name)?;
        let ds = &self.ds;
        let n = ds.rw_at.len();
        // A write by instruction i is visible from i + 1.
        let lo = ds.rw_at.partition_point(0, n, |at| at < range.from + 1);
        let hi = ds.rw_at.partition_point(lo, n, |at| at <= range.to + 1);
        let all: Vec<RegisterWrite> = (lo..hi)
            .filter(|&row| ds.rw_reg.get(row) == reg)
            .map(|row| {
                let at = ds.rw_at.get(row);
                let known = ds.rw_known.get(row);
                let bits = self.isa.registers()[reg as usize].bits;
                let full = simgdb_isa::RegState::full_mask(bits);
                RegisterWrite {
                    written_by: at.saturating_sub(1),
                    visible_from: at,
                    value: (known != 0).then(|| ds.rw_value.get(row)),
                    known_mask: (known & full != full).then_some(known),
                    derived: ds.rw_derived.get(row) != 0,
                    line: ds.rw_line.get(row),
                }
            })
            .filter(|w| w.written_by >= range.from && w.written_by <= range.to)
            .collect();
        Ok(Page::of(&all, limit, cursor))
    }

    /// `addr..addr+len` lies inside the ISA's address space.
    pub(crate) fn check_span(&self, addr: u64, len: u64) -> Result<(), EngineError> {
        match addr.checked_add(len) {
            Some(end) if end <= self.isa.address_limit() => Ok(()),
            _ => Err(EngineError::Query(format!("{addr:#x} + {len} is outside the address space"))),
        }
    }

    /// Variable containing `addr`, as `name+offset`.
    pub fn variable_at(&self, addr: u64) -> Option<String> {
        let vars = &self.ds.variables;
        vars.variables.iter().find_map(|v| {
            let size = v.size?;
            (addr >= v.addr && addr - v.addr < size)
                .then(|| if addr == v.addr { v.name.clone() } else { format!("{}+{}", v.name, addr - v.addr) })
        })
    }
}
