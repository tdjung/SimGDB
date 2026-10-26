//! Points in time and values with provenance (ARCHITECTURE §2.1, §2.3).
//!
//! Every value SimGDB reports carries where it came from. A value nobody
//! recorded is `Source::Unknown`, never a guess presented as fact.

use serde::{Deserialize, Serialize};

/// Unit of a recorded time stamp, exactly as the trace wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum TimeUnit {
    /// A time unit; the payload is its name (`ps`, `ns`, `us`, `ms`, `s`).
    Time(String),
    /// A clock count (`tic`, `cyc`, `clk`, `cs`, ...). Converting it to time
    /// needs a time map (D16).
    Cycle(String),
    /// The trace has no time stamps.
    None,
}

impl TimeUnit {
    pub fn from_token(token: &str) -> Option<TimeUnit> {
        match token {
            "fs" | "ps" | "ns" | "us" | "ms" | "s" => Some(TimeUnit::Time(token.to_string())),
            "tic" | "cyc" | "clk" | "cs" | "cycles" => Some(TimeUnit::Cycle(token.to_string())),
            _ => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TimeUnit::Time(n) | TimeUnit::Cycle(n) => n,
            TimeUnit::None => "",
        }
    }
}

/// A recorded time stamp: the value as written plus its unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimTime {
    pub value: u64,
    pub unit: TimeUnit,
}

/// A point in an execution: `(run, core, index, sim_time)`.
///
/// `index = i` is the state just before the i-th instruction record of the
/// core executes (where GDB stops at `pc_i`). With N records, `index = N` is
/// the state after the last one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub run: u32,
    pub core: u32,
    pub index: u64,
    /// Time stamp of instruction `index`; `None` at the end point or when the
    /// trace has no time stamps.
    pub sim_time: Option<SimTime>,
    /// Absolute time derived from a cycle stamp through the configured time
    /// map (D16); absent when there is no map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<DerivedTime>,
}

/// A time computed from a cycle count with the time map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedTime {
    pub value: u64,
    pub unit: String,
    /// Always `time_map`: says the value is computed, not recorded.
    pub source: String,
}

/// Piecewise map from cycle counts to time (frequency changes, clock
/// gating): each segment holds from its `from_cycle` until the next one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeMap {
    /// Time unit of `time` and `period` (`fs`, `ps`, `ns`, `us`, `ms`, `s`).
    pub unit: String,
    pub segments: Vec<TimeSegment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSegment {
    pub from_cycle: u64,
    /// Time at `from_cycle`.
    pub time: u64,
    /// Time per cycle from here on.
    pub period: u64,
}

impl TimeMap {
    /// Checks the map is usable: a known unit and segments in cycle order.
    pub fn validate(&self) -> Result<(), String> {
        if !["fs", "ps", "ns", "us", "ms", "s"].contains(&self.unit.as_str()) {
            return Err(format!("time map unit must be fs, ps, ns, us, ms or s, not {:?}", self.unit));
        }
        if self.segments.is_empty() {
            return Err("time map has no segments".into());
        }
        if self.segments.windows(2).any(|w| w[0].from_cycle >= w[1].from_cycle) {
            return Err("time map segments must be in increasing from_cycle order".into());
        }
        Ok(())
    }

    /// Time of `cycle`, or `None` before the first segment.
    pub fn time(&self, cycle: u64) -> Option<u64> {
        let k = self.segments.partition_point(|s| s.from_cycle <= cycle);
        let s = self.segments.get(k.checked_sub(1)?)?;
        Some(s.time + (cycle - s.from_cycle) * s.period)
    }
}

/// Where a value came from (D5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Written or observed in the trace.
    Trace,
    /// Read from a waveform (Step 2).
    Waveform,
    /// The ELF load image, untouched by any recorded access.
    ElfInitial,
    /// Deduced, not recorded at this point (e.g. a later read of memory with
    /// no write in between). Evidence says how.
    Inferred,
    /// Nobody recorded it.
    Unknown,
}

/// The record a value was taken from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Instruction index whose record produced the value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    /// 1-based line number in the trace file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Short explanation, e.g. "exception stacking may have overwritten".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A value with provenance. `value` is `None` exactly when `source` is
/// `Unknown`; `known_mask` marks partially known values (bit set = known).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value<T> {
    pub value: Option<T>,
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_mask: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence: Vec<Evidence>,
    /// Other values for the same thing from other sources (Step 2).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub conflicts: Vec<Conflict<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict<T> {
    pub value: T,
    pub source: Source,
    pub evidence: Evidence,
}

impl<T> Value<T> {
    pub fn known(value: T, source: Source, evidence: Evidence) -> Self {
        Value { value: Some(value), source, known_mask: None, evidence: vec![evidence], conflicts: vec![] }
    }

    pub fn unknown() -> Self {
        Value { value: None, source: Source::Unknown, known_mask: None, evidence: vec![], conflicts: vec![] }
    }

    pub fn unknown_because(evidence: Evidence) -> Self {
        Value { value: None, source: Source::Unknown, known_mask: None, evidence: vec![evidence], conflicts: vec![] }
    }

    /// Fully known: present and not partial.
    pub fn is_known(&self) -> bool {
        self.value.is_some() && self.known_mask.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_units() {
        assert_eq!(TimeUnit::from_token("ps"), Some(TimeUnit::Time("ps".into())));
        assert_eq!(TimeUnit::from_token("tic"), Some(TimeUnit::Cycle("tic".into())));
        assert_eq!(TimeUnit::from_token("IT"), None);
    }

    #[test]
    fn unknown_has_no_value() {
        let v: Value<u32> = Value::unknown();
        assert!(!v.is_known());
        assert_eq!(v.source, Source::Unknown);
    }
}
