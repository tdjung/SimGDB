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
