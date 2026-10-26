//! Variables by name (ARCHITECTURE §2.12): where they live and their value at
//! a point, from the engine's own DWARF reading.

use serde::Serialize;
use simgdb_elf::variables::{Encoding, Place, Type};
use simgdb_model::{Evidence, Point, Source, Value};

use crate::{Engine, EngineError};

/// A variable's value at a point.
#[derive(Clone, Debug, Serialize)]
pub struct VariableValue {
    pub point: Point,
    pub path: String,
    #[serde(flatten)]
    pub place: Place,
    /// Raw value (little- or big-endian per the ISA), when the type is a
    /// scalar of at most 8 bytes. Partially known values carry `known_mask`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value<u64>>,
    /// Decoded scalar (signed, unsigned, float, pointer, bool, enum name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded: Option<serde_json::Value>,
    /// Bytes, `??` where unknown.
    pub hex: String,
}

impl Engine {
    pub fn variables(&self) -> &simgdb_elf::variables::Variables {
        &self.ds.variables
    }

    /// Where `path` (`name`, `name[i]`, `name.member`, ...) lives.
    pub fn variable(&self, path: &str) -> Result<Place, EngineError> {
        self.ds.variables.resolve(path).map_err(EngineError::Variable)
    }

    /// Bytes as one value: known when every byte is known, partially known
    /// (with `known_mask`) when some are. The source is the least certain
    /// of the bytes' sources; evidence lists each distinct record.
    pub fn combine(&self, bytes: &[Value<u8>]) -> Value<u64> {
        if bytes.is_empty() || bytes.len() > 8 {
            return Value::unknown();
        }
        let little = self.isa.little_endian();
        let (mut value, mut mask) = (0u64, 0u64);
        let mut source = Source::Trace;
        let mut evidence: Vec<Evidence> = Vec::new();
        let rank = |s: Source| match s {
            Source::Trace => 0,
            Source::Waveform => 1,
            Source::ElfInitial => 2,
            Source::Inferred => 3,
            Source::Unknown => 4,
        };
        for (k, b) in bytes.iter().enumerate() {
            let shift = 8 * if little { k } else { bytes.len() - 1 - k };
            if let Some(v) = b.value {
                value |= (v as u64) << shift;
                mask |= 0xff << shift;
                if rank(b.source) > rank(source) {
                    source = b.source;
                }
            }
            for e in &b.evidence {
                if !evidence.contains(e) {
                    evidence.push(e.clone());
                }
            }
        }
        if mask == 0 {
            return Value { evidence, ..Value::unknown() };
        }
        let full = if bytes.len() == 8 { u64::MAX } else { (1u64 << (8 * bytes.len())) - 1 };
        Value { value: Some(value), source, known_mask: (mask != full).then_some(mask), evidence, conflicts: vec![] }
    }

    /// Value of a variable expression at `index`.
    pub fn read_variable(&self, path: &str, index: u64) -> Result<VariableValue, EngineError> {
        let place = self.variable(path)?;
        let size = place.size.ok_or_else(|| EngineError::Variable(format!("{path} has no known size")))?;
        let bytes = self.memory(place.addr, size, index)?;
        let hex = bytes.iter().map(|b| b.value.map(|v| format!("{v:02x}")).unwrap_or_else(|| "??".into())).collect();
        let (raw, decoded) = if size <= 8 {
            let raw = self.combine(&bytes);
            let decoded = match (raw.value, raw.known_mask) {
                (Some(v), None) => self.decode(place.ty, v, size),
                _ => None,
            };
            (Some(raw), decoded)
        } else {
            (None, None)
        };
        Ok(VariableValue { point: self.point(index)?, path: path.to_string(), place, raw, decoded, hex })
    }

    fn decode(&self, ty: Option<usize>, v: u64, size: u64) -> Option<serde_json::Value> {
        let vars = &self.ds.variables;
        let sign = |v: u64| {
            let bits = 8 * size as u32;
            if bits >= 64 { v as i64 } else { ((v << (64 - bits)) as i64) >> (64 - bits) }
        };
        Some(match vars.get(vars.strip(ty))? {
            Type::Base { encoding, .. } => match encoding {
                Encoding::Signed | Encoding::SignedChar => sign(v).into(),
                Encoding::Unsigned | Encoding::UnsignedChar | Encoding::Other => v.into(),
                Encoding::Bool => (v != 0).into(),
                Encoding::Float if size == 4 => (f32::from_bits(v as u32) as f64).into(),
                Encoding::Float if size == 8 => f64::from_bits(v).into(),
                Encoding::Float => return None,
            },
            Type::Pointer { .. } => format!("{v:#x}").into(),
            Type::Enum { values, signed, .. } => {
                let n = if *signed { sign(v) } else { v as i64 };
                match values.iter().find(|(_, x)| *x == n) {
                    Some((name, _)) => name.clone().into(),
                    None => n.into(),
                }
            }
            _ => return None,
        })
    }

    /// Integer value of a scalar variable at `index` (for conditions):
    /// signed types sign-extended. `None` when not fully known.
    pub(crate) fn variable_int(&self, place: &Place, index: u64) -> Result<Option<i128>, EngineError> {
        let size = place.size.unwrap_or(0);
        if size == 0 || size > 8 {
            return Err(EngineError::Variable(format!("{} is not a scalar", place.type_name)));
        }
        let raw = self.combine(&self.memory(place.addr, size, index)?);
        let (Some(v), None) = (raw.value, raw.known_mask) else { return Ok(None) };
        let vars = &self.ds.variables;
        let signed = match vars.get(vars.strip(place.ty)) {
            Some(Type::Base { encoding: Encoding::Signed | Encoding::SignedChar, .. }) => true,
            Some(Type::Enum { signed, .. }) => *signed,
            Some(Type::Base { encoding: Encoding::Float, .. }) => {
                return Err(EngineError::Variable(format!("{} is a float", place.type_name)));
            }
            _ => false,
        };
        let bits = 8 * size as u32;
        Ok(Some(if signed && bits < 64 {
            (((v << (64 - bits)) as i64) >> (64 - bits)) as i128
        } else if signed {
            v as i64 as i128
        } else {
            v as i128
        }))
    }
}
