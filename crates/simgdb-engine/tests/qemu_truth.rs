//! End-to-end check of state reconstruction against independent ground truth.
//!
//! The traces and truth come from tests/firmware (see its README): QEMU runs
//! the firmware once with a tracing plugin (rendered as three Tarmac
//! dialects), and gdb reads registers and RAM through QEMU's gdbstub while
//! replaying the same run. Every value SimGDB claims to know must equal what
//! gdb read; unknown values are counted, not compared.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use simgdb_engine::Engine;
use simgdb_model::Source;
use simgdb_store::{IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

fn truth(core: &str) -> serde_json::Value {
    let path = firmware().join(format!("traces/state_{core}.truth.json.gz"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[derive(Default, Debug)]
struct Tally {
    reg_known: u64,
    reg_unknown: u64,
    mem: BTreeMap<String, u64>,
    mismatches: Vec<String>,
}

fn check(core: &str, dialect: &str) -> Tally {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join(format!("traces/state_{core}.{dialect}.log.gz")),
            elf: Some(firmware().join(format!("state_{core}.elf"))),
            out: dir.path().to_path_buf(),
            checkpoint_interval: 256,
            ..Default::default()
        },
    )
    .unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    let t = truth(core);
    assert_eq!(engine.end(), t["instructions"].as_u64().unwrap(), "{core} {dialect}: instruction count");
    let mut tally = Tally::default();
    for p in t["points"].as_array().unwrap() {
        let index = p["index"].as_u64().unwrap();
        let pc = engine.pc(index).unwrap();
        if pc.value != Some(p["pc"].as_u64().unwrap()) {
            tally.mismatches.push(format!("index {index}: pc {:?} vs {:#x}", pc.value, p["pc"].as_u64().unwrap()));
        }
        for (name, want) in p["registers"].as_object().unwrap() {
            let reg = engine.register_id(name).unwrap();
            let v = engine.register(index, reg).unwrap();
            match v.value {
                Some(got) if v.known_mask.is_none() => {
                    tally.reg_known += 1;
                    let want = want.as_u64().unwrap();
                    if got != want {
                        tally
                            .mismatches
                            .push(format!("index {index}: {name} = {got:#x}, gdb {want:#x} ({:?})", v.evidence));
                    }
                }
                _ => tally.reg_unknown += 1,
            }
        }
        for (start, hex) in p["memory"].as_object().unwrap() {
            let base = u64::from_str_radix(start, 16).unwrap();
            let hex = hex.as_str().unwrap();
            let bytes: Vec<u8> =
                (0..hex.len() / 2).map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).unwrap()).collect();
            let got = engine.memory(base, bytes.len() as u64, index).unwrap();
            for (k, (v, want)) in got.iter().zip(&bytes).enumerate() {
                *tally.mem.entry(format!("{:?}", v.source)).or_default() += 1;
                match v.value {
                    Some(b) if b != *want => tally.mismatches.push(format!(
                        "index {index}: [{:#x}] = {b:#04x} ({:?}), gdb {want:#04x}: {:?}",
                        base + k as u64,
                        v.source,
                        v.evidence
                    )),
                    Some(_) => {}
                    None => assert_eq!(v.source, Source::Unknown),
                }
            }
        }
    }
    tally
}

#[test]
fn reconstruction_matches_gdbstub() {
    let mut failures = Vec::new();
    println!(
        "| core | trace dialect | registers known | memory bytes: trace / elf_initial / inferred / unknown | mismatches |"
    );
    println!("|---|---|---:|---|---:|");
    for core in ["m4", "m35p"] {
        for dialect in ["fm", "m4rtl", "es"] {
            let t = check(core, dialect);
            let total = t.reg_known + t.reg_unknown;
            let m = |k: &str| t.mem.get(k).copied().unwrap_or(0);
            println!(
                "| {core} | {dialect} | {}/{} ({:.0}%) | {} / {} / {} / {} | {} |",
                t.reg_known,
                total,
                100.0 * t.reg_known as f64 / total as f64,
                m("Trace"),
                m("ElfInitial"),
                m("Inferred"),
                m("Unknown"),
                t.mismatches.len()
            );
            for mm in t.mismatches.iter().take(10) {
                failures.push(format!("{core} {dialect}: {mm}"));
            }
            assert!(t.reg_known * 10 >= total * 7, "{core} {dialect}: too few registers known");
        }
    }
    assert!(failures.is_empty(), "values differ from gdb:\n{}", failures.join("\n"));
}
