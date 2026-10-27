//! Check of Cortex-M RTL bus-side copies (`B…___D/S`) against the core's
//! data records (`M…___D`). The index keeps the core records; each copy is
//! matched to a core record within a few instructions (a copy may come
//! before or after it): the same access, address, size and bytes, or a part
//! of one (the bus splits an unaligned access). The report says whether any
//! copy holds an access the index does not have, reads and writes apart: a
//! missing read only leaves a value unknown, a missing write would leave a
//! stale value.

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

#[derive(Default)]
pub struct BusCheck {
    active: bool,
    core: VecDeque<Core>,
    bus: VecDeque<(u64, MemAccess, Sample)>,
    out: BusCopies,
    swept_at: u64,
}

/// Byte `k` of `a` (by address) if known.
fn byte(a: &MemAccess, k: u64) -> Option<u8> {
    (a.known & (1 << k) != 0).then(|| (a.value >> (8 * k)) as u8)
}

/// `part` lies within `whole` and has the same bytes there.
fn within(part: &MemAccess, whole: &MemAccess) -> bool {
    part.write == whole.write
        && part.addr >= whole.addr
        && part.addr + part.size as u64 <= whole.addr + whole.size as u64
        && (0..part.size as u64).all(|k| byte(part, k) == byte(whole, part.addr - whole.addr + k))
}

enum Match {
    Exact,
    Reversed,
    Part,
}

fn compare(copy: &MemAccess, core: &MemAccess) -> Option<Match> {
    let same = |x: &MemAccess| {
        (x.write, x.addr, x.size, x.value, x.known) == (core.write, core.addr, core.size, core.value, core.known)
    };
    if same(copy) {
        Some(Match::Exact)
    } else if same(&copy.reversed()) {
        Some(Match::Reversed)
    } else if within(copy, core) {
        Some(Match::Part)
    } else {
        None
    }
}

impl BusCheck {
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
            if let Some(m) = compare(&self.bus[k].1, a) {
                self.bus.remove(k);
                self.count(m);
                copied = true;
            } else {
                k += 1;
            }
        }
        self.core.push_back(Core { at, a: *a, copied });
    }

    /// A bus-side copy seen at instruction `at`.
    pub fn bus(&mut self, at: u64, a: &MemAccess, sample: Sample) {
        self.active = true;
        self.out.records += 1;
        // The newest core record it fits that has no copy yet, an exact one
        // first (a loop repeats the same access: an older record with a
        // copy is not this one's). Parts may share a record.
        let mut best: Option<(usize, Match)> = None;
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
        }
        match best {
            Some((k, m)) => {
                self.core[k].copied = true;
                self.count(m);
            }
            None => self.bus.push_back((at, *a, sample)),
        }
    }

    /// Settle entries older than the window (cheap; call per instruction).
    pub fn sweep(&mut self, now: u64) {
        if !self.active || now < self.swept_at + WINDOW {
            return;
        }
        self.swept_at = now;
        let old = |at: u64| at + WINDOW < now;
        while self.bus.front().is_some_and(|e| old(e.0)) {
            let (_, a, s) = self.bus.pop_front().unwrap();
            let o = &mut self.out;
            if a.write {
                o.unmatched_writes += 1;
                if o.unmatched_write_samples.len() < SAMPLES {
                    o.unmatched_write_samples.push(s);
                }
            } else {
                o.unmatched_reads += 1;
                if o.unmatched_read_samples.len() < SAMPLES {
                    o.unmatched_read_samples.push(s);
                }
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
