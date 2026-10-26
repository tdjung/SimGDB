//! Variable addresses, sizes and types against gdb's DWARF reader
//! (tests/variables/expected, made by tests/variables/make_expected.py), and
//! variable values against RAM read by gdb at the truth points.

use std::io::Read;
use std::path::PathBuf;

use serde_json::Value as Json;
use simgdb_engine::Engine;
use simgdb_store::{IndexOptions, build};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests")
}

fn index(core: &str, dir: &std::path::Path) -> Engine {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: root().join(format!("firmware/traces/state_{core}.fm.log.gz")),
            elf: Some(root().join(format!("firmware/state_{core}.elf"))),
            out: dir.to_path_buf(),
            ..Default::default()
        },
    )
    .unwrap();
    Engine::open(dir).unwrap()
}

#[test]
fn places_match_gdb() {
    for core in ["m4", "m35p"] {
        let dir = tempfile::tempdir().unwrap();
        let engine = index(core, dir.path());
        let text = std::fs::read_to_string(root().join(format!("variables/expected/state_{core}.json"))).unwrap();
        let expected: serde_json::Map<String, Json> = serde_json::from_str(&text).unwrap();
        for (path, want) in &expected {
            let place = engine.variable(path).unwrap_or_else(|e| panic!("{core} {path}: {e}"));
            assert_eq!(place.addr, want["addr"].as_u64().unwrap(), "{core} {path}: address");
            assert_eq!(place.size, want["size"].as_u64(), "{core} {path}: size");
            assert_eq!(place.type_name, want["type"].as_str().unwrap(), "{core} {path}: type");
        }
        assert!(engine.variable("ring[8]").is_err(), "bounds are checked");
        assert!(engine.variable("pool[0].nope").is_err());
        assert!(engine.variable("nope").is_err());
    }
}

#[test]
fn values_match_gdb_memory() {
    for core in ["m4", "m35p"] {
        let dir = tempfile::tempdir().unwrap();
        let engine = index(core, dir.path());
        let mut text = String::new();
        let path = root().join(format!("firmware/traces/state_{core}.truth.json.gz"));
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut text).unwrap();
        let truth: Json = serde_json::from_str(&text).unwrap();
        let (mut known, mut unknown) = (0, 0);
        for p in truth["points"].as_array().unwrap() {
            let index = p["index"].as_u64().unwrap();
            for name in [
                "seed",
                "ticks",
                "svc_calls",
                "ring[3]",
                "head",
                "wide",
                "pool[2].value",
                "pool[4].stamp",
                "bytes[23]",
                "result",
                "primes[5]",
            ] {
                let v = engine.read_variable(name, index).unwrap();
                // gdb's bytes for the variable, from the region that holds it.
                let want = p["memory"].as_object().unwrap().iter().find_map(|(start, hex)| {
                    let start = u64::from_str_radix(start, 16).unwrap();
                    let hex = hex.as_str().unwrap();
                    let off = v.place.addr.checked_sub(start)? as usize;
                    let end = off + v.place.size.unwrap() as usize;
                    (2 * end <= hex.len()).then(|| hex[2 * off..2 * end].to_string())
                });
                let want = want.unwrap_or_else(|| panic!("{name} is outside the truth regions"));
                match &v.raw {
                    Some(raw) if raw.known_mask.is_none() && raw.value.is_some() => {
                        known += 1;
                        assert_eq!(v.hex, want, "{core} index {index}: {name}");
                        let le = (0..want.len() / 2).rev().map(|k| &want[2 * k..2 * k + 2]).collect::<String>();
                        assert_eq!(raw.value.unwrap(), u64::from_str_radix(&le, 16).unwrap(), "{core} {index} {name}");
                    }
                    _ => unknown += 1,
                }
            }
        }
        eprintln!("{core}: {known} variable values equal gdb's, {unknown} unknown");
        assert!(known > unknown);
    }
}
