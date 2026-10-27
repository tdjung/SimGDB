//! Check of Cortex-M RTL bus-side copies (`B…___D/S`) against the core's
//! data records (`M…___D`). The index keeps the core records; each copy is
//! matched to a core record within a few instructions (a copy may come
//! before or after it): the same access, address, size and bytes, or a part
//! of one (the bus splits an unaligned access). The report says whether any
//! copy holds an access the index does not have, reads and writes apart: a
//! missing read only leaves a value unknown, a missing write would leave a
//! stale value. A sample of an unmatched copy names the nearest core record
//! that touches the same bytes, if any, so the report alone explains it.
//!
//! Both sides are looked up by address (an access is at most 8 bytes, so a
//! record containing or overlapping a copy starts at most 7 bytes before
//! it), which keeps the check proportional to the number of records.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};

use simgdb_tarmac::MemAccess;

use crate::{BusCopies, Sample};

/// Instructions a copy and its core record may be apart.
pub const WINDOW: u64 = 64;
const SAMPLES: usize = 5;
/// Largest access in bytes (both kinds of record).
const MAX: u64 = 8;

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

/// Records in arrival order, numbered consecutively, with an index by
/// 8-byte block of their start address.
struct Window<T> {
    items: VecDeque<Option<T>>,
    /// Id of `items[0]`.
    base: u64,
    by_block: HashMap<u64, Vec<u64>, BuildHasherDefault<AddrHasher>>,
}

impl<T> Default for Window<T> {
    fn default() -> Self {
        Window { items: VecDeque::new(), base: 0, by_block: HashMap::default() }
    }
}

/// Multiplicative hash for addresses (SipHash is needlessly slow here).
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl<T> Window<T> {
    fn push(&mut self, addr: u64, item: T) {
        let id = self.base + self.items.len() as u64;
        self.items.push_back(Some(item));
        self.by_block.entry(addr >> 3).or_default().push(id);
    }

    fn get(&self, id: u64) -> Option<&T> {
        self.items.get((id - self.base) as usize)?.as_ref()
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut T> {
        self.items.get_mut((id - self.base) as usize)?.as_mut()
    }

    /// Ids of records starting in `[lo, hi)` (by `addr` of each), newest
    /// first.
    fn starting_in(&self, lo: u64, hi: u64, addr: impl Fn(&T) -> u64) -> Vec<u64> {
        let mut ids: Vec<u64> = Vec::new();
        for block in (lo >> 3)..=((hi.max(lo + 1) - 1) >> 3) {
            if let Some(v) = self.by_block.get(&block) {
                ids.extend(v.iter().copied().filter(|&id| self.get(id).is_some_and(|t| (lo..hi).contains(&addr(t)))));
            }
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids
    }

    fn remove(&mut self, id: u64, addr: u64) -> Option<T> {
        let item = self.items.get_mut((id - self.base) as usize)?.take()?;
        if let Some(v) = self.by_block.get_mut(&(addr >> 3)) {
            v.retain(|&x| x != id);
            if v.is_empty() {
                self.by_block.remove(&(addr >> 3));
            }
        }
        Some(item)
    }

    /// The oldest record still present, if `old` says it has expired.
    fn pop_old(&mut self, old: impl Fn(&T) -> bool, addr: impl Fn(&T) -> u64) -> Option<T> {
        loop {
            match self.items.front() {
                Some(None) => {
                    self.items.pop_front();
                    self.base += 1;
                }
                Some(Some(t)) if old(t) => {
                    let (id, a) = (self.base, addr(t));
                    let item = self.remove(id, a);
                    self.items.pop_front();
                    self.base += 1;
                    return item;
                }
                _ => return None,
            }
        }
    }
}

#[derive(Default)]
pub struct BusCheck {
    active: bool,
    mask: bool,
    core: Window<Core>,
    bus: Window<Pending>,
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Match {
    // Ordered by preference.
    Exact,
    Part,
    Reversed,
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
        // Copies seen before their core record: those inside it match (all
        // parts); others touching it note it as their nearest core record.
        for id in self.bus.starting_in(a.addr.saturating_sub(MAX - 1), end(a), |p| p.a.addr) {
            let p = self.bus.get(id).unwrap();
            if let Some(m) = compare(&p.a, a) {
                let addr = p.a.addr;
                self.bus.remove(id, addr);
                self.count(m);
                copied = true;
            } else if p.a.write == a.write && overlap(&p.a, a) {
                let d = at as i64 - p.at as i64;
                let p = self.bus.get_mut(id).unwrap();
                if p.near.is_none_or(|(n, _)| d.abs() < n.abs()) {
                    p.near = Some((d, *a));
                }
            }
        }
        self.core.push(a.addr, Core { at, a: *a, copied });
    }

    /// A bus-side copy seen at instruction `at`, line `line` of the trace.
    pub fn bus(&mut self, at: u64, a: &MemAccess, line: u64) {
        self.active = true;
        self.out.records += 1;
        // The best core record it fits, newest first: an exact one without a
        // copy yet (a loop repeats the same access: an older record with a
        // copy is not this one's), then a part (parts may share a record),
        // then reversed bytes.
        let mut best: Option<(Match, u64)> = None;
        let mut near: Option<(i64, MemAccess)> = None;
        for id in self.core.starting_in(a.addr.saturating_sub(MAX - 1), end(a), |c| c.a.addr) {
            let c = self.core.get(id).unwrap();
            let m = compare(a, &c.a).filter(|m| *m == Match::Part || !c.copied);
            if let Some(m) = m
                && best.is_none_or(|(b, bid)| m < b || (m == b && id > bid))
            {
                best = Some((m, id));
            }
            if c.a.write == a.write && overlap(&c.a, a) {
                let d = c.at as i64 - at as i64;
                if near.is_none_or(|(n, _)| d.abs() < n.abs()) {
                    near = Some((d, c.a));
                }
            }
        }
        match best {
            Some((m, id)) => {
                self.core.get_mut(id).unwrap().copied = true;
                self.count(m);
            }
            None => self.bus.push(a.addr, Pending { at, line, a: *a, near }),
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
        while let Some(p) = self.bus.pop_old(|p| old(p.at), |p| p.a.addr) {
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
        while let Some(c) = self.core.pop_old(|c| old(c.at), |c| c.a.addr) {
            if !c.copied {
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
