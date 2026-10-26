//! Source attribution must equal tarmac_to_cachegrind's (D14). The expected
//! files come from the converter's own functions, run with GNU addr2line and
//! with llvm-addr2line (tests/attribution/make_expected.py). The function
//! must equal GNU's (symbol fallback for code without a DWARF function); file
//! and line must equal GNU's, or LLVM's where GNU has line 0 (GNU misses
//! inline frames described with DWARF 5 range lists).

use std::path::PathBuf;

use simgdb_elf::source::attribute;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/attribution")
}

#[test]
fn matches_tarmac_to_cachegrind() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for entry in std::fs::read_dir(dir().join("elf")).unwrap() {
        let elf = entry.unwrap().path();
        let stem = elf.file_stem().unwrap().to_string_lossy().to_string();
        let load = |flavour: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(
                &std::fs::read_to_string(dir().join(format!("expected/{stem}.{flavour}.json"))).unwrap(),
            )
            .unwrap()
        };
        let (expected, llvm) = (load("gnu"), load("llvm"));
        let ours = attribute(&elf).unwrap();
        for (pc, want) in &expected {
            let pc_n = u64::from_str_radix(pc, 16).unwrap();
            let want = (want[0].as_str().unwrap(), want[1].as_str().unwrap(), want[2].as_u64().unwrap() as u32);
            let alt = &llvm[pc];
            let alt = (alt[0].as_str().unwrap(), want.1, alt[2].as_u64().unwrap() as u32);
            match ours.get(&pc_n) {
                Some(a) if (a.file.as_str(), a.function.as_str(), a.line) == want => {}
                Some(a) if want.2 == 0 && (a.file.as_str(), a.function.as_str(), a.line) == alt => {}
                Some(a) => failures.push(format!(
                    "{stem} {pc}: {:?} vs converter {want:?} ({:?})",
                    (&a.file, &a.function, a.line),
                    a.placement
                )),
                None => failures.push(format!("{stem} {pc}: not an instruction for SimGDB")),
            }
            checked += 1;
        }
        for pc in ours.keys() {
            if !expected.contains_key(&format!("{pc:x}")) {
                failures.push(format!("{stem} {pc:x}: instruction only for SimGDB"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures[..failures.len().min(30)].join("\n")
    );
    assert!(checked > 1000);
}
