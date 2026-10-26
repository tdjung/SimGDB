//! SW track as VCD: one signal per piece of software state, changing only
//! when it changes, on the trace's own time axis. Function and file names
//! are ids; the tables are written next to the VCD.

use std::io::{self, Write};

use simgdb_engine::{TrackNames, TrackRow};

/// How trace time stamps become VCD time.
pub enum Time {
    /// Recorded times in a time unit (`ps`, `ns`, ...).
    Trace { unit: String },
    /// Recorded cycles times a period (`period` in `unit`).
    Cycles { period: u64, unit: String },
    /// The instruction index as time (1 ns per instruction).
    Index,
}

pub struct Summary {
    pub samples: u64,
    /// Instructions that share a time stamp with the one before (only the
    /// last value at a time is visible in a VCD).
    pub same_time: u64,
    /// Instructions whose time stamp went backwards (drawn at the previous time).
    pub backwards: u64,
}

struct Signal {
    id: String,
    name: String,
    bits: u32,
}

fn id(k: usize) -> String {
    // Printable identifiers from '!' (33) to '~' (126).
    let mut k = k;
    let mut s = String::new();
    loop {
        s.push((33 + (k % 94)) as u8 as char);
        k /= 94;
        if k == 0 {
            break;
        }
        k -= 1;
    }
    s
}

fn bits(v: u64) -> String {
    format!("b{v:b}")
}

/// Write the VCD. `markers` names the 1-bit marker signals in order.
pub fn write<W: Write>(
    w: &mut W,
    rows: impl Iterator<Item = TrackRow>,
    markers: &[String],
    time: &Time,
    pc_bits: u32,
    comment: &str,
) -> io::Result<Summary> {
    let mut signals = vec![
        Signal { id: id(0), name: "index".into(), bits: 64 },
        Signal { id: id(1), name: "pc".into(), bits: pc_bits },
        Signal { id: id(2), name: "function".into(), bits: 16 },
        Signal { id: id(3), name: "file".into(), bits: 16 },
        Signal { id: id(4), name: "line".into(), bits: 32 },
        Signal { id: id(5), name: "depth".into(), bits: 16 },
        Signal { id: id(6), name: "exception".into(), bits: 16 },
    ];
    for (k, m) in markers.iter().enumerate() {
        let name: String =
            format!("mark_{m}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
        signals.push(Signal { id: id(7 + k), name, bits: 1 });
    }
    let (scale, unit_note) = match time {
        Time::Trace { unit } => (format!("1 {unit}"), format!("trace time in {unit}")),
        Time::Cycles { period, unit } => {
            (format!("1 {unit}"), format!("trace cycles x {period} {unit} (period given with --cycle-period)"))
        }
        Time::Index => ("1 ns".to_string(), "instruction index as time (1 ns per instruction), not real time".into()),
    };
    writeln!(w, "$date simgdb track $end")?;
    writeln!(w, "$version simgdb {} $end", env!("CARGO_PKG_VERSION"))?;
    writeln!(w, "$comment {comment}; time axis: {unit_note}; function and file are ids (see the .names.csv) $end")?;
    writeln!(w, "$timescale {scale} $end")?;
    writeln!(w, "$scope module sw $end")?;
    for s in &signals {
        writeln!(w, "$var wire {} {} {} $end", s.bits, s.id, s.name)?;
    }
    writeln!(w, "$upscope $end")?;
    writeln!(w, "$enddefinitions $end")?;
    let mut last: Vec<Option<u64>> = vec![None; signals.len()];
    let mut summary = Summary { samples: 0, same_time: 0, backwards: 0 };
    let mut now: Option<u64> = None;
    for r in rows {
        let t = match time {
            Time::Trace { .. } => r.time,
            Time::Cycles { period, .. } => r.time.map(|t| t.saturating_mul(*period)),
            Time::Index => Some(r.index),
        };
        let Some(mut t) = t else { continue };
        match now {
            Some(n) if t < n => {
                summary.backwards += 1;
                t = n;
            }
            Some(n) if t == n => summary.same_time += 1,
            _ => {}
        }
        if now != Some(t) {
            if now.is_none() {
                writeln!(w, "#{t}")?;
                writeln!(w, "$dumpvars")?;
            } else {
                writeln!(w, "#{t}")?;
            }
        }
        let values =
            [r.index, r.pc, r.function as u64, r.file as u64, r.line as u64, r.depth as u64, r.exception as u64];
        for (k, s) in signals.iter().enumerate() {
            let v = if k < 7 { values[k] } else { (r.markers >> (k - 7)) & 1 };
            if last[k] != Some(v) {
                if s.bits == 1 {
                    writeln!(w, "{v}{}", s.id)?;
                } else {
                    writeln!(w, "{} {}", bits(v), s.id)?;
                }
                last[k] = Some(v);
            }
        }
        if now.is_none() {
            writeln!(w, "$end")?;
        }
        now = Some(t);
        summary.samples += 1;
    }
    Ok(summary)
}

/// The id tables: `kind,id,name` per line (kind is function or file).
pub fn write_names<W: Write>(w: &mut W, names: &TrackNames) -> io::Result<()> {
    writeln!(w, "kind,id,name")?;
    for (k, n) in names.functions.iter().enumerate().skip(1) {
        writeln!(w, "function,{k},{n}")?;
    }
    for (k, n) in names.files.iter().enumerate().skip(1) {
        writeln!(w, "file,{k},{n}")?;
    }
    Ok(())
}

/// GTKWave translate filter for the `function` signal (hex value -> name).
pub fn write_gtkwave_filter<W: Write>(w: &mut W, names: &TrackNames) -> io::Result<()> {
    for (k, n) in names.functions.iter().enumerate().skip(1) {
        writeln!(w, "{k:04x} {n}")?;
    }
    Ok(())
}
