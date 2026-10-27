//! Check of Cortex-M RTL bus-side copies (`B…___D/S`) against the core's
//! data records (`M…___D`). The index keeps the core records; the copies
//! are matched by access, address, size and bytes within a few
//! instructions (a copy may come before or after its core record), and the
//! report says whether any copy holds an access the index does not have.

use std::collections::{HashMap, VecDeque};

use simgdb_tarmac::MemAccess;

use crate::{BusCopies, Sample};

/// Instructions a copy and its core record may be apart.
pub const WINDOW: u64 = 64;
const SAMPLES: usize = 5;

type Key = (bool, u64, u8);

#[derive(Default)]
pub struct BusCheck {
    active: bool,
    core: HashMap<Key, VecDeque<(u64, u64, u8)>>,
    bus: HashMap<Key, VecDeque<(u64, u64, u8, Sample)>>,
    out: BusCopies,
    swept_at: u64,
}

impl BusCheck {
    /// A core data access made by instruction `at`.
    pub fn core(&mut self, at: u64, a: &MemAccess) {
        if !self.active {
            return;
        }
        let key = (a.write, a.addr, a.size);
        if let Some(q) = self.bus.get_mut(&key) {
            let r = a.reversed();
            if let Some(k) = q.iter().position(|e| (e.1, e.2) == (a.value, a.known)) {
                q.remove(k);
                self.out.matched += 1;
                return;
            }
            if let Some(k) = q.iter().position(|e| (e.1, e.2) == (r.value, r.known)) {
                q.remove(k);
                self.out.matched_reversed += 1;
                return;
            }
        }
        self.core.entry(key).or_default().push_back((at, a.value, a.known));
    }

    /// A bus-side copy seen at instruction `at`.
    pub fn bus(&mut self, at: u64, a: &MemAccess, sample: Sample) {
        self.active = true;
        self.out.records += 1;
        let key = (a.write, a.addr, a.size);
        if let Some(q) = self.core.get_mut(&key) {
            let r = a.reversed();
            if let Some(k) = q.iter().position(|e| (e.1, e.2) == (a.value, a.known)) {
                q.remove(k);
                self.out.matched += 1;
                return;
            }
            if let Some(k) = q.iter().position(|e| (e.1, e.2) == (r.value, r.known)) {
                q.remove(k);
                self.out.matched_reversed += 1;
                return;
            }
        }
        self.bus.entry(key).or_default().push_back((at, a.value, a.known, sample));
    }

    /// Settle entries older than the window (cheap; call per instruction).
    pub fn sweep(&mut self, now: u64) {
        if !self.active || now < self.swept_at + WINDOW {
            return;
        }
        self.swept_at = now;
        let old = |at: u64| at + WINDOW < now;
        let out = &mut self.out;
        self.bus.retain(|_, q| {
            while q.front().is_some_and(|e| old(e.0)) {
                let (_, _, _, s) = q.pop_front().unwrap();
                out.unmatched += 1;
                if out.unmatched_samples.len() < SAMPLES {
                    out.unmatched_samples.push(s);
                }
            }
            !q.is_empty()
        });
        self.core.retain(|_, q| {
            while q.front().is_some_and(|e| old(e.0)) {
                q.pop_front();
                out.core_without_copy += 1;
            }
            !q.is_empty()
        });
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
