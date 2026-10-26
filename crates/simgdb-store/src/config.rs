//! Site configuration (ARCHITECTURE §2.14): what can differ between sites
//! and releases without rebuilding SimGDB, in a TOML file given to
//! `simgdb index --config`. A copy is kept in the index metadata.
//!
//! ```toml
//! [tarmac]
//! record_aliases = { XW4 = "MW4" }   # a site keyword read as a known record type
//! ignore = ["CSR"]                    # record types with no effect on state
//!
//! [registers]
//! aliases = { gpr4 = "r4", sp_main = "msp" }   # trace name -> ISA register
//!
//! [memory]
//! volatile = [[0x40000000, 0x60000000]]        # replaces the ISA's peripheral regions
//!
//! [time]                                        # cycle -> time, for traces timed in cycles
//! unit = "ns"
//! segments = [{ from_cycle = 0, time = 0, period = 10 }]
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use simgdb_isa::{Isa, TraceRegName};
use simgdb_model::TimeMap;

use crate::StoreError;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteConfig {
    pub tarmac: TarmacConfig,
    pub registers: RegistersConfig,
    pub memory: MemoryConfig,
    pub time: Option<TimeMap>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TarmacConfig {
    pub record_aliases: BTreeMap<String, String>,
    pub ignore: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RegistersConfig {
    /// Trace register name (case-insensitive) -> ISA register name.
    pub aliases: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryConfig {
    /// `[start, end)` ranges; replaces the ISA default when given.
    pub volatile: Option<Vec<[u64; 2]>>,
}

impl SiteConfig {
    pub fn load(path: &Path) -> Result<SiteConfig, StoreError> {
        let text = std::fs::read_to_string(path).map_err(|e| StoreError::io(path, e))?;
        SiteConfig::parse(&text).map_err(|e| StoreError::Trace(format!("{}: {e}", path.display())))
    }

    pub fn parse(text: &str) -> Result<SiteConfig, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Check names and ranges against the ISA.
    pub fn validate(&self, isa: &dyn Isa) -> Result<(), String> {
        for (from, to) in &self.registers.aliases {
            if isa.lookup(&TraceRegName { name: to, bank: None }).is_none() {
                let known: Vec<&str> = isa.registers().iter().map(|r| r.name).collect();
                return Err(format!(
                    "register alias {from} -> {to}: {to} is not a {} register ({})",
                    isa.name(),
                    known.join(", ")
                ));
            }
        }
        if let Some(v) = &self.memory.volatile
            && let Some(r) = v.iter().find(|r| r[0] >= r[1])
        {
            return Err(format!("memory.volatile range {:#x}..{:#x} is empty", r[0], r[1]));
        }
        if let Some(t) = &self.time {
            t.validate()?;
        }
        Ok(())
    }

    /// Tarmac notation for the parser.
    pub fn notation(&self) -> simgdb_tarmac::Notation {
        simgdb_tarmac::Notation {
            record_aliases: self.tarmac.record_aliases.clone().into_iter().collect(),
            ignore: self.tarmac.ignore.iter().cloned().collect(),
        }
    }

    /// ISA register name for a trace name, if aliased.
    pub fn register_alias(&self, name: &str) -> Option<&str> {
        self.registers.aliases.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects_typos() {
        let c = SiteConfig::parse(
            r#"
            [tarmac]
            record_aliases = { XW4 = "MW4" }
            ignore = ["CSR"]
            [registers]
            aliases = { GPR4 = "r4" }
            [memory]
            volatile = [[0x40000000, 0x60000000], [0xE0000000, 0x100000000]]
            [time]
            unit = "ns"
            segments = [{ from_cycle = 0, time = 0, period = 10 }, { from_cycle = 100, time = 1000, period = 20 }]
            "#,
        )
        .unwrap();
        let isa = simgdb_isa::by_name("arm-m").unwrap();
        c.validate(isa.as_ref()).unwrap();
        assert_eq!(c.register_alias("gpr4"), Some("r4"));
        assert_eq!(c.memory.volatile.as_ref().unwrap()[1], [0xE000_0000, 0x1_0000_0000]);
        let t = c.time.as_ref().unwrap();
        assert_eq!((t.time(0), t.time(99), t.time(100), t.time(105)), (Some(0), Some(990), Some(1000), Some(1100)));
        assert!(SiteConfig::parse("[registers]\nalias = {}").is_err(), "unknown keys are errors");
        let bad = SiteConfig::parse("[registers]\naliases = { x = \"r99\" }").unwrap();
        assert!(bad.validate(isa.as_ref()).unwrap_err().contains("r99"));
        let bad = SiteConfig::parse("[time]\nunit = \"ns\"\nsegments = []").unwrap();
        assert!(bad.validate(isa.as_ref()).is_err());
    }

    #[test]
    fn the_shipped_example_is_valid() {
        let c = SiteConfig::parse(include_str!("../../../dist/simgdb.toml.example")).unwrap();
        c.validate(simgdb_isa::by_name("arm-m").unwrap().as_ref()).unwrap();
        assert!(c.time.is_some() && !c.registers.aliases.is_empty());
    }
}
