//! Condition language for `find_first` (ARCHITECTURE §2.4): a small,
//! checkable expression language instead of arbitrary code.
//!
//! ```text
//! expr    := or
//! or      := and ("||" and)*
//! and     := unary ("&&" unary)*
//! unary   := "!" unary | "(" expr ")" | compare
//! compare := operand (("==" | "!=" | "<" | "<=" | ">" | ">=") operand)?
//! operand := NUMBER | "STRING" | pc | line | func | file
//!          | reg("r0") | var("pool[2].len") | mem8(A) | mem16(A) | mem32(A) | mem64(A)
//!          | sym("name")          A := NUMBER | sym("name")
//! ```
//!
//! `line == "file.c:123"`, `func == "name"` and `file == "file.c"` use the
//! source attribution (ARCHITECTURE §6). Values the trace does not show make
//! a comparison unknown; `&&`, `||` and `!` follow three-valued logic, and
//! only a definite true is a match.
//!
//! Evaluation visits only points where some input can change: executions of
//! the code a `pc`/`line`/`func`/`file` comparison names (and the point
//! after), register writes, and memory records of the bytes read. Between
//! two such points the condition cannot change.

use std::collections::{BTreeSet, HashSet};

use serde::Serialize;
use simgdb_elf::variables::Place;
use simgdb_isa::{RegId, RegState};
use simgdb_model::Point;

use crate::queries::{Range, Target, parse_number};
use crate::{Engine, EngineError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The first matching point at or after `from`.
    Forward,
    /// The last matching point at or before `to`.
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
enum Operand {
    Int(i128),
    Str(String),
    Pc,
    Line,
    Func,
    File,
    Reg(RegId, String),
    Var(Place, String),
    Mem(u64, u64),
}

#[derive(Clone, Debug)]
enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    /// A comparison; a lone operand means `!= 0`.
    Cmp(Operand, Op, Operand),
    /// `pc`, `line`, `func` or `file` compared for equality with a constant:
    /// true exactly at executions of `pcs`.
    At {
        pcs: HashSet<u64>,
        sorted: Vec<u64>,
        negate: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum V {
    Int(i128),
    Str(String),
    Unknown,
}

/// What `find_first` found.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub condition: String,
    pub range: Range,
    pub direction: Direction,
    /// The matching point, if any.
    pub found: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pc: Option<u64>,
    /// Source position of the matching point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::Where>,
    /// The condition's inputs at the matching point.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Input>,
    /// Points evaluated (where an input could change).
    pub evaluated: u64,
    /// Of those, points where the condition was undecided (unknown values).
    pub undecided: u64,
    /// The first undecided point in search order before the match: the true
    /// answer could be there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_undecided: Option<u64>,
    /// Undecided stretches `[from, to]` before the match in search order
    /// (first 5, in that order), e.g. only before startup set a variable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undecided_ranges: Vec<[u64; 2]>,
    /// The instruction just before the match and whether the condition held
    /// there: if not, that instruction (or the PC reaching this point) made
    /// it true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Neighbour>,
    /// Searching backward: the point after the match, where the condition no
    /// longer holds (its `index - 1` instruction ended it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Neighbour>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Neighbour {
    pub index: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pc: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Whether the condition held at this point (`null`: undecided).
    pub holds: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Input {
    pub name: String,
    /// Number, or `null` when unknown.
    pub value: Option<String>,
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    engine: &'a Engine,
    /// Change points of register / memory inputs, and whether any input
    /// changes at every point.
    regs: BTreeSet<RegId>,
    bytes: BTreeSet<u64>,
    every_point: bool,
}

fn err(msg: impl Into<String>) -> EngineError {
    EngineError::Query(msg.into())
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.s[self.pos..].starts_with(char::is_whitespace) {
            self.pos += self.s[self.pos..].chars().next().unwrap().len_utf8();
        }
    }

    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.s[self.pos..].starts_with(t) {
            self.pos += t.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &str) -> Result<(), EngineError> {
        if self.eat(t) { Ok(()) } else { Err(err(format!("expected `{t}` at column {}", self.pos + 1))) }
    }

    fn or(&mut self) -> Result<Expr, EngineError> {
        let mut v = vec![self.and()?];
        while self.eat("||") {
            v.push(self.and()?);
        }
        Ok(if v.len() == 1 { v.pop().unwrap() } else { Expr::Or(v) })
    }

    fn and(&mut self) -> Result<Expr, EngineError> {
        let mut v = vec![self.unary()?];
        while self.eat("&&") {
            v.push(self.unary()?);
        }
        Ok(if v.len() == 1 { v.pop().unwrap() } else { Expr::And(v) })
    }

    fn unary(&mut self) -> Result<Expr, EngineError> {
        self.ws();
        if self.s[self.pos..].starts_with('!') && !self.s[self.pos..].starts_with("!=") {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat("(") {
            let e = self.or()?;
            self.expect(")")?;
            return Ok(e);
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Expr, EngineError> {
        let a = self.operand()?;
        self.ws();
        let ops = [("==", Op::Eq), ("!=", Op::Ne), ("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)];
        let Some(&(_, op)) = ops.iter().find(|(t, _)| self.eat(t)) else {
            if matches!(a, Operand::Str(_) | Operand::Line | Operand::Func | Operand::File) {
                return Err(err("a string or line/func/file needs a comparison"));
            }
            return self.make(a, Op::Ne, Operand::Int(0));
        };
        let b = self.operand()?;
        self.make(a, op, b)
    }

    /// Build a comparison, turning code-location equality into `At`.
    fn make(&mut self, a: Operand, op: Op, b: Operand) -> Result<Expr, EngineError> {
        let (loc, k) = match (&a, &b) {
            (Operand::Pc | Operand::Line | Operand::Func | Operand::File, k) => (&a, k),
            (k, Operand::Pc | Operand::Line | Operand::Func | Operand::File) => (&b, k),
            _ => {
                for x in [&a, &b] {
                    match x {
                        Operand::Reg(r, _) => {
                            self.regs.insert(*r);
                        }
                        Operand::Var(p, _) => self.bytes.extend(p.addr..p.addr + p.size.unwrap_or(0)),
                        Operand::Mem(addr, size) => self.bytes.extend(*addr..addr + size),
                        Operand::Str(_) => return Err(err("strings compare only with line, func or file")),
                        _ => {}
                    }
                }
                return Ok(Expr::Cmp(a, op, b));
            }
        };
        let pcs: Vec<u64> = match (loc, k, op) {
            (Operand::Pc, Operand::Int(v), Op::Eq | Op::Ne) => vec![*v as u64],
            (Operand::Pc, Operand::Int(_), _) => {
                self.every_point = true;
                return Ok(Expr::Cmp(a, op, b));
            }
            (Operand::Line, Operand::Str(s), Op::Eq | Op::Ne) => match Target::parse(s) {
                t @ Target::Line { .. } => self.engine.target_pcs(&t)?,
                _ => return Err(err(format!("line wants \"file:line\", not {s:?}"))),
            },
            (Operand::Func, Operand::Str(s), Op::Eq | Op::Ne) => {
                self.engine.target_pcs(&Target::Function(s.clone()))?
            }
            (Operand::File, Operand::Str(s), Op::Eq | Op::Ne) => self.engine.file_pcs(s)?,
            _ => return Err(err("pc compares with a number; line, func and file with == or != and a string")),
        };
        let mut sorted = pcs;
        sorted.sort_unstable();
        sorted.dedup();
        Ok(Expr::At { pcs: sorted.iter().copied().collect(), sorted, negate: op == Op::Ne })
    }

    fn string(&mut self) -> Result<String, EngineError> {
        self.expect("\"")?;
        let end = self.s[self.pos..].find('"').ok_or_else(|| err("unterminated string"))?;
        let v = self.s[self.pos..self.pos + end].to_string();
        self.pos += end + 1;
        Ok(v)
    }

    fn call_string(&mut self) -> Result<String, EngineError> {
        self.expect("(")?;
        let v = self.string()?;
        self.expect(")")?;
        Ok(v)
    }

    fn address(&mut self) -> Result<u64, EngineError> {
        self.ws();
        if self.eat("sym") {
            let name = self.call_string()?;
            return self.engine.symbol(&name).ok_or_else(|| err(format!("no symbol {name}")));
        }
        match self.operand()? {
            Operand::Int(v) if v >= 0 => Ok(v as u64),
            _ => Err(err("an address is a number or sym(\"name\")")),
        }
    }

    fn operand(&mut self) -> Result<Operand, EngineError> {
        self.ws();
        let rest = &self.s[self.pos..];
        if rest.starts_with('"') {
            return Ok(Operand::Str(self.string()?));
        }
        let word: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if word.is_empty() {
            return Err(err(format!("expected a value at column {}", self.pos + 1)));
        }
        if word.starts_with(|c: char| c.is_ascii_digit()) {
            self.pos += word.len();
            let v = parse_number(&word).ok_or_else(|| err(format!("bad number {word}")))?;
            return Ok(Operand::Int(v as i128));
        }
        self.pos += word.len();
        Ok(match word.as_str() {
            "pc" => Operand::Pc,
            "line" => Operand::Line,
            "func" => Operand::Func,
            "file" => Operand::File,
            "reg" => {
                let name = self.call_string()?;
                Operand::Reg(self.engine.register_id(&name)?, name)
            }
            "var" => {
                let path = self.call_string()?;
                let place = self.engine.variable(&path)?;
                match place.size {
                    Some(1..=8) => Operand::Var(place, path),
                    _ => return Err(err(format!("{path} ({}) is not a scalar", place.type_name))),
                }
            }
            "sym" => {
                let name = self.call_string()?;
                Operand::Int(self.engine.symbol(&name).ok_or_else(|| err(format!("no symbol {name}")))? as i128)
            }
            "mem8" | "mem16" | "mem32" | "mem64" => {
                let size = word[3..].parse::<u64>().unwrap() / 8;
                self.expect("(")?;
                let addr = self.address()?;
                self.expect(")")?;
                Operand::Mem(addr, size)
            }
            _ => return Err(err(format!("unknown name {word}"))),
        })
    }
}

/// Register state carried forward through points.
struct Regs<'e> {
    engine: &'e Engine,
    state: RegState,
    row: usize,
    at: u64,
}

impl<'e> Regs<'e> {
    fn new(engine: &'e Engine, index: u64) -> Self {
        let (state, _) = engine.replay(index);
        let row = engine.ds.rw_at.partition_point(0, engine.ds.rw_at.len(), |at| at <= index);
        Regs { engine, state, row, at: index }
    }

    fn advance(&mut self, index: u64) {
        if index < self.at {
            *self = Regs::new(self.engine, index);
            return;
        }
        let ds = &self.engine.ds;
        let total = ds.rw_at.len();
        while self.row < total && ds.rw_at.get(self.row) <= index {
            let r = ds.rw_reg.get(self.row) as usize;
            let known = ds.rw_known.get(self.row);
            self.state.values[r] = ds.rw_value.get(self.row) & known;
            self.state.known[r] = known;
            self.row += 1;
        }
        self.at = index;
    }

    fn get(&self, reg: RegId) -> Option<u64> {
        let bits = self.engine.isa.registers()[reg as usize].bits;
        self.state.get(reg, bits)
    }
}

impl Engine {
    /// Code addresses whose source file matches `file`.
    pub(crate) fn file_pcs(&self, file: &str) -> Result<Vec<u64>, EngineError> {
        let ds = &self.ds;
        let pcs: Vec<u64> = (0..ds.src_pc.len())
            .filter(|&i| {
                crate::queries::file_matches(
                    ds.strings.get(ds.src_file.get(i) as usize).map(String::as_str).unwrap_or(""),
                    file,
                )
            })
            .map(|i| ds.src_pc.get(i))
            .collect();
        if pcs.is_empty() {
            return Err(err(format!("no code from {file}")));
        }
        Ok(pcs)
    }

    fn value(&self, o: &Operand, index: u64, regs: &Regs) -> Result<V, EngineError> {
        Ok(match o {
            Operand::Int(v) => V::Int(*v),
            Operand::Str(s) => V::Str(s.clone()),
            Operand::Pc => match self.pc(index)?.value {
                Some(p) => V::Int(p as i128),
                None => V::Unknown,
            },
            Operand::Reg(r, _) => regs.get(*r).map(|v| V::Int(v as i128)).unwrap_or(V::Unknown),
            Operand::Var(place, _) => self.variable_int(place, index)?.map(V::Int).unwrap_or(V::Unknown),
            Operand::Mem(addr, size) => {
                let v = self.combine(&self.memory(*addr, *size, index)?);
                match (v.value, v.known_mask) {
                    (Some(x), None) => V::Int(x as i128),
                    _ => V::Unknown,
                }
            }
            Operand::Line | Operand::Func | Operand::File => unreachable!("turned into At"),
        })
    }

    /// `Some(true/false)`, or `None` when undecided.
    fn eval(&self, e: &Expr, index: u64, regs: &Regs) -> Result<Option<bool>, EngineError> {
        Ok(match e {
            Expr::Or(v) => {
                let mut any_unknown = false;
                for x in v {
                    match self.eval(x, index, regs)? {
                        Some(true) => return Ok(Some(true)),
                        None => any_unknown = true,
                        Some(false) => {}
                    }
                }
                (!any_unknown).then_some(false)
            }
            Expr::And(v) => {
                let mut any_unknown = false;
                for x in v {
                    match self.eval(x, index, regs)? {
                        Some(false) => return Ok(Some(false)),
                        None => any_unknown = true,
                        Some(true) => {}
                    }
                }
                (!any_unknown).then_some(true)
            }
            Expr::Not(x) => self.eval(x, index, regs)?.map(|b| !b),
            Expr::At { pcs, negate, .. } => self.pc(index)?.value.map(|p| pcs.contains(&p) != *negate),
            Expr::Cmp(a, op, b) => match (self.value(a, index, regs)?, self.value(b, index, regs)?) {
                (V::Int(x), V::Int(y)) => Some(match op {
                    Op::Eq => x == y,
                    Op::Ne => x != y,
                    Op::Lt => x < y,
                    Op::Le => x <= y,
                    Op::Gt => x > y,
                    Op::Ge => x >= y,
                }),
                _ => None,
            },
        })
    }

    fn inputs(&self, e: &Expr, index: u64, regs: &Regs, out: &mut Vec<Input>) -> Result<(), EngineError> {
        match e {
            Expr::Or(v) | Expr::And(v) => {
                for x in v {
                    self.inputs(x, index, regs, out)?;
                }
            }
            Expr::Not(x) => self.inputs(x, index, regs, out)?,
            Expr::At { .. } => {}
            Expr::Cmp(a, _, b) => {
                for o in [a, b] {
                    let name = match o {
                        Operand::Reg(_, n) => format!("reg(\"{n}\")"),
                        Operand::Var(_, n) => format!("var(\"{n}\")"),
                        Operand::Mem(a, s) => format!("mem{}({a:#x})", s * 8),
                        _ => continue,
                    };
                    let value = match self.value(o, index, regs)? {
                        V::Int(v) => Some(format!("{v:#x}")),
                        _ => None,
                    };
                    if !out.iter().any(|i| i.name == name) {
                        out.push(Input { name, value });
                    }
                }
            }
        }
        Ok(())
    }

    fn at_points<'x>(e: &'x Expr, out: &mut Vec<&'x Vec<u64>>) {
        match e {
            Expr::Or(v) | Expr::And(v) => v.iter().for_each(|x| Engine::at_points(x, out)),
            Expr::Not(x) => Engine::at_points(x, out),
            Expr::At { sorted, .. } => out.push(sorted),
            Expr::Cmp(..) => {}
        }
    }

    /// The first (or last) point in `range` where `condition` holds.
    pub fn find_first(&self, condition: &str, range: Range, direction: Direction) -> Result<Found, EngineError> {
        let mut p = Parser {
            s: condition,
            pos: 0,
            engine: self,
            regs: BTreeSet::new(),
            bytes: BTreeSet::new(),
            every_point: false,
        };
        let expr = p.or()?;
        p.ws();
        if p.pos != condition.len() {
            return Err(err(format!("unexpected `{}` at column {}", &condition[p.pos..], p.pos + 1)));
        }
        let (from, to) = (range.from, range.to);
        // Points where an input can change.
        let mut points: Vec<u64> = vec![from];
        if p.every_point {
            points = (from..=to).collect();
        } else {
            let ds = &self.ds;
            let mut pcs = Vec::new();
            Engine::at_points(&expr, &mut pcs);
            for list in pcs {
                for &pc in list {
                    for i in self.executions_between(pc, from, to) {
                        points.push(i);
                        points.push(i + 1);
                    }
                }
            }
            if !p.regs.is_empty() {
                for row in ds.rw_at.partition_point(0, ds.rw_at.len(), |at| at < from)..ds.rw_at.len() {
                    let at = ds.rw_at.get(row);
                    if at > to {
                        break;
                    }
                    if p.regs.contains(&ds.rw_reg.get(row)) {
                        points.push(at);
                    }
                }
            }
            for &addr in &p.bytes {
                let n = ds.mi_addr.len();
                let lo = ds.mi_addr.partition_point(0, n, |a| a < addr);
                let hi = ds.mi_addr.partition_point(lo, n, |a| a <= addr);
                let a = ds.mi_at.partition_point(lo, hi, |at| at < from);
                for k in a..hi {
                    let at = ds.mi_at.get(k);
                    if at > to {
                        break;
                    }
                    points.push(at);
                }
            }
            points.retain(|&i| i >= from && i <= to);
            points.sort_unstable();
            points.dedup();
        }
        let mut found = Found {
            condition: condition.trim().to_string(),
            range,
            direction,
            found: None,
            pc: None,
            source: None,
            inputs: vec![],
            evaluated: 0,
            undecided: 0,
            first_undecided: None,
            undecided_ranges: vec![],
            previous: None,
            next: None,
        };
        // Undecided stretches in time order, as (first, last) positions.
        let mut stretches: Vec<(usize, usize)> = Vec::new();
        // The condition is constant from one point to just before the next.
        // Forward: in order. Backward: windows from the end, each in order,
        // keeping the last match of the window; a match then holds up to the
        // next point.
        let forward = direction == Direction::Forward;
        let window = self.ds.meta.checkpoint_interval.max(1) as usize;
        let len = points.len();
        let windows: Vec<std::ops::Range<usize>> = if forward {
            std::iter::once(0..len).collect()
        } else {
            (0..len).step_by(window).map(|s| s..(s + window).min(len)).rev().collect()
        };
        let end_of = |pos: usize| if pos + 1 < len { points[pos + 1] - 1 } else { to };
        for w in windows {
            if w.is_empty() {
                continue;
            }
            let mut regs = Regs::new(self, points[w.start]);
            let (mut hit, mut undecided) = (None, None);
            for pos in w {
                let i = points[pos];
                regs.advance(i);
                found.evaluated += 1;
                match self.eval(&expr, i, &regs)? {
                    Some(true) => {
                        hit = Some(pos);
                        if forward {
                            break;
                        }
                    }
                    None => {
                        found.undecided += 1;
                        match stretches.last_mut() {
                            Some((_, last)) if *last + 1 == pos => *last = pos,
                            _ => stretches.push((pos, pos)),
                        }
                        if forward {
                            undecided.get_or_insert(pos);
                        } else {
                            undecided = Some(pos);
                        }
                    }
                    Some(false) => {}
                }
            }
            if let (false, Some(h), Some(u)) = (forward, hit, undecided)
                && u < h
            {
                // Searching backward, the match comes first.
                undecided = None;
            }
            if found.first_undecided.is_none() {
                found.first_undecided = undecided.map(|u| if forward { points[u] } else { end_of(u) });
            }
            if let Some(h) = hit {
                let i = if forward { points[h] } else { end_of(h) };
                let regs = Regs::new(self, i);
                found.found = Some(self.point(i)?);
                found.pc = self.pc(i)?.value;
                found.source = found.pc.and_then(|pc| self.where_is(pc));
                self.inputs(&expr, i, &regs, &mut found.inputs)?;
                if i > 0 {
                    found.previous = Some(self.neighbour(&expr, i - 1)?);
                }
                if !forward && i < self.end() {
                    found.next = Some(self.neighbour(&expr, i + 1)?);
                }
                break;
            }
        }
        let at = found.found.as_ref().map(|p| p.index);
        stretches.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in stretches {
            match merged.last_mut() {
                Some((_, last)) if *last + 1 >= a => *last = (*last).max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut ranges: Vec<[u64; 2]> = merged
            .into_iter()
            .map(|(a, b)| [points[a], end_of(b)])
            .filter(|r| match (at, forward) {
                (Some(x), true) => r[0] < x,
                (Some(x), false) => r[1] > x,
                (None, _) => true,
            })
            .collect();
        if !forward {
            ranges.reverse();
        }
        ranges.truncate(5);
        found.undecided_ranges = ranges;
        Ok(found)
    }

    fn neighbour(&self, expr: &Expr, index: u64) -> Result<Neighbour, EngineError> {
        let regs = Regs::new(self, index);
        let pc = self.pc(index)?.value;
        Ok(Neighbour {
            index,
            pc,
            function: pc.and_then(|p| self.where_is(p)).map(|w| w.function),
            holds: self.eval(expr, index, &regs)?,
        })
    }

    fn executions_between(&self, pc: u64, from: u64, to: u64) -> impl Iterator<Item = u64> + '_ {
        let ds = &self.ds;
        let n = ds.pi_pc.len();
        let lo = ds.pi_pc.partition_point(0, n, |p| p < pc);
        let hi = ds.pi_pc.partition_point(lo, n, |p| p <= pc);
        let a = ds.pi_index.partition_point(lo, hi, |i| i < from);
        let b = ds.pi_index.partition_point(a, hi, |i| i <= to);
        (a..b).map(move |k| ds.pi_index.get(k))
    }
}
