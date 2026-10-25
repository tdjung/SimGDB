//! Regressions from the independent code review of the first milestone. Each
//! trace once made SimGDB report a stale or wrong value as known, or crash.

use simgdb_engine::Engine;
use simgdb_model::Source;
use simgdb_store::{IndexOptions, build};

fn index(trace: &str) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.log");
    std::fs::write(&path, trace).unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(isa.as_ref(), &IndexOptions { trace: path, out: dir.path().join("idx"), ..Default::default() }).unwrap();
    let e = Engine::open(&dir.path().join("idx")).unwrap();
    (dir, e)
}

fn reg(e: &Engine, at: u64, name: &str) -> (Option<u64>, Source) {
    let v = e.register(at, e.register_id(name).unwrap()).unwrap();
    (if v.is_known() { v.value } else { None }, v.source)
}

fn word(e: &Engine, addr: u64, at: u64) -> Option<u32> {
    e.memory(addr, 4, at).unwrap().iter().rev().try_fold(0u32, |acc, b| b.value.map(|v| acc << 8 | v as u32))
}

const PRELUDE: &str = "\
1 ps R MSP 20001000
1 ps R r13 20001000
1 ps R XPSR 01000000
1 ps R CONTROL 00000000
";

#[test]
fn switch_to_unwritten_psp_makes_sp_unknown() {
    let (_d, e) = index(&format!(
        "{PRELUDE}2 ps IT (0) 00000100 f3808814 T thread : MSR CONTROL, r0\n2 ps R CONTROL 00000002\n3 ps IT (1) 00000104 bf00 T thread : NOP\n"
    ));
    assert_eq!(reg(&e, 1, "sp").0, None);
    assert_eq!(reg(&e, 1, "msp").0, Some(0x2000_1000));
}

#[test]
fn sp_written_with_mode_unknown_makes_banks_unknown() {
    let (_d, e) = index(
        "1 ps R MSP 20001000\n1 ps R r13 20001000\n2 ps IT (0) 00000100 b082 T thread : SUB sp,#8\n2 ps R r13 20000ff8\n3 ps IT (1) 00000102 bf00 T thread : NOP\n",
    );
    assert_eq!(reg(&e, 1, "sp").0, Some(0x2000_0ff8));
    assert_eq!(reg(&e, 1, "msp").0, None);
}

#[test]
fn unknown_sp_reaches_the_active_bank() {
    let (_d, e) = index(&format!(
        "{PRELUDE}2 ps IT (0) 00000100 b082 T thread : SUB sp,#8\n2 ps R r13 xxxxxxxx\n3 ps IT (1) 00000102 bf00 T thread : NOP\n"
    ));
    assert_eq!(reg(&e, 1, "sp").0, None);
    assert_eq!(reg(&e, 1, "msp").0, None);
}

#[test]
fn non_secure_bank_does_not_move_sp() {
    let (_d, e) = index(
        "1 ps R MSP_S 30040000\n1 ps R r13 30040000\n1 ps R XPSR 01000000\n1 ps R CONTROL_S 00000000\n\
         2 ps IT (0) 00000100 f3808888 T thread : MSR MSP_NS, r0\n2 ps R MSP_NS 20010000\n3 ps IT (1) 00000104 bf00 T thread : NOP\n",
    );
    assert_eq!(reg(&e, 1, "sp").0, Some(0x3004_0000));
    assert_eq!(reg(&e, 1, "msp").0, Some(0x3004_0000));
    assert_eq!(e.ds.meta.report.unmodelled_registers.get("msp_ns"), Some(&1));
}

#[test]
fn bank_annotation_before_the_value() {
    let (_d, e) = index(
        "1 ps R MSP 20001000\n1 ps R r13 20001000\n1 ps R XPSR 0100000f\n1 ps R CONTROL 00000000\n\
         2 ps IT (0) 00000100 f3808809 T handler : MSR PSP, r0\n2 ps R r13 (PSP) 20000800\n3 ps IT (1) 00000104 bf00 T handler : NOP\n",
    );
    assert_eq!(reg(&e, 1, "psp").0, Some(0x2000_0800));
    assert_eq!(reg(&e, 1, "msp").0, Some(0x2000_1000));
    assert_eq!(reg(&e, 1, "sp").0, Some(0x2000_1000));
}

#[test]
fn other_core_writes_make_shared_bytes_unknown() {
    let (_d, e) = index(
        "1 clk cpu0 IT (0) 00000100 6008 T thread : STR r0,[r1]\n1 clk cpu0 MW4 20000000 11111111\n\
         2 clk cpu1 IT (0) 00000200 6008 T thread : STR r0,[r1]\n2 clk cpu1 MW4 20000000 22222222\n\
         3 clk cpu0 IT (1) 00000102 bf00 T thread : NOP\n4 clk cpu0 IT (2) 00000104 bf00 T thread : NOP\n",
    );
    assert_eq!(word(&e, 0x2000_0000, 1), None);
    assert_eq!(e.ds.meta.report.other_core_writes, 1);
}

#[test]
fn frame_marking_keeps_the_interrupted_stack() {
    // The frame is 32 bytes at 0x20000fa0; 0x20000fd0 belongs to the
    // interrupted code and stays known through the exception.
    let (_d, e) = index(
        "1 ps R MSP 20000fc0\n1 ps R r13 20000fc0\n1 ps R XPSR 01000000\n1 ps R CONTROL 00000000\n1 ps R lr 00000101\n\
         2 ps IT (0) 00000100 9004 T thread : STR r0,[sp,#16]\n2 ps MW4 20000fd0 11223344\n\
         3 ps E 00000200 0000000f CoreEvent_IRQ\n\
         3 ps R lr fffffff9\n3 ps R XPSR 0100000f\n3 ps R r13 20000fa0\n\
         4 ps IT (1) 00000200 bf00 T handler : NOP\n",
    );
    assert_eq!(word(&e, 0x2000_0fd0, 1), Some(0x1122_3344));
    // Inside the frame, no store recorded: unknown.
    assert_eq!(word(&e, 0x2000_0fbc, 1), None);
}

#[test]
fn address_past_the_isa_range_is_reported_not_wrapped() {
    let (_d, e) = index(
        "1 ps R MSP 20001000\n2 ps IT (0) 00000100 bf00 T thread : NOP\n2 ps MW4 fffffffffffffffe 11223344\n3 ps IT (1) 00000104 bf00 T thread : NOP\n",
    );
    assert_eq!(e.memory(0, 2, 1).unwrap()[0].value, None);
    assert_eq!(e.ds.meta.report.parse_errors.count, 1);
}

#[test]
fn bad_requests_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.log");
    std::fs::write(&path, "1 ps IT (0) 00000100 bf00 T thread : NOP\n").unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let opts = IndexOptions { trace: path, out: dir.path().join("idx"), checkpoint_interval: 0, ..Default::default() };
    assert!(build(isa.as_ref(), &opts).is_err());
    let (_d, e) = index("1 ps IT (0) 00000100 bf00 T thread : NOP\n");
    assert!(e.memory(0, u64::MAX, 0).is_err());
}
