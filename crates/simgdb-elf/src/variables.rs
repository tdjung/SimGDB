//! Global and static variables from DWARF (ARCHITECTURE §2.12): name, address
//! and type, so queries can name variables (`last_write("pool[2].value")`,
//! `var("ticks") == 5`) without asking GDB.
//!
//! Only variables at a fixed address (`DW_OP_addr`) are read: globals, file
//! statics and function statics (named `function::name`, as GDB does). Local
//! variables need location lists and are not read here.

use std::collections::HashMap;
use std::path::Path;

use addr2line::gimli;
use gimli::{AttributeValue, DwAt, DwTag, constants as dw};
use object::{Object, ObjectSection};
use serde::{Deserialize, Serialize};

use crate::ElfError;

pub type TypeId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Signed,
    Unsigned,
    Float,
    Bool,
    SignedChar,
    UnsignedChar,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: Option<String>,
    /// Byte offset in the struct; `None` when DWARF gives it in a form not
    /// decoded here (the member can then not be addressed).
    pub offset: Option<u64>,
    #[serde(rename = "type")]
    pub ty: Option<TypeId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bit_size: Option<u64>,
    /// Bit offset from the start of the member's storage unit at `offset`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bit_offset: Option<u64>,
}

/// A C type. `None` as a target means `void`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Type {
    Base {
        name: String,
        size: u64,
        encoding: Encoding,
    },
    Pointer {
        size: u64,
        target: Option<TypeId>,
    },
    /// `count` is `None` for `[]`.
    Array {
        element: Option<TypeId>,
        count: Option<u64>,
    },
    Struct {
        keyword: String,
        name: Option<String>,
        size: u64,
        members: Vec<Member>,
    },
    Enum {
        name: Option<String>,
        size: u64,
        signed: bool,
        values: Vec<(String, i64)>,
    },
    Typedef {
        name: String,
        target: Option<TypeId>,
    },
    Qualified {
        qualifier: String,
        target: Option<TypeId>,
    },
    Function {
        ret: Option<TypeId>,
        params: Vec<Option<TypeId>>,
        varargs: bool,
    },
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    /// `name`, or `function::name` for a function static.
    pub name: String,
    pub addr: u64,
    #[serde(rename = "type")]
    pub ty: Option<TypeId>,
    /// Byte size, when the type has one.
    pub size: Option<u64>,
    /// Visible outside its compile unit.
    pub external: bool,
    /// Compile unit name (to tell file statics of the same name apart).
    pub unit: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variables {
    pub types: Vec<Type>,
    pub variables: Vec<Variable>,
}

/// Where a variable expression lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Place {
    pub addr: u64,
    pub size: Option<u64>,
    #[serde(skip)]
    pub ty: Option<TypeId>,
    #[serde(rename = "type")]
    pub type_name: String,
}

impl Variables {
    /// Resolve `name`, `function::name`, then any of `[index]` and
    /// `.member`. Pointers are not followed (that needs memory at a point).
    pub fn resolve(&self, path: &str) -> Result<Place, String> {
        let path = path.trim();
        let end = path.find(['[', '.']).unwrap_or(path.len());
        let (name, mut rest) = (path[..end].trim(), &path[end..]);
        let mut matches = self.variables.iter().filter(|v| v.name == name);
        let var = matches.next().ok_or_else(|| format!("no variable named {name}"))?;
        if matches.next().is_some() {
            return Err(format!("{name} names several file-static variables"));
        }
        let (mut addr, mut ty) = (var.addr, var.ty);
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix('[') {
                let close = r.find(']').ok_or("missing ]")?;
                let index = parse_index(r[..close].trim())?;
                rest = &r[close + 1..];
                let Some(Type::Array { element, count }) = self.get(self.strip(ty)) else {
                    return Err(format!("{} is not an array", self.name(ty)));
                };
                if count.is_some_and(|c| index >= c) {
                    return Err(format!("index {index} out of bounds [{}]", count.unwrap()));
                }
                let step = index
                    .checked_mul(self.size(*element).ok_or("element size unknown")?)
                    .and_then(|o| addr.checked_add(o))
                    .ok_or_else(|| format!("index {index} is out of the address space"))?;
                addr = step;
                ty = *element;
            } else if let Some(r) = rest.strip_prefix('.') {
                let end = r.find(['[', '.']).unwrap_or(r.len());
                let field = r[..end].trim();
                rest = &r[end..];
                let Some(Type::Struct { members, .. }) = self.get(self.strip(ty)) else {
                    return Err(format!("{} is not a struct or union", self.name(ty)));
                };
                let m = members
                    .iter()
                    .find(|m| m.name.as_deref() == Some(field))
                    .ok_or_else(|| format!("no member named {field}"))?;
                if m.bit_size.is_some() {
                    return Err(format!("{field} is a bit-field"));
                }
                let off =
                    m.offset.ok_or_else(|| format!("the offset of {field} is not known (DWARF form not read)"))?;
                addr = addr.checked_add(off).ok_or("address overflow")?;
                ty = m.ty;
            } else {
                return Err(format!("cannot parse {rest:?}"));
            }
        }
        Ok(Place { addr, size: self.size(ty), ty, type_name: self.name(ty) })
    }

    /// The variable containing `addr`.
    pub fn containing(&self, addr: u64) -> Option<&Variable> {
        self.variables.iter().find(|v| v.size.is_some_and(|s| addr >= v.addr && addr - v.addr < s))
    }

    /// Expression for the scalar holding `addr` (`pool[1].len`), with
    /// `+k` when `addr` is not its first byte.
    pub fn path_at(&self, addr: u64) -> Option<String> {
        let v = self.containing(addr)?;
        let mut path = v.name.clone();
        let (mut base, mut ty) = (v.addr, v.ty);
        for _ in 0..64 {
            match self.get(self.strip(ty)) {
                Some(Type::Array { element, .. }) => {
                    let size = self.size(*element).filter(|&s| s > 0)?;
                    let i = (addr - base) / size;
                    path.push_str(&format!("[{i}]"));
                    base += i * size;
                    ty = *element;
                }
                Some(Type::Struct { members, .. }) => {
                    let off = addr - base;
                    let m = members
                        .iter()
                        .filter(|m| m.bit_size.is_none())
                        .filter_map(|m| m.offset.map(|o| (m, o)))
                        .find(|(m, o)| off >= *o && self.size(m.ty).is_some_and(|s| off - o < s));
                    match m {
                        Some((m, o)) => {
                            path.push('.');
                            path.push_str(m.name.as_deref().unwrap_or("?"));
                            base += o;
                            ty = m.ty;
                        }
                        None => break,
                    }
                }
                _ => break,
            }
        }
        if addr > base {
            path.push_str(&format!("+{}", addr - base));
        }
        Some(path)
    }

    pub fn get(&self, id: Option<TypeId>) -> Option<&Type> {
        id.and_then(|i| self.types.get(i))
    }

    /// Byte size of a type (`None` for void, functions and unknown sizes).
    pub fn size(&self, id: Option<TypeId>) -> Option<u64> {
        match self.get(id)? {
            Type::Base { size, .. }
            | Type::Pointer { size, .. }
            | Type::Struct { size, .. }
            | Type::Enum { size, .. } => Some(*size),
            Type::Array { element, count } => Some(self.size(*element)? * (*count)?),
            Type::Typedef { target, .. } | Type::Qualified { target, .. } => self.size(*target),
            Type::Function { .. } | Type::Unknown => None,
        }
    }

    /// The type with typedefs and qualifiers removed.
    pub fn strip(&self, mut id: Option<TypeId>) -> Option<TypeId> {
        for _ in 0..64 {
            match self.get(id)? {
                Type::Typedef { target, .. } | Type::Qualified { target, .. } => id = *target,
                _ => return id,
            }
        }
        None
    }

    /// C spelling of a type, as GDB's `whatis` prints it.
    pub fn name(&self, id: Option<TypeId>) -> String {
        self.decl(id, String::new(), 0)
    }

    fn decl(&self, id: Option<TypeId>, suffix: String, depth: u32) -> String {
        let join = |base: &str, suffix: &str| {
            if suffix.is_empty() { base.to_string() } else { format!("{base} {suffix}") }
        };
        if depth > 32 {
            return join("?", &suffix);
        }
        let Some(ty) = self.get(id) else { return join("void", &suffix) };
        match ty {
            Type::Base { name, .. } => join(c_spelling(name), &suffix),
            Type::Typedef { name, .. } => join(name, &suffix),
            Type::Struct { keyword, name, .. } => {
                join(&format!("{keyword} {}", name.as_deref().unwrap_or("{...}")), &suffix)
            }
            Type::Enum { name, .. } => join(&format!("enum {}", name.as_deref().unwrap_or("{...}")), &suffix),
            Type::Pointer { target, .. } => {
                let inner = format!("*{suffix}");
                self.decl(*target, inner, depth + 1)
            }
            Type::Array { element, count } => {
                let dim = count.map(|c| format!("[{c}]")).unwrap_or_else(|| "[]".into());
                let inner = if suffix.starts_with('*') { format!("({suffix}){dim}") } else { format!("{suffix}{dim}") };
                self.decl(*element, inner, depth + 1)
            }
            Type::Qualified { qualifier, target } => match self.get(*target) {
                // "char * const": the qualifier goes after the star.
                Some(Type::Pointer { target: t, .. }) => {
                    let inner =
                        if suffix.is_empty() { format!("* {qualifier}") } else { format!("* {qualifier} {suffix}") };
                    self.decl(*t, inner, depth + 1)
                }
                _ => {
                    // GCC qualifies both an array and its elements; say it once.
                    let inner = self.decl(*target, suffix, depth + 1);
                    if inner.starts_with(&format!("{qualifier} ")) { inner } else { format!("{qualifier} {inner}") }
                }
            },
            Type::Function { ret, params, varargs } => {
                let mut p: Vec<String> = params.iter().map(|t| self.name(*t)).collect();
                if *varargs {
                    p.push("...".into());
                }
                if p.is_empty() {
                    p.push("void".into());
                }
                let inner = format!("({suffix})({})", p.join(", "));
                self.decl(*ret, inner, depth + 1)
            }
            Type::Unknown => join("?", &suffix),
        }
    }
}

/// GCC's base type names as C (and GDB) spell them.
fn c_spelling(name: &str) -> &str {
    match name {
        "short int" => "short",
        "short unsigned int" => "unsigned short",
        "long int" => "long",
        "long unsigned int" => "unsigned long",
        "long long int" => "long long",
        "long long unsigned int" => "unsigned long long",
        n => n,
    }
}

fn parse_index(s: &str) -> Result<u64, String> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|_| format!("bad index {s}"))
}

type R<'a> = gimli::EndianSlice<'a, gimli::LittleEndian>;

struct Reader_<'a, 'd> {
    dwarf: &'d gimli::Dwarf<R<'a>>,
    out: Variables,
    ids: HashMap<gimli::DebugInfoOffset, TypeId>,
}

fn string<'a>(dwarf: &gimli::Dwarf<R<'a>>, unit: &gimli::Unit<R<'a>>, v: AttributeValue<R<'a>>) -> Option<String> {
    dwarf.attr_string(unit, v).ok().map(|s| s.to_string_lossy().into_owned())
}

fn udata<'a>(e: &gimli::DebuggingInformationEntry<R<'a>>, at: DwAt) -> Option<u64> {
    e.attr_value(at).ok().flatten().and_then(|v| v.udata_value())
}

fn sdata<'a>(e: &gimli::DebuggingInformationEntry<R<'a>>, at: DwAt) -> Option<i64> {
    match e.attr_value(at).ok().flatten()? {
        AttributeValue::Sdata(v) => Some(v),
        v => v.udata_value().map(|u| u as i64),
    }
}

/// An array bound: older GCC writes -1 (zero-length arrays) as an
/// all-ones 4- or 8-byte constant. (A 1- or 2-byte 0xff.. is a real bound:
/// GCC uses those forms for small arrays.)
fn bound<'a>(e: &gimli::DebuggingInformationEntry<R<'a>>, at: DwAt) -> Option<i64> {
    match e.attr_value(at).ok().flatten()? {
        AttributeValue::Data4(0xffff_ffff) | AttributeValue::Data8(u64::MAX) => Some(-1),
        AttributeValue::Sdata(v) => Some(v),
        v => v.udata_value().map(|u| u as i64),
    }
}

/// `DW_AT_type` (or another reference) as an offset in this unit.
fn reference<'a>(
    unit: &gimli::Unit<R<'a>>,
    e: &gimli::DebuggingInformationEntry<R<'a>>,
    at: DwAt,
) -> Option<gimli::UnitOffset> {
    match e.attr_value(at).ok().flatten()? {
        AttributeValue::UnitRef(o) => Some(o),
        AttributeValue::DebugInfoRef(o) => o.to_unit_offset(&unit.header),
        _ => None,
    }
}

impl<'a> Reader_<'a, '_> {
    fn ty(&mut self, unit: &gimli::Unit<R<'a>>, offset: Option<gimli::UnitOffset>) -> Option<TypeId> {
        let offset = offset?;
        let global = offset.to_debug_info_offset(&unit.header)?;
        if let Some(&id) = self.ids.get(&global) {
            return Some(id);
        }
        let id = self.out.types.len();
        self.out.types.push(Type::Unknown);
        self.ids.insert(global, id);
        let ty = self.read_type(unit, offset).unwrap_or(Type::Unknown);
        self.out.types[id] = ty;
        Some(id)
    }

    fn read_type(&mut self, unit: &gimli::Unit<R<'a>>, offset: gimli::UnitOffset) -> Option<Type> {
        let e = unit.entry(offset).ok()?;
        let name = e.attr_value(dw::DW_AT_name).ok().flatten().and_then(|v| string(self.dwarf, unit, v));
        let size = udata(&e, dw::DW_AT_byte_size);
        let target = reference(unit, &e, dw::DW_AT_type);
        let tag: DwTag = e.tag();
        Some(match tag {
            dw::DW_TAG_base_type => {
                let encoding = match e.attr_value(dw::DW_AT_encoding).ok().flatten() {
                    Some(AttributeValue::Encoding(enc)) => match enc {
                        dw::DW_ATE_signed => Encoding::Signed,
                        dw::DW_ATE_unsigned => Encoding::Unsigned,
                        dw::DW_ATE_float => Encoding::Float,
                        dw::DW_ATE_boolean => Encoding::Bool,
                        dw::DW_ATE_signed_char => Encoding::SignedChar,
                        dw::DW_ATE_unsigned_char => Encoding::UnsignedChar,
                        _ => Encoding::Other,
                    },
                    _ => Encoding::Other,
                };
                Type::Base { name: name.unwrap_or_else(|| "?".into()), size: size.unwrap_or(0), encoding }
            }
            dw::DW_TAG_pointer_type | dw::DW_TAG_reference_type => {
                let size = size.unwrap_or(unit.encoding().address_size as u64);
                Type::Pointer { size, target: self.ty(unit, target) }
            }
            dw::DW_TAG_typedef => {
                Type::Typedef { name: name.unwrap_or_else(|| "?".into()), target: self.ty(unit, target) }
            }
            dw::DW_TAG_const_type | dw::DW_TAG_volatile_type | dw::DW_TAG_restrict_type | dw::DW_TAG_atomic_type => {
                let qualifier = match tag {
                    dw::DW_TAG_const_type => "const",
                    dw::DW_TAG_volatile_type => "volatile",
                    dw::DW_TAG_restrict_type => "restrict",
                    _ => "_Atomic",
                };
                Type::Qualified { qualifier: qualifier.into(), target: self.ty(unit, target) }
            }
            dw::DW_TAG_array_type => {
                let element = self.ty(unit, target);
                let mut counts = Vec::new();
                let mut tree = unit.entries_tree(Some(offset)).ok()?;
                let mut children = tree.root().ok()?.children();
                while let Ok(Some(child)) = children.next() {
                    let c = child.entry();
                    if c.tag() == dw::DW_TAG_subrange_type {
                        let count = udata(c, dw::DW_AT_count).or_else(|| {
                            let upper = bound(c, dw::DW_AT_upper_bound)?;
                            let lower = sdata(c, dw::DW_AT_lower_bound).unwrap_or(0);
                            (upper >= lower - 1).then(|| (upper - lower + 1) as u64)
                        });
                        counts.push(count);
                    }
                }
                // int a[2][3]: an array of 2 arrays of 3.
                let mut ty = Type::Array { element, count: counts.pop().flatten() };
                while let Some(count) = counts.pop() {
                    let id = self.out.types.len();
                    self.out.types.push(ty);
                    ty = Type::Array { element: Some(id), count };
                }
                ty
            }
            dw::DW_TAG_structure_type | dw::DW_TAG_union_type | dw::DW_TAG_class_type => {
                let keyword = match tag {
                    dw::DW_TAG_union_type => "union",
                    dw::DW_TAG_class_type => "class",
                    _ => "struct",
                };
                let mut members = Vec::new();
                let mut tree = unit.entries_tree(Some(offset)).ok()?;
                let mut children = tree.root().ok()?.children();
                let mut pending = Vec::new();
                while let Ok(Some(child)) = children.next() {
                    let c = child.entry();
                    if c.tag() != dw::DW_TAG_member {
                        continue;
                    }
                    let name = c.attr_value(dw::DW_AT_name).ok().flatten().and_then(|v| string(self.dwarf, unit, v));
                    let bit_size = udata(c, dw::DW_AT_bit_size);
                    let data_bit = udata(c, dw::DW_AT_data_bit_offset);
                    let offset = match c.attr_value(dw::DW_AT_data_member_location).ok().flatten() {
                        // DWARF 2/3: a location expression, `DW_OP_plus_uconst n`.
                        Some(AttributeValue::Exprloc(expr)) => {
                            let mut ops = expr.operations(unit.encoding());
                            match (ops.next(), ops.next()) {
                                (Ok(Some(gimli::Operation::PlusConstant { value })), Ok(None)) => Some(value),
                                _ => None,
                            }
                        }
                        Some(v) => v.udata_value(),
                        // Union members (and bit-fields with DW_AT_data_bit_offset).
                        None => Some(data_bit.map(|b| b / 8).unwrap_or(0)),
                    };
                    let bit_offset = match (bit_size, data_bit, offset) {
                        (Some(_), Some(b), Some(o)) => b.checked_sub(o * 8),
                        _ => None,
                    };
                    pending.push((name, offset, reference(unit, c, dw::DW_AT_type), bit_size, bit_offset));
                }
                for (name, offset, ty, bit_size, bit_offset) in pending {
                    members.push(Member { name, offset, ty: self.ty(unit, ty), bit_size, bit_offset });
                }
                Type::Struct { keyword: keyword.into(), name, size: size.unwrap_or(0), members }
            }
            dw::DW_TAG_enumeration_type => {
                let mut values = Vec::new();
                let mut tree = unit.entries_tree(Some(offset)).ok()?;
                let mut children = tree.root().ok()?.children();
                while let Ok(Some(child)) = children.next() {
                    let c = child.entry();
                    if c.tag() == dw::DW_TAG_enumerator {
                        let n = c.attr_value(dw::DW_AT_name).ok().flatten().and_then(|v| string(self.dwarf, unit, v));
                        if let (Some(n), Some(v)) = (n, sdata(c, dw::DW_AT_const_value)) {
                            values.push((n, v));
                        }
                    }
                }
                let signed = values.iter().any(|(_, v)| *v < 0);
                Type::Enum { name, size: size.unwrap_or(4), signed, values }
            }
            dw::DW_TAG_subroutine_type => {
                let ret = self.ty(unit, target);
                let mut params = Vec::new();
                let mut varargs = false;
                let mut refs = Vec::new();
                let mut tree = unit.entries_tree(Some(offset)).ok()?;
                let mut children = tree.root().ok()?.children();
                while let Ok(Some(child)) = children.next() {
                    let c = child.entry();
                    match c.tag() {
                        dw::DW_TAG_formal_parameter => refs.push(reference(unit, c, dw::DW_AT_type)),
                        dw::DW_TAG_unspecified_parameters => varargs = true,
                        _ => {}
                    }
                }
                for r in refs {
                    params.push(self.ty(unit, r));
                }
                Type::Function { ret, params, varargs }
            }
            _ => Type::Unknown,
        })
    }

    /// Address of a `DW_OP_addr` location.
    fn address(&self, unit: &gimli::Unit<R<'a>>, e: &gimli::DebuggingInformationEntry<R<'a>>) -> Option<u64> {
        let AttributeValue::Exprloc(expr) = e.attr_value(dw::DW_AT_location).ok().flatten()? else { return None };
        let mut ops = expr.operations(unit.encoding());
        let first = ops.next().ok()??;
        if ops.next().ok()?.is_some() {
            return None;
        }
        match first {
            gimli::Operation::Address { address } => Some(address),
            gimli::Operation::AddressIndex { index } => self.dwarf.address(unit, index).ok(),
            _ => None,
        }
    }

    fn unit(&mut self, unit: &gimli::Unit<R<'a>>) {
        let unit_name = unit.name.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut entries = unit.entries();
        // Enclosing subprogram names by depth.
        let mut scopes: Vec<(isize, Option<String>)> = Vec::new();
        let mut depth = 0isize;
        while let Ok(Some((delta, e))) = entries.next_dfs() {
            depth += delta;
            while scopes.last().is_some_and(|(d, _)| *d >= depth) {
                scopes.pop();
            }
            match e.tag() {
                dw::DW_TAG_subprogram => {
                    let name = self.name_of(unit, e);
                    scopes.push((depth, name));
                }
                dw::DW_TAG_variable => {
                    if matches!(e.attr_value(dw::DW_AT_declaration).ok().flatten(), Some(AttributeValue::Flag(true))) {
                        continue;
                    }
                    let Some(addr) = self.address(unit, e) else { continue };
                    let Some(name) = self.name_of(unit, e) else { continue };
                    let name = match scopes.iter().rev().find_map(|(_, n)| n.as_ref()) {
                        Some(f) => format!("{f}::{name}"),
                        None => name,
                    };
                    let decl = reference(unit, e, dw::DW_AT_specification);
                    let ty_ref = reference(unit, e, dw::DW_AT_type).or_else(|| {
                        decl.and_then(|d| unit.entry(d).ok()).and_then(|d| reference(unit, &d, dw::DW_AT_type))
                    });
                    let external =
                        matches!(e.attr_value(dw::DW_AT_external).ok().flatten(), Some(AttributeValue::Flag(true)))
                            || decl.and_then(|d| unit.entry(d).ok()).is_some_and(|d| {
                                matches!(
                                    d.attr_value(dw::DW_AT_external).ok().flatten(),
                                    Some(AttributeValue::Flag(true))
                                )
                            });
                    let ty = self.ty(unit, ty_ref);
                    let size = self.out.size(ty);
                    self.out.variables.push(Variable { name, addr, ty, size, external, unit: unit_name.clone() });
                }
                _ => {}
            }
        }
    }

    /// `DW_AT_name`, through `DW_AT_specification` / `DW_AT_abstract_origin`.
    fn name_of(&self, unit: &gimli::Unit<R<'a>>, e: &gimli::DebuggingInformationEntry<R<'a>>) -> Option<String> {
        if let Some(v) = e.attr_value(dw::DW_AT_name).ok().flatten() {
            return string(self.dwarf, unit, v);
        }
        for at in [dw::DW_AT_specification, dw::DW_AT_abstract_origin] {
            if let Some(o) = reference(unit, e, at) {
                let d = unit.entry(o).ok()?;
                if let Some(v) = d.attr_value(dw::DW_AT_name).ok().flatten() {
                    return string(self.dwarf, unit, v);
                }
            }
        }
        None
    }
}

/// Every fixed-address variable of the ELF, sorted by name then address.
pub fn variables(path: &Path) -> Result<Variables, ElfError> {
    let bytes = std::fs::read(path).map_err(|e| ElfError::Io(path.display().to_string(), e))?;
    let file = object::File::parse(&*bytes)?;
    let sections: HashMap<&str, std::borrow::Cow<[u8]>> =
        file.sections().filter_map(|s| Some((s.name().ok()?, s.uncompressed_data().ok()?))).collect();
    let empty: &[u8] = &[];
    let load = |id: gimli::SectionId| -> Result<R<'_>, gimli::Error> {
        let data = sections.get(id.name()).map(|d| d.as_ref()).unwrap_or(empty);
        Ok(gimli::EndianSlice::new(data, gimli::LittleEndian))
    };
    let dwarf = gimli::Dwarf::load(load).map_err(|e| ElfError::Dwarf(e.to_string()))?;
    let mut r = Reader_ { dwarf: &dwarf, out: Variables::default(), ids: HashMap::new() };
    let mut units = dwarf.units();
    while let Some(header) = units.next().map_err(|e| ElfError::Dwarf(e.to_string()))? {
        let unit = dwarf.unit(header).map_err(|e| ElfError::Dwarf(e.to_string()))?;
        r.unit(&unit);
    }
    r.out.variables.sort_by(|a, b| (&a.name, a.addr).cmp(&(&b.name, b.addr)));
    r.out.variables.dedup_by(|a, b| a.name == b.name && a.addr == b.addr);
    Ok(r.out)
}
