//! Reconstruction rules on a small hand-written trace (LIMITATIONS.md,
//! "Step 1 구현의 규칙"): cases the QEMU firmware does not produce.

use simgdb_engine::Engine;
use simgdb_model::Source;
use simgdb_store::{IndexOptions, build};

const TRACE: &str = "\
Tarmac Text Rev 3t
0 ns R MSP 20001000
0 ns R XPSR 01000000
0 ns R CONTROL 00000000
10 ns IT (0) 00000100 2001 T thread : MOVS r0,#1
10 ns R r0 00000001
20 ns IT (1) 00000102 4b02 T thread : LDR r3,[pc,#8]
20 ns MR4 0000010c 20000800
20 ns R r3 20000800
20 ns IF (1) 00000102 4b02 T thread : LDR r3,[pc,#8]
30 ns IS (2) 00000104 d001 T thread : BEQ 10a
40 ns IT (3) 00000106 6819 T thread : LDR r1,[r3,#0]
40 ns MR4 20000800 cafef00d
40 ns R r1 cafef00d
50 ns IT (4) 00000108 6018 T thread : STR r0,[r3,#0]
50 ns MW4 20000800 00000001
50 ns E 00000200 0000000f CoreEvent_IRQ
50 ns R r13 20000fe0
50 ns R XPSR 0100000f
50 ns R r14 fffffff9
60 ns IT (5) 00000200 b510 T thread : PUSH {r4,lr}
60 ns R r13 20000fd8
60 ns MR4 40001000 00000005
60 ns R r2 xxxx00ff
70 ns IT (6) 00000202 bd10 T thread : POP {r4,pc}
";

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("t.log");
    std::fs::write(&trace, TRACE).unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let meta = build(
        isa.as_ref(),
        &IndexOptions { trace, out: dir.path().join("idx"), checkpoint_interval: 2, ..Default::default() },
    )
    .unwrap();
    assert_eq!(meta.report.if_duplicates, 1);
    assert_eq!(meta.report.condition_failed, 1);
    let e = Engine::open(&dir.path().join("idx")).unwrap();
    (dir, e)
}

fn word(e: &Engine, addr: u64, at: u64) -> (Option<u32>, Source) {
    let b = e.memory(addr, 4, at).unwrap();
    let source = b[0].source;
    let v = b.iter().rev().try_fold(0u32, |acc, x| x.value.map(|v| acc << 8 | v as u32));
    (v, source)
}

#[test]
fn points_and_time() {
    let (_d, e) = engine();
    assert_eq!(e.end(), 7);
    assert!(e.condition_failed(2));
    assert_eq!(e.index_at_time(35), Some(3));
    assert_eq!(e.point(3).unwrap().sim_time.unwrap().value, 40);
    assert!(e.pc(7).unwrap().value.is_none(), "PC after the last instruction is not recorded");
}

#[test]
fn registers_become_known_when_written() {
    let (_d, e) = engine();
    let r0 = e.register_id("r0").unwrap();
    assert_eq!(e.register(0, r0).unwrap().source, Source::Unknown);
    assert_eq!(e.register(1, r0).unwrap().value, Some(1));
    // Partially known value: GDB shows it unavailable, JSON shows the mask.
    let r2 = e.register(6, e.register_id("r2").unwrap()).unwrap();
    assert_eq!((r2.value, r2.known_mask), (Some(0xff), Some(0xffff)));
    // Exception entry: SP written as r13 in handler mode lands in MSP.
    let msp = e.register(5, e.register_id("msp").unwrap()).unwrap();
    assert_eq!(msp.value, Some(0x2000_0fe0));
}

#[test]
fn memory_sources() {
    let (_d, e) = engine();
    // Before any access: a later read with no write before it.
    assert_eq!(word(&e, 0x2000_0800, 0), (Some(0xcafe_f00d), Source::Inferred));
    // The read is visible at the reading instruction itself.
    assert_eq!(word(&e, 0x2000_0800, 3), (Some(0xcafe_f00d), Source::Trace));
    assert_eq!(word(&e, 0x2000_0800, 5), (Some(1), Source::Trace));
    // Never accessed: unknown.
    assert_eq!(word(&e, 0x2000_0900, 5), (None, Source::Unknown));
    // Peripheral region: a read is evidence, not a value.
    let p = e.memory(0x4000_1000, 1, 6).unwrap();
    assert_eq!(p[0].source, Source::Unknown);
    assert_eq!(p[0].evidence.len(), 1);
}

#[test]
fn exception_frame_is_unknown_without_stacking_records() {
    let (_d, e) = engine();
    // Below the pre-entry SP (0x20001000) the frame was pushed: unknown at
    // the handler's first instruction, with evidence pointing at the E record.
    let b = e.memory(0x2000_0fe0, 4, 5).unwrap();
    assert_eq!(b[0].source, Source::Unknown);
    assert!(b[0].evidence[0].note.as_deref().unwrap().contains("exception entry"));
}
