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
    /// Floating-point registers (S0-S31, FPSCR) among `reg_known` and
    /// `reg_unknown`. Most are never written by the firmware, so they stay
    /// unknown (their reset value is UNKNOWN; QEMU happens to hold 0).
    fp_known: u64,
    fp_unknown: u64,
    unknown_by_register: BTreeMap<String, u64>,
    mem: BTreeMap<String, u64>,
    mismatches: Vec<String>,
}

fn check(core: &str, dialect: &str) -> Tally {
    check_with(core, dialect, None)
}

fn check_with(core: &str, dialect: &str, endian: Option<simgdb_isa::Endian>) -> Tally {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join(format!("traces/state_{core}.{dialect}.log.gz")),
            elf: Some(firmware().join(format!("state_{core}.elf"))),
            out: dir.path().to_path_buf(),
            checkpoint_interval: 256,
            endian,
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
            let fp = name == "fpscr" || name.starts_with('s') && name[1..].bytes().all(|b| b.is_ascii_digit());
            let reg = engine.register_id(name).unwrap();
            let v = engine.register(index, reg).unwrap();
            match v.value {
                Some(got) if v.known_mask.is_none() => {
                    tally.reg_known += 1;
                    tally.fp_known += fp as u64;
                    let want = want.as_u64().unwrap();
                    if got != want {
                        tally
                            .mismatches
                            .push(format!("index {index}: {name} = {got:#x}, gdb {want:#x} ({:?})", v.evidence));
                    }
                }
                _ => {
                    tally.reg_unknown += 1;
                    tally.fp_unknown += fp as u64;
                    *tally.unknown_by_register.entry(name.clone()).or_default() += 1;
                }
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
    // m4be: the m4 firmware built big-endian (BE8), run on QEMU patched for
    // big-endian M-profile data (tests/firmware/qemu/qemu-m-bigend.patch).
    // m4f: hard-float build; its traces write the FP registers as whole D
    // registers (fm), D registers with one half `-` (m4rtl) and S registers (es).
    for core in ["m4", "m35p", "m4be", "m4f"] {
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
            let core_known = t.reg_known - t.fp_known;
            let core_total = total - t.fp_known - t.fp_unknown;
            // m4f runs FP code first, before r8-r12 are ever written.
            let tenths = if core == "m4f" { 6 } else { 7 };
            assert!(
                core_known * 10 >= core_total * tenths,
                "{core} {dialect}: too few registers known: {core_known}/{core_total}, unknown {:?}",
                t.unknown_by_register
            );
            if core == "m4f" {
                // FP registers the firmware uses, compared at most points.
                assert!(t.fp_known > 300, "{core} {dialect}: {} FP register values compared", t.fp_known);
                // FPCCR (lazy FP state) inferred from reset at every point,
                // FPCAR from the first FP frame on (traces do not record them).
                assert!(!t.unknown_by_register.contains_key("fpccr"), "{core} {dialect}: {:?}", t.unknown_by_register);
                assert!(t.unknown_by_register.get("fpcar").copied().unwrap_or(0) < 20, "{:?}", t.unknown_by_register);
            }
        }
    }
    assert!(failures.is_empty(), "values differ from gdb:\n{}", failures.join("\n"));
}

/// The comparison above can tell the byte orders apart: the big-endian run
/// read as little-endian data (forced) disagrees with gdb where the trace
/// writes data as numbers (`MR4`/`MW4`, and Cortex-M RTL `___D` lines, whose
/// value is byte-swapped), while the default (from the ELF header) agrees.
/// `LD`/`ST` diagrams give bytes by address, so the core's byte order does
/// not change them.
#[test]
fn big_endian_trace_read_as_little_endian_differs() {
    for dialect in ["fm", "m4rtl"] {
        let wrong = check_with("m4be", dialect, Some(simgdb_isa::Endian::Little));
        assert!(wrong.mismatches.len() > 10, "{dialect}: {} mismatches", wrong.mismatches.len());
    }
    let t = check_with("m4be", "es", Some(simgdb_isa::Endian::Little));
    assert!(t.mismatches.is_empty(), "es: {:?}", t.mismatches);
}

/// Traces that do not record CONTROL, PRIMASK, BASEPRI, FAULTMASK or PSP
/// (as Cortex-M4 RTL traces): the same traces with those records removed.
/// Only the fm and es dialects: they record SP and the banked registers
/// separately, so removing PSP keeps SP. (m4rtl writes SP only as
/// `r13 v (PSP)`/`(MSP)`; without those records nothing would say how SP
/// moves on the process stack, a different trace.) SimGDB infers
/// them from reset, the executed MSR and exception entries and returns; every
/// value it then claims must still equal gdb's, and CONTROL must be known
/// nearly everywhere.
#[test]
fn inferred_system_registers_match_gdbstub() {
    let hidden = ["control", "primask", "basepri", "faultmask", "psp", "sp_process"];
    for core in ["m4", "m35p", "m4be", "m4f"] {
        for dialect in ["fm", "es"] {
            let dir = tempfile::tempdir().unwrap();
            let mut text = String::new();
            let path = firmware().join(format!("traces/state_{core}.{dialect}.log.gz"));
            flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut text).unwrap();
            let mut kept = String::new();
            for line in text.lines() {
                let words: Vec<&str> = line.split_whitespace().collect();
                let at = words.iter().position(|w| *w == "R");
                if let Some(name) = at.and_then(|k| words.get(k + 1))
                    && hidden.contains(&name.to_ascii_lowercase().as_str())
                {
                    continue;
                }
                kept.push_str(line);
                kept.push('\n');
            }
            let trace = dir.path().join("hidden.log");
            std::fs::write(&trace, kept).unwrap();
            let out = dir.path().join("idx");
            let isa = simgdb_isa::by_name("arm-m").unwrap();
            let meta = build(
                isa.as_ref(),
                &IndexOptions {
                    trace,
                    elf: Some(firmware().join(format!("state_{core}.elf"))),
                    out: out.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
            // (PSP may still be set from SP records through the bank rule.)
            for r in ["control", "primask", "basepri", "faultmask"] {
                assert!(meta.report.registers_never_written.contains(&r.to_string()), "{core} {dialect}: {r} hidden");
            }
            let engine = Engine::open(&out).unwrap();
            let t = truth(core);
            let mut mismatches = Vec::new();
            let mut known: BTreeMap<&str, u64> = BTreeMap::new();
            let points = t["points"].as_array().unwrap();
            for p in points {
                let index = p["index"].as_u64().unwrap();
                for (name, want) in p["registers"].as_object().unwrap() {
                    let v = engine.register(index, engine.register_id(name).unwrap()).unwrap();
                    if let (Some(got), None) = (v.value, v.known_mask) {
                        if ["control", "primask", "basepri", "faultmask", "psp", "sp", "msp"].contains(&name.as_str()) {
                            *known.entry(name.as_str()).or_default() += 1;
                        }
                        if got != want.as_u64().unwrap() {
                            mismatches.push(format!("index {index}: {name} = {got:#x}, gdb {want} ({:?})", v.evidence));
                        }
                    }
                }
            }
            assert!(mismatches.is_empty(), "{core} {dialect}: {}", mismatches.join("\n"));
            // Known at every point (from reset on), except CONTROL in m4f's
            // FP phase where gdb and SimGDB agree only when FPCA is known.
            let n = points.len() as u64;
            for r in ["primask", "basepri", "faultmask"] {
                assert_eq!(known.get(r), Some(&n), "{core} {dialect}: {r} known {known:?}");
            }
            assert!(known.get("control").copied().unwrap_or(0) * 10 >= n * 9, "{core} {dialect}: {known:?}");
            eprintln!("{core} {dialect}: of {n} points known {known:?}");
        }
    }
}
