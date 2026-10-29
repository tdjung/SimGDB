//! SW track as VCD: one signal per piece of software state, changing only
//! when it changes, on the trace's own time axis. Function and file names
//! are text in the VCD itself: a bit vector of 8-bit characters, right
//! aligned like a Verilog string, so a viewer's ASCII radix shows the name.

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
    /// Recorded cycles through the configured time map.
    Map(simgdb_model::TimeMap),
}

pub struct Summary {
    pub samples: u64,
    /// Instructions that share a time stamp with the one before (only the
    /// last value at a time is visible in a VCD).
    pub same_time: u64,
    /// Instructions whose time stamp went backwards (drawn at the previous time).
    pub backwards: u64,
    /// Instructions left out: no time stamp, or before the time map starts.
    pub untimed: u64,
}

struct Signal {
    id: String,
    name: String,
    bits: u32,
    /// Text signal: the value is an index into this table.
    text: Option<Vec<String>>,
}

/// Bits of `s` as a Verilog string: first character in the top byte.
fn text_bits(s: &str) -> String {
    if s.is_empty() {
        return "b0".to_string();
    }
    let mut out = String::with_capacity(1 + 8 * s.len());
    out.push('b');
    for (k, b) in s.bytes().enumerate() {
        let byte = format!("{b:08b}");
        // No leading zeros (VCD zero-extends a shorter value).
        out.push_str(if k == 0 { byte.trim_start_matches('0') } else { &byte });
    }
    out
}

/// File names as short as they can be while still telling the files apart:
/// the last path components, as few as make each one unique (`state.c`,
/// or `uart/init.c` next to `spi/init.c`). Full paths are long and a
/// viewer cuts a value from the right, which would leave only directories.
fn short_paths(files: &[String]) -> Vec<String> {
    let parts: Vec<Vec<&str>> =
        files.iter().map(|f| f.split(['/', '\\']).filter(|p| !p.is_empty()).collect()).collect();
    let tail = |k: usize, n: usize| parts[k][parts[k].len().saturating_sub(n)..].join("/");
    (0..files.len())
        .map(|k| {
            let mut n = 1;
            while n < parts[k].len()
                && (0..files.len()).any(|j| j != k && files[j] != files[k] && tail(j, n) == tail(k, n))
            {
                n += 1;
            }
            if n >= parts[k].len() { files[k].clone() } else { tail(k, n) }
        })
        .collect()
}

/// Width of a text signal holding any of `names` (at least one character).
fn text_width(names: &[String]) -> u32 {
    8 * names.iter().map(|n| n.len()).max().unwrap_or(0).max(1) as u32
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
    names: &TrackNames,
    markers: &[String],
    time: &Time,
    pc_bits: u32,
    comment: &str,
) -> io::Result<Summary> {
    let mut signals = vec![
        Signal { id: id(0), name: "index".into(), bits: 64, text: None },
        Signal { id: id(1), name: "pc".into(), bits: pc_bits, text: None },
        Signal {
            id: id(2),
            name: "function".into(),
            bits: text_width(&names.functions),
            text: Some(names.functions.clone()),
        },
        {
            let files = short_paths(&names.files);
            Signal { id: id(3), name: "file".into(), bits: text_width(&files), text: Some(files) }
        },
        Signal { id: id(4), name: "line".into(), bits: 32, text: None },
        Signal { id: id(5), name: "depth".into(), bits: 32, text: None },
        Signal { id: id(6), name: "exception".into(), bits: 32, text: None },
    ];
    for (k, m) in markers.iter().enumerate() {
        let name: String =
            format!("mark_{m}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
        signals.push(Signal { id: id(7 + k), name, bits: 1, text: None });
    }
    let (scale, unit_note) = match time {
        Time::Trace { unit } => (format!("1 {unit}"), format!("trace time in {unit}")),
        Time::Cycles { period, unit } => {
            (format!("1 {unit}"), format!("trace cycles x {period} {unit} (period given with --cycle-period)"))
        }
        Time::Index => ("1 ns".to_string(), "instruction index as time (1 ns per instruction), not real time".into()),
        Time::Map(m) => {
            (format!("1 {}", m.unit), format!("trace cycles through the time map ({} segments)", m.segments.len()))
        }
    };
    writeln!(w, "$date simgdb track $end")?;
    writeln!(w, "$version simgdb {} $end", env!("CARGO_PKG_VERSION"))?;
    writeln!(
        w,
        "$comment {comment}; time axis: {unit_note}; function and file are text (show them with the ASCII radix; file is the shortest unique path tail), \
         0 = no source; \
         exception 0xffffffff = a handler whose number the trace did not give $end"
    )?;
    writeln!(w, "$timescale {scale} $end")?;
    writeln!(w, "$scope module sw $end")?;
    for s in &signals {
        writeln!(w, "$var wire {} {} {} $end", s.bits, s.id, s.name)?;
    }
    writeln!(w, "$upscope $end")?;
    writeln!(w, "$enddefinitions $end")?;
    let mut last: Vec<Option<u64>> = vec![None; signals.len()];
    let mut summary = Summary { samples: 0, same_time: 0, backwards: 0, untimed: 0 };
    let mut now: Option<u64> = None;
    for r in rows {
        let t = match time {
            Time::Trace { .. } => r.time,
            Time::Cycles { period, .. } => r.time.and_then(|t| t.checked_mul(*period)),
            Time::Index => Some(r.index),
            Time::Map(m) => r.time.and_then(|t| m.time(t)),
        };
        let Some(mut t) = t else {
            summary.untimed += 1;
            continue;
        };
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
                if let Some(table) = &s.text {
                    writeln!(w, "{} {}", text_bits(&table[v as usize]), s.id)?;
                } else if s.bits == 1 {
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

#[cfg(test)]
mod tests {
    #[test]
    fn text_is_a_right_aligned_verilog_string() {
        assert_eq!(super::text_bits("Ab"), "b100000101100010");
        assert_eq!(super::text_bits(""), "b0");
        assert_eq!(super::text_width(&["".into(), "main".into()]), 32);
    }

    #[test]
    fn file_names_keep_only_the_components_that_tell_them_apart() {
        let files: Vec<String> =
            ["", "/w/src/state.c", "/w/drv/uart/init.c", "/w/drv/spi/init.c", "C:\\fw\\boot.s", "main.c"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(super::short_paths(&files), ["", "state.c", "uart/init.c", "spi/init.c", "boot.s", "main.c"]);
    }
}
