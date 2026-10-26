//! MCP front end (ARCHITECTURE §5): the engine as a few atomic, stateless
//! tools. Every tool takes its point or range explicitly, answers JSON with
//! provenance (`source`, `evidence`), says `unknown` instead of guessing, and
//! caps list sizes (summary first, then pages with a cursor).
//!
//! Logic stays in the engine; this crate only parses arguments, calls the
//! engine, presents addresses in hex and logs the call (tool, arguments,
//! result summary) to the annotations database for evaluation.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use simgdb_engine::{Direction, Engine, EngineError, Range, SourcePos, Target};
use simgdb_store::annotations::Annotations;

/// The skill document: how to use the tools and analysis procedures.
pub const SKILL: &str = include_str!("../skill/SKILL.md");

/// Default and largest list sizes.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 1000;

pub const INSTRUCTIONS: &str = "SimGDB answers questions about one recorded simulation run (a Tarmac trace \
plus the firmware ELF). A point is an instruction index: point i is the state just before instruction i \
executes (what a debugger shows stopped at that PC); effects of instruction i are visible from point i+1. \
Every tool takes its point or range explicitly; give `at` (index) or `time` (trace time stamp). \
Addresses and PCs are hex strings; indexes are decimal numbers. \
Values carry `source` (trace, elf_initial, inferred, unknown) and `evidence` (instruction index and trace \
line). `unknown` means the trace does not show it: never treat it as zero. Start with trace_info. \
Lists are capped: use `total`, then `cursor` = `next_cursor` for more.";

/// Keys whose numbers are addresses or PCs: shown as hex strings.
const HEX_KEYS: &[&str] =
    &["pc", "pcs", "addr", "entry_pc", "interrupted_pc", "returns_to", "call_pc", "elf_entry", "known_mask"];

/// Rewrite address-like numbers as `0x...` strings, everywhere in `v`.
fn hexify(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                if HEX_KEYS.contains(&k.as_str()) {
                    hex_numbers(x);
                } else {
                    hexify(x);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hexify),
        _ => {}
    }
}

fn hex_numbers(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                *v = Value::String(format!("{u:#x}"));
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hex_numbers),
        // e.g. `pc: {value, source, evidence}`
        Value::Object(map) => {
            if let Some(x) = map.get_mut("value") {
                hex_numbers(x);
            }
            for (k, x) in map.iter_mut() {
                if k != "value" {
                    hexify(x);
                }
            }
        }
        _ => {}
    }
}

/// A number given as JSON number or as a string (`"0x2000_0000"`, `"1234"`).
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Num {
    Int(u64),
    Text(String),
}

impl Num {
    fn get(&self) -> Result<u64, String> {
        match self {
            Num::Int(v) => Ok(*v),
            Num::Text(s) => {
                let t = s.trim().replace('_', "");
                match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    Some(h) => u64::from_str_radix(h, 16),
                    None => t.parse(),
                }
                .map_err(|_| format!("not a number: {s}"))
            }
        }
    }
}

fn num(v: &Option<Num>) -> Result<Option<u64>, String> {
    v.as_ref().map(Num::get).transpose()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocateArgs {
    /// Point as an instruction index (state before that instruction).
    pub at: Option<Num>,
    /// Point as a trace time stamp: the first instruction at or after it.
    pub time: Option<Num>,
    /// Full detail: inline frames, entry/return points and PC provenance per frame (default false).
    pub detail: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegistersArgs {
    /// Point as an instruction index (state before that instruction).
    pub at: Option<Num>,
    /// Point as a trace time stamp: the first instruction at or after it.
    pub time: Option<Num>,
    /// Only these registers (e.g. ["r0", "sp", "pc"]); default all.
    pub names: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryArgs {
    /// Start address (number or "0x..." string). Give this or `var`.
    pub addr: Option<Num>,
    /// Variable expression instead of an address: `name`, `name[3]`, `name.member`.
    pub var: Option<String>,
    /// Bytes (default: the variable's size, or 16). At most 4096.
    pub len: Option<u64>,
    /// Point as an instruction index (state before that instruction).
    pub at: Option<Num>,
    /// Point as a trace time stamp: the first instruction at or after it.
    pub time: Option<Num>,
    /// read_memory: per-byte values instead of runs of bytes with the same source (default false).
    pub detail: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegisterHistoryArgs {
    /// Register name, e.g. "r0", "sp", "lr".
    pub name: String,
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// Most items to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VariableArgs {
    /// Fixed-address variable expression: `ticks`, `ring[3]`, `pool[2].len`, `func::static_var`.
    pub path: String,
    /// Point as an instruction index (state before that instruction).
    pub at: Option<Num>,
    /// Point as a trace time stamp: the first instruction at or after it.
    pub time: Option<Num>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListVariablesArgs {
    /// Only names containing this text.
    pub filter: Option<String>,
    /// Most items to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExceptionsArgs {
    /// Only this kind: a name ("SysTick", "SVCall", "HardFault", "IRQ3") or exception number.
    pub kind: Option<String>,
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// Most items to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecutionsArgs {
    /// A PC ("0x1f4"), a source line ("file.c:123") or a function name ("main").
    pub target: String,
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// Most items to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallTreeArgs {
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// Levels shown (default 4); deeper calls are counted, not listed.
    pub depth: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WritesArgs {
    /// Start address (number or "0x..." string). Give this or `var`.
    pub addr: Option<Num>,
    /// Variable expression instead of an address (its size is the length).
    pub var: Option<String>,
    /// Bytes (default: the variable's size, or 16).
    pub len: Option<u64>,
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// Most items to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindArgs {
    /// Condition, e.g. `var("ticks") == 5 && func == "depth"`, `pc == sym("HardFault_Handler")`,
    /// `line == "main.c:42" && reg("r0") != 0`, `mem32(0x20000000) > 3`. Operands: pc, line, func,
    /// file, reg("name"), var("path"), mem8/16/32/64(addr), sym("name"), numbers, "strings";
    /// operators == != < <= > >= && || ! and parentheses.
    pub condition: String,
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
    /// "forward" (first match at or after `from`, default) or "backward" (last match at or before `to`).
    pub direction: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiffArgs {
    /// First point (index).
    pub a: Num,
    /// Second point (index).
    pub b: Num,
    /// Only registers and the per-variable summary, no memory runs (default false).
    pub variables_only: Option<bool>,
    /// Most memory runs to return (default 20, at most 1000).
    pub limit: Option<usize>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookmarkArgs {
    /// Point as an instruction index (state before that instruction).
    pub at: Option<Num>,
    /// Point as a trace time stamp: the first instruction at or after it.
    pub time: Option<Num>,
    /// What is interesting here.
    pub note: String,
    /// Who adds it (default "ai").
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookmarkListArgs {
    /// First point of the range (default 0).
    pub from: Option<Num>,
    /// Last point of the range, inclusive (default: the end).
    pub to: Option<Num>,
}

#[derive(Clone)]
pub struct SimGdbMcp {
    engine: Arc<Engine>,
    annotations: Option<Arc<Mutex<Annotations>>>,
    tool_router: ToolRouter<Self>,
}

type Answer = Result<Value, String>;

fn e(err: EngineError) -> String {
    err.to_string()
}

fn limit(l: Option<usize>) -> usize {
    l.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

fn to_value<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap()
}

/// `function (file:line)`, with the inline chain if any.
fn brief(pc: Option<u64>, s: &Option<SourcePos>) -> String {
    match s {
        Some(s) => {
            let file = s.file.rsplit('/').next().unwrap_or(&s.file);
            let inline: Vec<&str> = s.inline.iter().map(|f| f.function.as_str()).collect();
            let inlined = if inline.len() > 1 { format!(" [inlined: {}]", inline.join(" < ")) } else { String::new() };
            format!("{} ({}:{}){}", s.function, file, s.line, inlined)
        }
        None => pc.map(|p| format!("{p:#x} (no source)")).unwrap_or_else(|| "unknown".into()),
    }
}

impl SimGdbMcp {
    pub fn new(engine: Arc<Engine>, annotations: Option<Annotations>) -> Self {
        SimGdbMcp {
            engine,
            annotations: annotations.map(|a| Arc::new(Mutex::new(a))),
            tool_router: Self::tool_router(),
        }
    }

    fn point(&self, at: &Option<Num>, time: &Option<Num>) -> Result<u64, String> {
        match (num(at)?, num(time)?) {
            (Some(i), _) => Ok(i),
            (None, Some(t)) => {
                self.engine.index_at_time(t).ok_or_else(|| format!("no instruction at or after time {t}"))
            }
            (None, None) => Err("give `at` (instruction index) or `time`".into()),
        }
    }

    fn range(&self, from: &Option<Num>, to: &Option<Num>) -> Result<Range, String> {
        self.engine.range(num(from)?, num(to)?).map_err(e)
    }

    fn location(&self, addr: &Option<Num>, var: &Option<String>, len: Option<u64>) -> Result<(u64, u64), String> {
        match (var, num(addr)?) {
            (Some(v), _) => {
                let p = self.engine.variable(v).map_err(e)?;
                Ok((p.addr, len.or(p.size).ok_or("variable size unknown: give `len`")?))
            }
            (None, Some(a)) => Ok((a, len.unwrap_or(16))),
            (None, None) => Err("give `addr` or `var`".into()),
        }
    }

    /// Run a tool body, log it, and turn the answer into a tool result.
    fn reply<A: std::fmt::Debug>(&self, tool: &str, args: &A, f: impl FnOnce() -> Answer) -> CallToolResult {
        let start = Instant::now();
        let out = f().map(|mut v| {
            hexify(&mut v);
            v
        });
        let ms = start.elapsed().as_secs_f64() * 1e3;
        if let Some(a) = &self.annotations {
            let summary = match &out {
                Ok(v) => v.to_string().chars().take(300).collect::<String>(),
                Err(m) => m.clone(),
            };
            if let Ok(db) = a.lock() {
                let _ = db.log_call(tool, &format!("{args:?}"), out.is_ok(), &summary, ms);
            }
        }
        match out {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&v).unwrap())]),
            Err(m) => CallToolResult::error(vec![ContentBlock::text(m)]),
        }
    }
}

#[tool_router]
impl SimGdbMcp {
    #[tool(description = "What this run is: instruction count, time unit and range, what the trace records \
        (capabilities), limitations, exception counts, variables and bookmarks. Call first.")]
    fn trace_info(&self, Parameters(a): Parameters<NoArgs>) -> CallToolResult {
        self.reply("trace_info", &a, || {
            let eng = &self.engine;
            let all = eng.range(None, None).map_err(e)?;
            let exc = eng.exception_list(all, None, 0, 0).map_err(e)?;
            let bookmarks = match &self.annotations {
                Some(a) => {
                    a.lock().map_err(|_| "annotations busy")?.bookmarks(0, u64::MAX).map_err(|x| x.to_string())?.len()
                }
                None => 0,
            };
            Ok(json!({
                "info": eng.info(),
                "points": format!("0..={} (point i = before instruction i; {} = after the last)", eng.end(), eng.end()),
                "exceptions": exc.by_exception,
                "variables": eng.variables().variables.len(),
                "bookmarks": bookmarks,
            }))
        })
    }

    #[tool(description = "Where execution is at a point: PC, function, source file:line, and the call stack \
        innermost first (built from the trace's calls, returns and exceptions, not stack memory). Brief by \
        default; `detail: true` adds inline frames, entry/return points and where each frame's PC came from.")]
    fn locate(&self, Parameters(a): Parameters<LocateArgs>) -> CallToolResult {
        self.reply("locate", &a, || {
            let loc = self.engine.locate(self.point(&a.at, &a.time)?).map_err(e)?;
            if a.detail.unwrap_or(false) {
                return Ok(to_value(loc));
            }
            let stack: Vec<Value> = loc
                .call_stack
                .iter()
                .map(|f| {
                    let mut v = json!({"kind": f.kind, "at": brief(f.pc, &f.source), "pc": f.pc});
                    if let Some(n) = f.exception {
                        v["exception"] = json!(n);
                    }
                    v
                })
                .collect();
            Ok(json!({"point": loc.point, "pc": loc.pc, "where": brief(loc.pc.value, &loc.source),
                      "call_stack": stack}))
        })
    }

    #[tool(description = "Register values at a point, each with source and evidence. Unknown registers are \
        reported as unknown (the trace has not shown them yet).")]
    fn registers(&self, Parameters(a): Parameters<RegistersArgs>) -> CallToolResult {
        self.reply("registers", &a, || {
            let i = self.point(&a.at, &a.time)?;
            let mut regs = self.engine.registers(i).map_err(e)?;
            if let Some(names) = &a.names {
                let want: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
                regs.retain(|r| want.iter().any(|w| w == r.name));
            }
            let regs: Vec<Value> = regs
                .into_iter()
                .map(|r| {
                    let mut v = to_value(&r);
                    v["hex"] = json!(r.value.value.map(|x| format!("{x:#x}")));
                    v
                })
                .collect();
            Ok(json!({"point": self.engine.point(i).map_err(e)?, "registers": regs}))
        })
    }

    #[tool(description = "Register writes in a range for one register, in trace order: the value after each \
        write, the writing instruction and the trace line. Use it to follow how a register evolved.")]
    fn register_history(&self, Parameters(a): Parameters<RegisterHistoryArgs>) -> CallToolResult {
        self.reply("register_history", &a, || {
            let r = self.range(&a.from, &a.to)?;
            let page = self.engine.register_history(&a.name, r, limit(a.limit), a.cursor.unwrap_or(0)).map_err(e)?;
            let mut v = to_value(page);
            // Register values read better in hex too.
            if let Some(items) = v["items"].as_array_mut() {
                for w in items {
                    if let Some(x) = w["value"].as_u64() {
                        w["hex"] = json!(format!("{x:#x}"));
                    }
                }
            }
            Ok(v)
        })
    }

    #[tool(description = "Memory bytes at a point (by address or variable), as hex with ?? for unknown bytes, \
        plus where they came from (runs of bytes with the same source and evidence; `detail` for per byte).")]
    fn read_memory(&self, Parameters(a): Parameters<MemoryArgs>) -> CallToolResult {
        self.reply("read_memory", &a, || {
            let i = self.point(&a.at, &a.time)?;
            let (addr, len) = self.location(&a.addr, &a.var, a.len)?;
            if len > 4096 {
                return Err("at most 4096 bytes per call".into());
            }
            let bytes = self.engine.memory(addr, len, i).map_err(e)?;
            let hex: String =
                bytes.iter().map(|b| b.value.map(|v| format!("{v:02x}")).unwrap_or_else(|| "??".into())).collect();
            let mut out = json!({"point": self.engine.point(i).map_err(e)?, "addr": addr, "len": len, "hex": hex,
                                 "variable": self.engine.variable_at(addr)});
            if a.detail.unwrap_or(false) {
                out["bytes"] = to_value(&bytes);
            } else {
                // Runs of bytes with the same provenance.
                let mut runs: Vec<Value> = Vec::new();
                let mut start = 0;
                for k in 1..=bytes.len() {
                    let same = k < bytes.len()
                        && bytes[k].source == bytes[start].source
                        && bytes[k].evidence == bytes[start].evidence
                        && bytes[k].conflicts.is_empty()
                        && bytes[start].conflicts.is_empty();
                    if !same {
                        let b = &bytes[start];
                        let mut run = json!({"offset": start, "len": k - start, "source": b.source});
                        if !b.evidence.is_empty() {
                            run["evidence"] = to_value(&b.evidence);
                        }
                        if !b.conflicts.is_empty() {
                            run["conflicts"] = to_value(&b.conflicts);
                        }
                        runs.push(run);
                        start = k;
                    }
                }
                out["sources"] = Value::Array(runs);
            }
            Ok(out)
        })
    }

    #[tool(description = "A fixed-address variable's value at a point: address, type, raw bytes and decoded \
        value (globals, file statics, function statics; `name`, `name[i]`, `name.member`). `decoded` is null \
        when not fully known. Local variables and pointer dereference are not available here (use a debugger).")]
    fn read_variable(&self, Parameters(a): Parameters<VariableArgs>) -> CallToolResult {
        self.reply("read_variable", &a, || {
            Ok(to_value(self.engine.read_variable(&a.path, self.point(&a.at, &a.time)?).map_err(e)?))
        })
    }

    #[tool(description = "List fixed-address variables (globals and statics) with address, size and C type; \
        filter by name.")]
    fn list_variables(&self, Parameters(a): Parameters<ListVariablesArgs>) -> CallToolResult {
        self.reply("list_variables", &a, || {
            let v = self.engine.variables();
            let all: Vec<Value> = v
                .variables
                .iter()
                .filter(|x| a.filter.as_deref().is_none_or(|f| x.name.contains(f)))
                .map(|x| json!({"name": x.name, "addr": x.addr, "size": x.size, "type": v.name(x.ty)}))
                .collect();
            let start = (a.cursor.unwrap_or(0) as usize).min(all.len());
            let end = (start + limit(a.limit)).min(all.len());
            Ok(json!({"total": all.len(), "items": &all[start..end],
                      "next_cursor": (end < all.len()).then_some(end)}))
        })
    }

    #[tool(description = "Where a PC, source line (\"file.c:123\") or function executed in a range. \
        `instructions`: executions of that code itself (a function's own instructions, not its callees). \
        `entries`: points where execution entered it (for a function: its calls; for a line: entries from \
        another line, not resumptions after an interrupt). Answers \"how often\" and \"when\".")]
    fn find_executions(&self, Parameters(a): Parameters<ExecutionsArgs>) -> CallToolResult {
        self.reply("find_executions", &a, || {
            let r = self.range(&a.from, &a.to)?;
            Ok(to_value(
                self.engine
                    .find_executions(&Target::parse(&a.target), r, limit(a.limit), a.cursor.unwrap_or(0))
                    .map_err(e)?,
            ))
        })
    }

    #[tool(description = "Calls and exceptions entered in a range as a tree of call paths, plus per-function \
        and per-handler totals. `inclusive_instructions` counts everything between entry and return (callees \
        and interrupts included) for calls that returned; `unreturned_instructions` covers calls still open at \
        the end of the range. Built from the trace flow, not stack memory.")]
    fn call_tree(&self, Parameters(a): Parameters<CallTreeArgs>) -> CallToolResult {
        self.reply("call_tree", &a, || {
            let r = self.range(&a.from, &a.to)?;
            Ok(to_value(self.engine.call_tree(r, a.depth.unwrap_or(4)).map_err(e)?))
        })
    }

    #[tool(description = "Exceptions and interrupts entered in a range (optionally one kind): counts by kind, \
        then entries with handler, `entered_at` (first handler instruction), `taken_after` (the instruction \
        before: for SVC or a fault the one that raised it), return point, nesting and the interrupted PC.")]
    fn exceptions(&self, Parameters(a): Parameters<ExceptionsArgs>) -> CallToolResult {
        self.reply("exceptions", &a, || {
            let r = self.range(&a.from, &a.to)?;
            Ok(to_value(
                self.engine.exception_list(r, a.kind.as_deref(), limit(a.limit), a.cursor.unwrap_or(0)).map_err(e)?,
            ))
        })
    }

    #[tool(description = "Which trace record last wrote each byte (by address or variable) as seen at a point: \
        the writing instruction (`written_by`), its PC and function, the value and the trace line.")]
    fn last_write(&self, Parameters(a): Parameters<MemoryArgs>) -> CallToolResult {
        self.reply("last_write", &a, || {
            let i = self.point(&a.at, &a.time)?;
            let (addr, len) = self.location(&a.addr, &a.var, a.len)?;
            Ok(to_value(self.engine.last_write(addr, len, i).map_err(e)?))
        })
    }

    #[tool(description = "All writes to an address range (or variable) by instructions in a range, in trace \
        order: writing instruction, PC, function, value. A store that also covers bytes outside the range \
        (e.g. a wide stack store over a variable) carries `inside`: just the bytes within the range.")]
    fn writes_to(&self, Parameters(a): Parameters<WritesArgs>) -> CallToolResult {
        self.reply("writes_to", &a, || {
            let r = self.range(&a.from, &a.to)?;
            let (addr, len) = self.location(&a.addr, &a.var, a.len)?;
            Ok(to_value(self.engine.writes_to(addr, len, r, limit(a.limit), a.cursor.unwrap_or(0)).map_err(e)?))
        })
    }

    #[tool(description = "The first point (or with direction \"backward\" the last point) in a range where a \
        condition holds, with the source position and input values there. If a value changed to make it \
        true, the writing instruction is `found.index - 1`. Comparisons on values the trace does not show are \
        undecided, never a match: `evaluated` = points checked (only where an input can change), `undecided` \
        = of those, how many could not be decided, `first_undecided` = the first such point in search order \
        (if it comes before the match, the true answer could be there).")]
    fn find_first(&self, Parameters(a): Parameters<FindArgs>) -> CallToolResult {
        self.reply("find_first", &a, || {
            let r = self.range(&a.from, &a.to)?;
            let d = match a.direction.as_deref().unwrap_or("forward") {
                "forward" => Direction::Forward,
                "backward" => Direction::Backward,
                other => return Err(format!("direction is \"forward\" or \"backward\", not {other:?}")),
            };
            Ok(to_value(self.engine.find_first(&a.condition, r, d).map_err(e)?))
        })
    }

    #[tool(description = "What differs between two points: registers; variables with changed bytes (which \
        elements or members changed, and old/new values for scalars); memory runs written in between, each \
        within one variable. `variables_only` drops the memory runs.")]
    fn diff_state(&self, Parameters(a): Parameters<DiffArgs>) -> CallToolResult {
        self.reply("diff_state", &a, || {
            let mut d =
                to_value(self.engine.diff(a.a.get()?, a.b.get()?, limit(a.limit), a.cursor.unwrap_or(0)).map_err(e)?);
            if a.variables_only.unwrap_or(false) {
                d.as_object_mut().unwrap().remove("memory");
            }
            Ok(d)
        })
    }

    #[tool(description = "Bookmark a point with a note, shared with people using GDB or VS Code.")]
    fn bookmark_add(&self, Parameters(a): Parameters<BookmarkArgs>) -> CallToolResult {
        self.reply("bookmark_add", &a, || {
            let i = self.point(&a.at, &a.time)?;
            self.engine.point(i).map_err(e)?;
            let db = self.annotations.as_ref().ok_or("no annotations database (read-only index directory?)")?;
            let b = db
                .lock()
                .map_err(|_| "annotations busy")?
                .add_bookmark(i, &a.note, a.author.as_deref().unwrap_or("ai"))
                .map_err(|x| x.to_string())?;
            Ok(to_value(b))
        })
    }

    #[tool(description = "Bookmarks (points people or AI marked with a note) in a range, in point order.")]
    fn bookmark_list(&self, Parameters(a): Parameters<BookmarkListArgs>) -> CallToolResult {
        self.reply("bookmark_list", &a, || {
            let r = self.range(&a.from, &a.to)?;
            let db = self.annotations.as_ref().ok_or("no annotations database (read-only index directory?)")?;
            let list = db.lock().map_err(|_| "annotations busy")?.bookmarks(r.from, r.to).map_err(|x| x.to_string())?;
            Ok(json!({"bookmarks": list}))
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SimGdbMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("simgdb", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve over stdin/stdout until the client disconnects.
pub async fn serve_stdio(server: SimGdbMcp) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = server.serve((tokio::io::stdin(), tokio::io::stdout())).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_become_hex() {
        let mut v = json!({"pc": {"value": 576, "source": "trace"}, "addr": 16, "pcs": [1, 2], "index": 1080,
                           "writes": [{"pc": 292, "value": 700, "written_by": 2580}]});
        hexify(&mut v);
        assert_eq!(
            v,
            json!({"pc": {"value": "0x240", "source": "trace"}, "addr": "0x10", "pcs": ["0x1", "0x2"],
                   "index": 1080, "writes": [{"pc": "0x124", "value": 700, "written_by": 2580}]})
        );
    }
}
