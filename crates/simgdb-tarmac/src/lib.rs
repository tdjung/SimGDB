//! Tarmac trace parser.
//!
//! Parses one line at a time into a [`Line`]: an optional time stamp, unit and
//! core, and one [`Record`]. The parser keeps only the little state Tarmac
//! needs across lines: the last time stamp (lines without one inherit it) and
//! whether the previous line was an `LD`/`ST` diagram that the next line may
//! continue without repeating the record type.
//!
//! Supported forms (see `tests/fixtures` for real-world samples):
//!
//! - Instructions: `IT`/`IS`/`IF` (`(seq) pc opcode state mode : disasm`,
//!   `(pc:seq) pc opcode T16 disasm`, `(pc) opcode state ...`), `ES`
//!   (`(pc:opcode) state mode: [CCFAIL] disasm`), fetch failures (`--------`).
//! - Registers: `R name value`, `R name (MODE) value`, `R r13 value (MSP)`,
//!   `x`/`X` digits for unknown bits, `_` separators.
//! - Memory: `MRn`/`MWn` with optional physical address and attributes,
//!   Cortex-M RTL `MNW4___D` (bytes in memory order), `R04`/`W08`, `LD`/`ST`
//!   16-byte diagrams with continuation lines, `x` for unknown data.
//! - Exceptions: `E pc number CoreEvent_...`, `ES EXC [n] name`, `EXC ...`.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use flate2::read::MultiGzDecoder;
use simgdb_model::TimeUnit;

/// Instruction record kind as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    It,
    Is,
    If,
    Es,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    pub event: Event,
    pub pc: u64,
    /// Encoding as printed (Thumb-2: first halfword in the high bits).
    /// `None` when the fetch failed (`--------`).
    pub opcode: Option<u32>,
    /// Encoding width in bytes (2 or 4), 0 if unknown.
    pub width: u8,
    /// Condition failed: `IS`, or `CCFAIL` in an `ES` line.
    pub failed: bool,
    /// Sequence number printed in parentheses, if any.
    pub seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    pub name: String,
    /// Bank annotation after the value, e.g. `MSP` in `R r13 ... (MSP)`.
    pub bank: Option<String>,
    /// Low 64 bits of the value (wider registers keep their low bits).
    pub value: u64,
    /// Bit set = known (from `x`/`X` digits).
    pub known: u64,
    /// Width written in the trace, in bits.
    pub bits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemAccess {
    pub write: bool,
    pub addr: u64,
    /// 1..=8 bytes.
    pub size: u8,
    /// Little-endian logical value: byte k of the value is at `addr + k`.
    pub value: u64,
    /// Bit k set = byte k known.
    pub known: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Instruction(Instruction),
    Register(Register),
    Memory(Vec<MemAccess>),
    Exception {
        number: Option<u32>,
        name: String,
        /// Handler address when the record gives one (`E` records).
        target: Option<u64>,
    },
    /// `Tarmac Text Rev ...`
    Header,
    Blank,
    /// Known record types with no effect on state (branch hints, debug and
    /// simulator events, instruction-side or peripheral-bus fetches).
    Ignored(String),
    /// A record type this parser does not know. Reported, never silently
    /// dropped.
    Unrecognised(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Time stamp of this line, or the previous one when it has none.
    pub time: Option<u64>,
    pub unit: Option<TimeUnit>,
    /// `cpuN` prefix.
    pub core: Option<u32>,
    pub record: Record,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ParseError {
    pub message: String,
}

fn err<T>(message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { message: message.into() })
}

/// A whitespace token with its byte offset in the line.
#[derive(Clone, Copy, Debug)]
struct Tok<'a> {
    s: &'a str,
    pos: usize,
}

fn tokens(line: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i > start {
            out.push(Tok { s: &line[start..i], pos: start });
        }
    }
    out
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(s: &str) -> Option<u64> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.is_empty() || s.len() > 16 || !is_hex(s) {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

/// Address token: `hex`, `hex:physical`, `hex_NS`.
fn address(s: &str) -> Option<u64> {
    let virt = s.split(':').next()?;
    let virt = virt.split('_').next()?;
    hex(virt)
}

/// Hex value with `_` separators and `x`/`X` unknown digits:
/// (low 64 bits, known mask, width in bits).
fn value_with_unknowns(s: &str) -> Option<(u64, u64, u32)> {
    let digits: Vec<u8> = s.bytes().filter(|&b| b != b'_').collect();
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_hexdigit() || *b == b'x' || *b == b'X') {
        return None;
    }
    let bits = digits.len() as u32 * 4;
    let (mut value, mut known) = (0u64, 0u64);
    for (k, &d) in digits.iter().rev().enumerate().take(16) {
        if d != b'x' && d != b'X' {
            let nibble = (d as char).to_digit(16).unwrap() as u64;
            value |= nibble << (4 * k);
            known |= 0xf << (4 * k);
        }
    }
    Some((value, known, bits))
}

fn width_from_state(state: &str, opcode_digits: usize) -> u8 {
    match state {
        "T16" => 2,
        "T32" | "A" | "O" => 4,
        _ => match opcode_digits {
            4 => 2,
            8 => 4,
            _ => 0,
        },
    }
}

fn is_state(s: &str) -> bool {
    matches!(s, "T16" | "T32" | "T" | "A" | "O")
}

/// Opcode token: 4 or 8 hex digits, or a fetch failure (`----`, `....`).
fn opcode(s: &str) -> Option<Option<u32>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if (s.len() == 4 || s.len() == 8) && is_hex(s) {
        return Some(Some(u32::from_str_radix(s, 16).ok()?));
    }
    if s.len() >= 4 && s.bytes().all(|b| b == b'-' || b == b'.') {
        return Some(None);
    }
    None
}

#[derive(Default)]
pub struct Parser {
    time: Option<u64>,
    unit: Option<TimeUnit>,
    /// Column where the address of the last LD/ST diagram started, and its type.
    continuation: Option<(usize, bool)>,
}

const IGNORED: &[&str] = &["BR", "CADI", "P", "CACHE", "TTW", "INFO_EXCEPTION_REASON", "SIGNAL", "INFO"];

impl Parser {
    pub fn new() -> Self {
        Parser::default()
    }

    pub fn parse(&mut self, raw: &str) -> Result<Line, ParseError> {
        let line = raw.trim_end_matches(['\r', '\n']);
        let toks = tokens(line);
        let continuation = self.continuation.take();
        if toks.is_empty() {
            return Ok(self.line(None, Record::Blank));
        }
        // An LD/ST continuation line starts at the column of the previous
        // address and has no type token (its address may look decimal).
        if let Some((column, write)) = continuation {
            if toks[0].pos == column && hex(toks[0].s).is_some() {
                let record = self.diagram(&toks, write)?;
                return Ok(self.line(None, record));
            }
        }
        let mut i = 0;
        let mut unit = None;
        if let Some(t) = toks[0].s.parse::<u64>().ok().or_else(|| float_time(toks[0].s)) {
            self.time = Some(t);
            i = 1;
            if let Some(u) = toks.get(1).and_then(|t| TimeUnit::from_token(t.s)) {
                unit = Some(u);
                i = 2;
            }
        } else if let Some((t, u)) = fused_time(toks[0].s) {
            self.time = Some(t);
            unit = Some(u);
            i = 1;
        }
        if let Some(u) = &unit {
            self.unit = Some(u.clone());
        }
        let mut core = None;
        if let Some(t) = toks.get(i) {
            if let Some(n) = t.s.strip_prefix("cpu") {
                core = n.parse().ok();
                i += 1;
            }
        }
        let Some(kind) = toks.get(i) else {
            return Ok(self.line(core, Record::Blank));
        };
        let rest = &toks[i + 1..];
        let record = match kind.s {
            "IT" | "IS" | "IF" => self.it(kind.s, rest)?,
            "ES" => self.es(line, rest)?,
            "R" => register(rest)?,
            "LD" | "ST" => {
                let write = kind.s == "ST";
                let Some(first) = rest.first() else { return err("LD/ST without address") };
                self.continuation = Some((first.pos, write));
                self.diagram(rest, write)?
            }
            "E" => exception_e(rest),
            "EXC" => exception_exc(rest),
            "Tarmac" => Record::Header,
            s if IGNORED.contains(&s) => Record::Ignored(s.to_string()),
            s if is_memory_type(s) => memory(s, rest)?,
            s => Record::Unrecognised(s.to_string()),
        };
        let mut out = self.line(core, record);
        if unit.is_none() {
            out.unit = None;
        } else {
            out.unit = unit;
        }
        Ok(out)
    }

    fn line(&self, core: Option<u32>, record: Record) -> Line {
        Line { time: self.time, unit: None, core, record }
    }

    /// Unit of the most recent time stamp that had one.
    pub fn unit(&self) -> Option<&TimeUnit> {
        self.unit.as_ref()
    }

    fn it(&mut self, kind: &str, rest: &[Tok<'_>]) -> Result<Record, ParseError> {
        let event = match kind {
            "IT" => Event::It,
            "IS" => Event::Is,
            _ => Event::If,
        };
        let mut j = 0;
        let mut paren: Option<String> = None;
        if let Some(t) = rest.first() {
            if t.s.starts_with('(') {
                let mut text = String::new();
                while j < rest.len() {
                    text.push_str(rest[j].s);
                    j += 1;
                    if text.ends_with(')') {
                        break;
                    }
                }
                paren = Some(text.trim_start_matches('(').trim_end_matches(')').to_string());
            }
        }
        // Form A: [paren] pc opcode state ...
        let explicit = match (rest.get(j), rest.get(j + 1), rest.get(j + 2)) {
            (Some(p), Some(o), Some(s)) if address(p.s).is_some() && opcode(o.s).is_some() && is_state(s.s) => {
                Some((address(p.s).unwrap(), opcode(o.s).unwrap(), o.s.len(), s.s, j + 3))
            }
            _ => None,
        };
        let (pc, op, digits, state, _) = match explicit {
            Some(found) => found,
            None => {
                // Form B: (pc[:...]) opcode state ...
                let Some(p) = paren.as_deref().and_then(|p| address(p.split(',').next().unwrap_or(p))) else {
                    return err("IT: expected an address and opcode");
                };
                match (rest.get(j), rest.get(j + 1)) {
                    (Some(o), Some(s)) if opcode(o.s).is_some() && is_state(s.s) => {
                        (p, opcode(o.s).unwrap(), o.s.len(), s.s, j + 2)
                    }
                    _ => return err("IT: expected opcode and instruction set after the address"),
                }
            }
        };
        let seq = paren.as_deref().and_then(|p| {
            // (seq) or (pc:seq)
            let last = p.rsplit(':').next()?;
            if p.contains(':') || explicit.is_some() { hex(last).or_else(|| last.parse().ok()) } else { None }
        });
        Ok(Record::Instruction(Instruction {
            event,
            pc,
            opcode: op,
            width: width_from_state(state, digits),
            failed: event == Event::Is,
            seq,
        }))
    }

    fn es(&mut self, line: &str, rest: &[Tok<'_>]) -> Result<Record, ParseError> {
        let Some(first) = rest.first() else { return err("ES without body") };
        if first.s == "EXC" {
            return Ok(exception_exc(&rest[1..]));
        }
        if !first.s.starts_with('(') {
            // "ES Reset" and similar exception announcements.
            return Ok(Record::Exception { number: None, name: first.s.to_string(), target: None });
        }
        let body = &line[first.pos..];
        let close = body.find(')').ok_or_else(|| ParseError { message: "ES: unclosed '('".into() })?;
        let inner = &body[1..close];
        let (pc, op) =
            inner.split_once(':').ok_or_else(|| ParseError { message: "ES: expected (pc:opcode)".into() })?;
        let pc = hex(pc.trim()).ok_or_else(|| ParseError { message: "ES: bad pc".into() })?;
        let op_text = op.trim();
        let op = opcode(op_text).ok_or_else(|| ParseError { message: "ES: bad opcode".into() })?;
        let after = tokens(&body[close + 1..]);
        let state = after.first().map(|t| t.s).unwrap_or("");
        let failed = after.iter().any(|t| t.s == "CCFAIL");
        Ok(Record::Instruction(Instruction {
            event: Event::Es,
            pc,
            opcode: op,
            width: if op.is_some() { width_from_state(state, op_text.len()) } else { 0 },
            failed,
            seq: None,
        }))
    }

    fn diagram(&mut self, rest: &[Tok<'_>], write: bool) -> Result<Record, ParseError> {
        let Some(base) = rest.first().and_then(|t| hex(t.s)) else { return err("LD/ST: expected address") };
        self.continuation = Some((rest[0].pos, write));
        if base.checked_add(16).is_none() {
            return err("LD/ST: address out of range");
        }
        // 16 cells, most significant (highest address) first.
        let mut cells: Vec<Option<Option<u8>>> = Vec::with_capacity(16);
        for t in &rest[1..] {
            if cells.len() >= 16 {
                break;
            }
            if t.s.len() % 2 != 0 || !t.s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b'#') {
                return err("LD/ST: expected data bytes");
            }
            for k in (0..t.s.len()).step_by(2) {
                let cell = &t.s[k..k + 2];
                cells.push(match cell {
                    ".." => None,
                    "##" => Some(None),
                    _ => Some(Some(
                        u8::from_str_radix(cell, 16).map_err(|_| ParseError { message: "LD/ST: bad byte".into() })?,
                    )),
                });
            }
        }
        if cells.len() != 16 {
            return err("LD/ST: expected 16 data bytes");
        }
        // Group accessed bytes into contiguous runs of at most 8.
        let mut out = Vec::new();
        let mut off = 0usize; // offset from base, ascending
        while off < 16 {
            let Some(first) = cells[15 - off] else {
                off += 1;
                continue;
            };
            let mut acc = MemAccess { write, addr: base + off as u64, size: 0, value: 0, known: 0 };
            let mut cur = Some(first);
            while let Some(byte) = cur {
                let k = acc.size as usize;
                if let Some(b) = byte {
                    acc.value |= (b as u64) << (8 * k);
                    acc.known |= 1 << k;
                }
                acc.size += 1;
                off += 1;
                cur = if off < 16 && acc.size < 8 { cells[15 - off] } else { None };
            }
            out.push(acc);
        }
        Ok(Record::Memory(out))
    }
}

fn float_time(s: &str) -> Option<u64> {
    if s.contains('.') && s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        s.split('.').next()?.parse().ok()
    } else {
        None
    }
}

/// `271ns`: digits fused with a unit.
fn fused_time(s: &str) -> Option<(u64, TimeUnit)> {
    let end = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    if end == 0 {
        return None;
    }
    let unit = TimeUnit::from_token(&s[end..])?;
    let value = s[..end].parse().ok().or_else(|| float_time(&s[..end]))?;
    Some((value, unit))
}

fn register(rest: &[Tok<'_>]) -> Result<Record, ParseError> {
    let Some(name) = rest.first() else { return err("R without register name") };
    let mut j = 1;
    // Annotation before the value: a mode (`R R1 (USR) 00000800`) or a
    // stack bank (`R r13 (PSP) 20000800`). Kept; the ISA decides what it means.
    let mut before = None;
    if let Some(t) = rest.get(j).filter(|t| t.s.starts_with('(')) {
        before = Some(t.s.trim_start_matches('(').trim_end_matches(')').to_string());
        j += 1;
    }
    let mut name = name.s.to_string();
    let mut parsed = rest.get(j).and_then(|t| value_with_unknowns(strip_physical(t.s)));
    // Two-word names such as "DC CISW": the second word is not a value.
    if parsed.is_none() && rest.get(j + 1).is_some() {
        if rest.get(j).is_some_and(|t| t.s.bytes().all(|b| b == b'-')) {
            return Ok(Record::Ignored(format!("R {name} (no value)")));
        }
        name = format!("{} {}", name, rest[j].s);
        j += 1;
        parsed = rest.get(j).and_then(|t| value_with_unknowns(strip_physical(t.s)));
    }
    let Some((mut value, mut known, mut bits)) = parsed else {
        if rest.get(j).is_some_and(|t| t.s.bytes().all(|b| b == b'-')) {
            return Ok(Record::Ignored(format!("R {name} (no value)")));
        }
        return err(format!("R {name}: expected a hex value"));
    };
    j += 1;
    // Values split by spaces (R X0 01234567 89abcdef, R Q0 a b c d).
    while let Some((v, k, b)) = rest.get(j).and_then(|t| if t.s.len() == 8 { value_with_unknowns(t.s) } else { None }) {
        value = if b >= 64 { v } else { (value << b) | v };
        known = if b >= 64 { k } else { (known << b) | k };
        bits += b;
        j += 1;
    }
    let bank = rest
        .get(j)
        .and_then(|t| t.s.strip_prefix('(').and_then(|s| s.strip_suffix(')')).map(|s| s.to_string()))
        .or(before);
    Ok(Record::Register(Register { name, bank, value, known, bits }))
}

/// `01230123:23452345` in some FPCR records: keep the part after ':'.
fn strip_physical(s: &str) -> &str {
    s.rsplit(':').next().unwrap_or(s)
}

fn is_memory_type(s: &str) -> bool {
    let b = s.as_bytes();
    (b.first() == Some(&b'M')
        && b.len() >= 3
        && b[1..].iter().any(|c| *c == b'R' || *c == b'W')
        && b.iter().any(|c| c.is_ascii_digit()))
        || matches!(s, "R01" | "R02" | "R04" | "R08" | "W01" | "W02" | "W04" | "W08")
}

fn memory(kind: &str, rest: &[Tok<'_>]) -> Result<Record, ParseError> {
    let b = kind.as_bytes();
    // Read/write letter and size digits.
    let start = if b[0] == b'M' { 1 } else { 0 };
    let rw = b[start..].iter().position(|c| *c == b'R' || *c == b'W').map(|p| p + start);
    let Some(rw) = rw else { return Ok(Record::Unrecognised(kind.to_string())) };
    let write = b[rw] == b'W';
    let digits: String = kind[rw + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    let size: u32 = digits.parse().map_err(|_| ParseError { message: format!("{kind}: no size") })?;
    // Cortex-M RTL 8-character types end in D (data), I (instruction side)
    // or A (peripheral bus); only data accesses are memory state.
    let mut memory_order = false;
    if kind.len() == 8 {
        match b[7] {
            b'I' | b'A' => return Ok(Record::Ignored(kind.to_string())),
            b'D' => memory_order = true,
            _ => {}
        }
    }
    if kind.contains('C') && rest.iter().any(|t| t.s == "TRANSFER") {
        return Ok(Record::Ignored(kind.to_string()));
    }
    let mut j = 0;
    if rest.get(j).is_some_and(|t| t.s == "X") {
        j += 1;
    }
    if rest.get(j).is_some_and(|t| t.s.starts_with('(') && t.s != "(ABORTED)") {
        while j < rest.len() && !rest[j].s.ends_with(')') {
            j += 1;
        }
        j += 1;
    }
    let Some(addr) = rest.get(j).and_then(|t| address(t.s)) else { return err(format!("{kind}: expected address")) };
    if rest.iter().any(|t| t.s == "(ABORTED)") {
        return Ok(Record::Ignored("ABORTED".to_string()));
    }
    // Data: the last token (two for 16-byte accesses), after any attributes.
    let data: Vec<&str> = rest[j + 1..].iter().map(|t| t.s).collect();
    let Some(last) = data.last() else { return err(format!("{kind}: expected data")) };
    let parse = |s: &str| value_with_unknowns(s).ok_or_else(|| ParseError { message: format!("{kind}: bad data {s}") });
    let one = |addr: u64, size: u32, text: &str| -> Result<MemAccess, ParseError> {
        let (mut value, known, _) = parse(text)?;
        let mut known_bytes = 0u8;
        for k in 0..size {
            if (known >> (8 * k)) & 0xff == 0xff {
                known_bytes |= 1 << k;
            }
        }
        if memory_order {
            let mut v = 0u64;
            for k in 0..size {
                v |= ((value >> (8 * k)) & 0xff) << (8 * (size - 1 - k));
            }
            value = v;
            let mut kb = 0u8;
            for k in 0..size {
                if known_bytes & (1 << k) != 0 {
                    kb |= 1 << (size - 1 - k);
                }
            }
            known_bytes = kb;
        }
        Ok(MemAccess { write, addr, size: size as u8, value: value & mask(size), known: known_bytes })
    };
    match size {
        1..=8 => Ok(Record::Memory(vec![one(addr, size, last)?])),
        16 => {
            if data.len() < 2 {
                return err(format!("{kind}: expected two data words"));
            }
            let high = data[data.len() - 2];
            let next =
                addr.checked_add(8).ok_or_else(|| ParseError { message: format!("{kind}: address out of range") })?;
            Ok(Record::Memory(vec![one(addr, 8, last)?, one(next, 8, high)?]))
        }
        _ => err(format!("{kind}: unsupported size {size}")),
    }
}

fn mask(size: u32) -> u64 {
    if size >= 8 { u64::MAX } else { (1u64 << (8 * size)) - 1 }
}

fn exception_e(rest: &[Tok<'_>]) -> Record {
    match rest.first() {
        Some(t) if t.s.starts_with("DebugEvent_") => Record::Ignored(format!("E {}", t.s)),
        _ => {
            let target = rest.first().and_then(|t| hex(t.s));
            let number = rest.get(1).and_then(|t| hex(t.s)).map(|n| n as u32);
            let name = rest.iter().map(|t| t.s).find(|s| s.starts_with("CoreEvent_")).unwrap_or("E").to_string();
            Record::Exception { number, name, target }
        }
    }
}

fn exception_exc(rest: &[Tok<'_>]) -> Record {
    let mut number = None;
    let mut j = 0;
    if let Some(t) = rest.first() {
        if let Some(inner) = t.s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            number = hex(inner).or_else(|| inner.parse().ok()).map(|n| n as u32);
            j = 1;
        }
    }
    let name = rest[j..].iter().map(|t| t.s).collect::<Vec<_>>().join(" ");
    Record::Exception { number, name, target: None }
}

/// Open a trace file, decompressing gzip (by magic bytes, not extension).
pub fn open(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    drop(file);
    let file = std::fs::File::open(path)?;
    if n == 2 && magic == [0x1f, 0x8b] {
        Ok(Box::new(BufReader::with_capacity(1 << 20, MultiGzDecoder::new(file))))
    } else {
        Ok(Box::new(BufReader::with_capacity(1 << 20, file)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Record {
        Parser::new().parse(line).unwrap().record
    }

    fn insn(line: &str) -> Instruction {
        match one(line) {
            Record::Instruction(i) => i,
            r => panic!("{line}: {r:?}"),
        }
    }

    fn mem(line: &str) -> Vec<MemAccess> {
        match one(line) {
            Record::Memory(m) => m,
            r => panic!("{line}: {r:?}"),
        }
    }

    #[test]
    fn instruction_forms() {
        let i = insn("1000 ps IT (12) 00000140 2001 T thread : MOVS r0,#1");
        assert_eq!((i.pc, i.opcode, i.width, i.failed, i.seq), (0x140, Some(0x2001), 2, false, Some(0x12)));
        let i = insn("      3041 cyc IT (00022a7c:00000006) 00022a7c     48f6 T16 LDR      r0,[pc,#984]");
        assert_eq!((i.pc, i.width, i.seq), (0x22a7c, 2, Some(6)));
        let i = insn("1009 tic ES (0000006e:d20a) T thrd: CCFAIL BCS.N 86 <Reset_Handler+0x42>");
        assert_eq!((i.pc, i.opcode, i.failed), (0x6e, Some(0xd20a), true));
        let i = insn("1016 tic ES (0000007e:f8431b04) T thrd: STR.W r1, [r3], #4");
        assert_eq!(i.width, 4);
        let i = insn("100900 ps IS (0000006e:00000008) 0000006e d20a T16 BCS.N 86");
        assert!(i.failed);
        let i = insn("6000000 cs IT (00000000004d6eb8) 54fffea1 O  ---_- :        b.ne	0x4d6e8c");
        assert_eq!(i.pc, 0x4d6eb8);
        let i = insn("11519 tic ES (10000b58:--------) T thrd_s:");
        assert_eq!(i.opcode, None);
    }

    #[test]
    fn registers() {
        let Record::Register(r) = one("271ns R r13 20001fff (MSP)") else { panic!() };
        assert_eq!((r.name.as_str(), r.bank.as_deref(), r.value), ("r13", Some("MSP"), 0x2000_1fff));
        let Record::Register(r) = one("         10 ns R psr xx0xxXxx") else { panic!() };
        assert_eq!((r.value, r.known), (0, 0x00f0_0000));
        let Record::Register(r) = one("                    R R1 (USR) 00000800") else { panic!() };
        assert_eq!((r.name.as_str(), r.value, r.bank.as_deref()), ("R1", 0x800, Some("USR")));
        let Record::Register(r) = one("2 ps R r13 (PSP) 20000800") else { panic!() };
        assert_eq!((r.value, r.bank.as_deref()), (0x2000_0800, Some("PSP")));
    }

    #[test]
    fn memory_forms() {
        assert_eq!(mem("3039 cyc MNW4___D 2002fa00 efbeefbe")[0].value, 0xbeefbeef);
        assert_eq!(one("3037 cyc MNR4O__I 00022ae4 f7ffffcb"), Record::Ignored("MNR4O__I".into()));
        let m = mem("1 clk MR8 0000c7c8:00000000c7c8 00000000_0000ca68");
        assert_eq!((m[0].addr, m[0].size, m[0].value, m[0].write), (0xc7c8, 8, 0xca68, false));
        let m = mem("     109570 ns MSW4___D 201fffec xxxxxxxx");
        assert_eq!(m[0].known, 0);
        let m = mem("                   ST 2000ffa0  ........ 00000000 ........ ........");
        assert_eq!((m[0].addr, m[0].size, m[0].value, m[0].known), (0x2000_ffa8, 4, 0, 0xf));
        let m = mem("LD 00000020  ........ ........ ........ 00000080     0000000020    NM ISH INC");
        assert_eq!((m[0].addr, m[0].value), (0x20, 0x80));
    }

    #[test]
    fn diagram_continuation() {
        let mut p = Parser::new();
        p.parse("       3981 tic ES  (000000d8:c878)     T thrd_s:         LDMCS    r0!,{r3-r6}").unwrap();
        p.parse("                    LD 00004020  ........ 626d654d 2043424d 4545203e   S:0000000000004020  NM NSH")
            .unwrap();
        let l = p
            .parse("                       00004010  3e000a73 ........ ........ ........   S:0000000000004010  NM NSH")
            .unwrap();
        assert_eq!(l.time, Some(3981));
        let Record::Memory(m) = l.record else { panic!() };
        assert_eq!((m[0].addr, m[0].value), (0x401c, 0x3e000a73));
    }

    #[test]
    fn exceptions() {
        assert_eq!(
            one("13340000 ps E 00000240 0000000f CoreEvent_IRQ"),
            Record::Exception { number: Some(15), name: "CoreEvent_IRQ".into(), target: Some(0x240) }
        );
        assert_eq!(
            one("    2568 tic ES EXC [0x0f] SysTick"),
            Record::Exception { number: Some(15), name: "SysTick".into(), target: None }
        );
        assert_eq!(
            one("0 clk cpu0 E DebugEvent_HaltingDebugState 00000000"),
            Record::Ignored("E DebugEvent_HaltingDebugState".into())
        );
    }

    #[test]
    fn time_and_core() {
        let mut p = Parser::new();
        let l = p.parse("2 clk cpu1 R r0 3003fe00").unwrap();
        assert_eq!((l.time, l.core, l.unit), (Some(2), Some(1), Some(TimeUnit::Cycle("clk".into()))));
        let l = p.parse("   R r1 00000000").unwrap();
        assert_eq!((l.time, l.unit), (Some(2), None));
    }
}
