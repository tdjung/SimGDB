//! Variable layouts read from DWARF 2, 4 and 5 (tests/variables/elf, built
//! from src/layout.c) against gdb's answers (tests/variables/expected):
//! struct members given as DWARF 2/3 location expressions, unions,
//! multi-dimensional arrays, a 256-element array (bound in a 1-byte form),
//! function statics.

use std::path::PathBuf;

use serde_json::Value;
use simgdb_elf::variables::variables;

#[test]
fn layouts_match_gdb_for_each_dwarf_version() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/variables");
    for v in [2, 4, 5] {
        let vars = variables(&root.join(format!("elf/layout_dwarf{v}.elf"))).unwrap();
        let text = std::fs::read_to_string(root.join(format!("expected/layout_dwarf{v}.json"))).unwrap();
        let want: serde_json::Map<String, Value> = serde_json::from_str(&text).unwrap();
        for (path, w) in &want {
            let p = vars.resolve(path).unwrap_or_else(|e| panic!("DWARF {v} {path}: {e}"));
            assert_eq!(p.addr, w["addr"].as_u64().unwrap(), "DWARF {v} {path}: address");
            assert_eq!(p.size, w["size"].as_u64(), "DWARF {v} {path}: size");
            assert_eq!(p.type_name, w["type"].as_str().unwrap(), "DWARF {v} {path}: type");
            // And back: the address names the same element or member.
            let ty = w["type"].as_str().unwrap();
            let scalar = !ty.contains('[') && !ty.starts_with("struct") && !ty.starts_with("union");
            // (Inside a union several names fit the same byte.)
            if !path.contains("::") && !path.starts_with("gu.") && scalar {
                assert_eq!(vars.path_at(p.addr).as_deref(), Some(path.as_str()), "DWARF {v}");
            }
        }
        assert!(vars.resolve("big[256]").is_err());
        assert!(vars.resolve("gs.in[0x4000000000000000]").is_err(), "no wrap-around");
    }
}

/// Damaged or unsupported debug information must not stop indexing: what
/// can be read is used and the problems are reported. (damaged_dwarf.elf is
/// layout_dwarf4.elf with a compile unit header that runs past the end of
/// .debug_info, which made `simgdb index` fail with "Hit the end of input".)
#[test]
fn damaged_debug_information_is_reported_not_fatal() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/variables");
    let elf = root.join("elf/damaged_dwarf.elf");
    let (_, problems) = simgdb_elf::variables::variables_with_problems(&elf).unwrap();
    assert!(problems.iter().any(|p| p.contains("end of input")), "{problems:?}");
    let (sources, problems) = simgdb_elf::source::attribute_with_problems(&elf).unwrap();
    assert!(!problems.is_empty());
    // Functions still come from the symbol table.
    assert!(sources.values().any(|a| a.function == "use"), "{:?}", sources.values().next());
}
