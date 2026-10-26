//! SW track (PLAN Step 1): per instruction, where the software is, as
//! signals to view next to hardware waveforms. Formats (VCD) live in the
//! front end; this produces the samples.

use std::collections::HashMap;

use simgdb_store::NONE;
use simgdb_store::calls::{FRAME_EXCEPTION, TOP_LEVEL};

use crate::queries::Range;
use crate::{Engine, EngineError, Target};

/// One instruction's software state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackRow {
    pub index: u64,
    /// Recorded time stamp, `None` if the instruction has none.
    pub time: Option<u64>,
    pub pc: u64,
    /// Function id (into `TrackNames::functions`), 0 = no source.
    pub function: u32,
    /// File id (into `TrackNames::files`), 0 = no source.
    pub file: u32,
    pub line: u32,
    /// Call depth from the trace flow (0 = the code the trace started in).
    pub depth: u32,
    /// Exception being handled (nearest exception frame), 0 = none,
    /// `EXCEPTION_UNKNOWN` = in a handler whose number the trace did not give.
    pub exception: u32,
    /// Bit k set: the instruction is on marker line k.
    pub markers: u64,
}

pub const EXCEPTION_UNKNOWN: u32 = u32::MAX;

/// Names behind the ids. Index 0 is "" (no source).
#[derive(Clone, Debug, Default)]
pub struct TrackNames {
    pub functions: Vec<String>,
    pub files: Vec<String>,
}

impl Engine {
    /// Samples for `range`, one per instruction, plus the id tables.
    /// `markers` are targets (`file.c:123`, functions, PCs) shown as 1-bit
    /// signals; at most 64.
    pub fn track(
        &self,
        range: Range,
        markers: &[Target],
    ) -> Result<(impl Iterator<Item = TrackRow> + '_, TrackNames), EngineError> {
        if markers.len() > 64 {
            return Err(EngineError::Query("at most 64 marker lines".into()));
        }
        let ds = &self.ds;
        // Exception number in force per frame (parents come first).
        let frames = ds.call_kind.len();
        let mut exc_of = vec![0u32; frames];
        for f in 0..frames {
            let p = ds.call_parent.get(f);
            let parent = if p == TOP_LEVEL { 0 } else { exc_of[p as usize] };
            exc_of[f] = if ds.call_kind.get(f) == FRAME_EXCEPTION {
                // Not the parent's number: that would show the wrong handler.
                ds.call_number.get(f)
            } else {
                parent
            };
        }
        let mut marker_pcs: HashMap<u64, u64> = HashMap::new();
        for (k, t) in markers.iter().enumerate() {
            for pc in self.target_pcs(t)? {
                *marker_pcs.entry(pc).or_default() |= 1 << k;
            }
        }
        // Source per PC, with interned names (built once over the ELF code).
        let mut names = TrackNames { functions: vec![String::new()], files: vec![String::new()] };
        let mut fn_ids: HashMap<u32, u32> = HashMap::new();
        let mut file_ids: HashMap<u32, u32> = HashMap::new();
        let mut by_pc: HashMap<u64, (u32, u32, u32)> = HashMap::new();
        for i in 0..ds.src_pc.len() {
            let f = ds.src_function.get(i);
            let fid = *fn_ids.entry(f).or_insert_with(|| {
                names.functions.push(ds.strings.get(f as usize).cloned().unwrap_or_default());
                names.functions.len() as u32 - 1
            });
            let fl = ds.src_file.get(i);
            let flid = *file_ids.entry(fl).or_insert_with(|| {
                names.files.push(ds.strings.get(fl as usize).cloned().unwrap_or_default());
                names.files.len() as u32 - 1
            });
            by_pc.insert(ds.src_pc.get(i), (fid, flid, ds.src_line.get(i)));
        }
        let end = range.to.min(self.end().saturating_sub(1));
        let rows = (range.from..=end).filter(move |&i| i < self.end()).map(move |i| {
            let pc = ds.insn_pc.get(i as usize);
            let t = ds.insn_time.get(i as usize);
            let f = ds.insn_frame.get(i as usize);
            let (function, file, line) = by_pc.get(&pc).copied().unwrap_or((0, 0, 0));
            TrackRow {
                index: i,
                time: (t != NONE).then_some(t),
                pc,
                function,
                file,
                line,
                depth: if f == TOP_LEVEL { 0 } else { ds.call_depth.get(f as usize) },
                exception: if f == TOP_LEVEL { 0 } else { exc_of[f as usize] },
                markers: marker_pcs.get(&pc).copied().unwrap_or(0),
            }
        });
        Ok((rows, names))
    }
}
