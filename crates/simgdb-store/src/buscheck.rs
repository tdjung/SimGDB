//! Check of Cortex-M RTL bus-side copies (`B…___D/S`) against the core's
//! data records (`M…___D`). The index keeps the core records; each copy is
//! matched to a core record within a few instructions (a copy may come
//! before or after it): the same access, address, size and bytes, or a part
//! of one (the bus splits an unaligned access). The report says whether any
//! copy holds an access the index does not have, reads and writes apart: a
//! missing read only leaves a value unknown, a missing write would leave a
//! stale value. A sample of an unmatched copy names the nearest core record
//! that touches the same bytes, if any, so the report alone explains it.

use std::collections::VecDeque;

use simgdb_tarmac::MemAccess;

use crate::{BusCopies, Sample};

/// Instructions a copy and its core record may be apart.
pub const WINDOW: u64 = 64;
const SAMPLES: usize = 5;

struct Core {
    at: u64,
    a: MemAccess,
    copied: bool,
}

struct Pending {
    at: u64,
    line: u64,
    a: MemAccess,
    /// Nearest core record touching the same bytes: (instructions after
    /// the copy, negative before; the record).
    near: Option<(i64, MemAccess)>,
}

#[derive(Default)]
pub struct BusCheck {
    active: bool,
    mask: bool,
    core: VecDeque<Core>,
    bus: VecDeque<Pending>,
    out: BusCopies,
    swept_at: u64,
}

/// Byte `k` of `a` (by address) if known.
fn byte(a: &MemAccess, k: u64) -> Option<u8> {
    (a.known & (1 << k) != 0).then(|| (a.value >> (8 * k)) as u8)
}

fn end(a: &MemAccess) -> u64 {
    a.addr + a.size as u64
}

fn overlap(x: &MemAccess, y: &MemAccess) -> bool {
    x.addr < end(y) && y.addr < end(x)
}

/// `part` lies within `whole` and has the same bytes there.
fn within(part: &MemAccess, whole: &MemAccess) -> bool {
    part.addr >= whole.addr
        && end(part) <= end(whole)
        && (0..part.size as u64).all(|k| byte(part, k) == byte(whole, part.addr - whole.addr + k))
}

enum Match {
    Exact,
    Reversed,
    Part,
}

fn compare(copy: &MemAccess, core: &MemAccess) -> Option<Match> {
    if copy.write != core.write || !overlap(copy, core) {
        return None;
    }
    if (copy.addr, copy.size) == (core.addr, core.size) {
        if (copy.value, copy.known) == (core.value, core.known) {
            return Some(Match::Exact);
        }
        let r = copy.reversed();
        if (r.value, r.known) == (core.value, core.known) {
            return Some(Match::Reversed);
        }
    }
    within(copy, core).then_some(Match::Part)
}

/// `write 1 byte at 0x2001ffcd: 00` (bytes by address, `??` unknown).
fn describe(a: &MemAccess, mask: bool) -> String {
    let bytes: String = (0..a.size as u64)
        .map(|k| match byte(a, k) {
            Some(_) if mask => "hh".to_string(),
            Some(b) => format!("{b:02x}"),
            None => "??".to_string(),
        })
        .collect();
    let addr = if mask { "0x…".to_string() } else { format!("{:#010x}", a.addr) };
    format!(
        "{} {} byte{} at {addr}: {bytes}",
        if a.write { "write" } else { "read" },
        a.size,
        if a.size == 1 { "" } else { "s" }
    )
}

impl BusCheck {
    pub fn new(mask: bool) -> BusCheck {
        BusCheck { mask, ..Default::default() }
    }

    fn count(&mut self, m: Match) {
        match m {
            Match::Exact => self.out.matched += 1,
            Match::Reversed => self.out.matched_reversed += 1,
            Match::Part => self.out.matched_part += 1,
        }
    }

    /// A core data access made by instruction `at`.
    pub fn core(&mut self, at: u64, a: &MemAccess) {
        if !self.active {
            return;
        }
        let mut copied = false;
        // Copies seen before their core record (all of its parts).
        let mut k = 0;
        while k < self.bus.len() {
            let p = &mut self.bus[k];
            if let Some(m) = compare(&p.a, a) {
                self.bus.remove(k);
                self.count(m);
                copied = true;
                continue;
            }
            if p.a.write == a.write && overlap(&p.a, a) {
                let d = at as i64 - p.at as i64;
                if p.near.is_none_or(|(n, _)| d.abs() < n.abs()) {
                    p.near = Some((d, *a));
                }
            }
            k += 1;
        }
        self.core.push_back(Core { at, a: *a, copied });
    }

    /// A bus-side copy seen at instruction `at`, line `line` of the trace.
    pub fn bus(&mut self, at: u64, a: &MemAccess, line: u64) {
        self.active = true;
        self.out.records += 1;
        // The newest core record it fits that has no copy yet, an exact one
        // first (a loop repeats the same access: an older record with a
        // copy is not this one's). Parts may share a record.
        let mut best: Option<(usize, Match)> = None;
        let mut near: Option<(i64, MemAccess)> = None;
        for (k, c) in self.core.iter().enumerate().rev() {
            match compare(a, &c.a) {
                Some(Match::Exact) if !c.copied => {
                    best = Some((k, Match::Exact));
                    break;
                }
                Some(Match::Part) if best.is_none() => best = Some((k, Match::Part)),
                Some(Match::Reversed) if best.is_none() && !c.copied => best = Some((k, Match::Reversed)),
                _ => {}
            }
            if near.is_none() && c.a.write == a.write && overlap(&c.a, a) {
                near = Some((c.at as i64 - at as i64, c.a));
            }
        }
        match best {
            Some((k, m)) => {
                self.core[k].copied = true;
                self.count(m);
            }
            None => self.bus.push_back(Pending { at, line, a: *a, near }),
        }
    }

    fn sample(&self, p: &Pending) -> Sample {
        let mut text = format!("bus copy: {}", describe(&p.a, self.mask));
        match p.near {
            Some((d, c)) => {
                let when = match d {
                    0 => "the same instruction".to_string(),
                    d if d < 0 => format!("{} instructions before", -d),
                    d => format!("{d} instructions after"),
                };
                text.push_str(&format!("; nearest core record on these bytes, {when}: {}", describe(&c, self.mask)));
            }
            None => text.push_str(&format!("; no core record on these bytes within {WINDOW} instructions")),
        }
        Sample { line: p.line, text }
    }

    /// Settle entries older than the window (cheap; call per instruction).
    pub fn sweep(&mut self, now: u64) {
        if !self.active || now < self.swept_at + WINDOW {
            return;
        }
        self.swept_at = now;
        let old = |at: u64| at + WINDOW < now;
        while self.bus.front().is_some_and(|e| old(e.at)) {
            let p = self.bus.pop_front().unwrap();
            let full = if p.a.write {
                self.out.unmatched_write_samples.len() >= SAMPLES
            } else {
                self.out.unmatched_read_samples.len() >= SAMPLES
            };
            let s = (!full).then(|| self.sample(&p));
            let o = &mut self.out;
            if p.a.write {
                o.unmatched_writes += 1;
                o.unmatched_write_samples.extend(s);
            } else {
                o.unmatched_reads += 1;
                o.unmatched_read_samples.extend(s);
            }
        }
        while self.core.front().is_some_and(|c| old(c.at)) {
            if !self.core.pop_front().unwrap().copied {
                self.out.core_without_copy += 1;
            }
        }
    }

    pub fn finish(mut self) -> Option<BusCopies> {
        if !self.active {
            return None;
        }
        self.sweep(u64::MAX - WINDOW - 1);
        self.out.window = WINDOW;
        Some(self.out)
    }
}
