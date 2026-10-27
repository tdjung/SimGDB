//! Cortex-M4 RTL traces show each data access twice: the core's `M…___D`
//! record and a bus-side `B…___D/S` copy, sometimes an instruction later.
//! The index keeps the core records; the copies are checked against them.

use std::io::Read;
use std::path::PathBuf;

use simgdb_store::{Dataset, IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

fn index(text: &str, dir: &std::path::Path) -> Dataset {
    let trace = dir.join("t.log");
    std::fs::write(&trace, text).unwrap();
    let out = dir.join("idx");
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions { trace, elf: Some(firmware().join("state_m4.elf")), out: out.clone(), ..Default::default() },
    )
    .unwrap();
    Dataset::open(&out).unwrap()
}

#[test]
fn bus_copies_are_checked_not_indexed() {
    let mut plain = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(firmware().join("traces/state_m4.m4rtl.log.gz")).unwrap())
        .read_to_string(&mut plain)
        .unwrap();
    // Add a copy of every core data record: alternately right before it and
    // after the next instruction record (as in the user's trace), with the
    // bus letter by region.
    let mut with = String::new();
    let mut late: Vec<String> = Vec::new();
    let mut copies = 0;
    for (n, line) in plain.lines().enumerate() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.get(1) == Some(&"cyc") && p.get(2) == Some(&"IT") {
            with.push_str(line);
            with.push('\n');
            for l in late.drain(..) {
                with.push_str(&l);
                with.push('\n');
            }
            continue;
        }
        if p.len() == 5 && p[2].starts_with('M') && p[2].ends_with("___D") {
            let bus = if p[3].starts_with('2') { 'S' } else { 'D' };
            let copy = format!("{} cyc B{}{} {} {}", p[0], &p[2][1..7], bus, p[3], p[4]);
            copies += 1;
            if n % 2 == 0 {
                with.push_str(&copy);
                with.push('\n');
            } else {
                late.push(copy);
            }
        }
        with.push_str(line);
        with.push('\n');
    }
    // A copy with no core record, and a core read the bus splits in two.
    with.push_str("99998 cyc MNR4___D 00001002 a1b2c3d4\n");
    with.push_str("99998 cyc BNR2___D 00001002 a1b2\n");
    with.push_str("99998 cyc BNR2___D 00001004 c3d4\n");
    with.push_str("99998 cyc MNW1___D 20000210 01\n");
    with.push_str("99998 cyc BNW1___S 20000210 00\n");
    with.push_str("99999 cyc BNW4___S 20000200 11223344\n");
    with.push_str("99999 cyc IT (00000044:00000000) 00000044     4a11 T16 LDR r2, [pc, #68]\n");

    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let base = index(&plain, a.path());
    let ds = index(&with, b.path());
    // Memory state is the core records' only (plus the two added ones).
    assert_eq!(ds.mem_addr.len(), base.mem_addr.len() + 2);
    for r in 0..base.mem_addr.len() {
        assert_eq!(
            (ds.mem_at.get(r), ds.mem_addr.get(r), ds.mem_value.get(r)),
            (base.mem_at.get(r), base.mem_addr.get(r), base.mem_value.get(r))
        );
    }
    let c = ds.meta.report.bus_copies.clone().expect("copies seen");
    assert_eq!(c.records, copies + 4);
    assert_eq!((c.matched, c.matched_reversed, c.matched_part), (copies, 0, 2), "{c:?}");
    assert_eq!((c.unmatched_reads, c.unmatched_writes), (0, 2), "{c:?}");
    // Each sample says what the copy was and what the core wrote there.
    let s: Vec<&str> = c.unmatched_write_samples.iter().map(|s| s.text.as_str()).collect();
    assert!(
        s.iter().any(|t| t.contains("write 4 bytes at 0x20000200: 11223344") && t.contains("no core record")),
        "{s:?}"
    );
    assert!(
        s.iter().any(|t| t.contains("write 1 byte at 0x20000210: 00")
            && t.contains("nearest core record on these bytes, the same instruction: write 1 byte at 0x20000210: 01")
            && t.contains("copies on its bytes (instructions from it): [+0] write 1 byte at 0x20000210: 00")),
        "{s:?}"
    );
    assert_eq!(c.core_without_copy, 1); // the core write whose copy differs
    assert!(ds.meta.report.unrecognised.is_empty());
    assert!(base.meta.report.bus_copies.is_none());
}
