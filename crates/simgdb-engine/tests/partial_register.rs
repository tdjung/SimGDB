//! A register record with `-` digits writes only the other bits; the rest
//! keep their value (Tarmac partial updates such as `R D2 --------37800000`).

use simgdb_engine::Engine;
use simgdb_store::{IndexOptions, build};

#[test]
fn dashes_keep_the_previous_bits() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("t.log");
    std::fs::write(
        &trace,
        "1 ps IT (1) 00000100 2001 T thread : MOVS r0,#1\n\
         1 ps R r0 12345678\n\
         2 ps IT (2) 00000102 2001 T thread : MOVS r0,#1\n\
         2 ps R r0 ----abcd\n\
         3 ps IT (3) 00000104 2001 T thread : MOVS r0,#1\n\
         3 ps R r1 ----x0cd\n\
         4 ps IT (4) 00000106 2001 T thread : MOVS r0,#1\n\
         4 ps R D2 --------37800000\n",
    )
    .unwrap();
    let out = dir.path().join("idx");
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let meta = build(isa.as_ref(), &IndexOptions { trace, out: out.clone(), ..Default::default() }).unwrap();
    assert_eq!(meta.report.parse_errors.count, 0);
    // FPU registers are not modelled: counted, not a parse error.
    assert_eq!(meta.report.unmodelled_registers.get("d2"), Some(&1));
    let e = Engine::open(&out).unwrap();
    let r0 = e.register_id("r0").unwrap();
    assert_eq!(e.register(1, r0).unwrap().value, Some(0x1234_5678));
    assert_eq!(e.register(2, r0).unwrap().value, Some(0x1234_abcd));
    // Bits never known stay unknown; written `x` digits become unknown.
    let r1 = e.register(3, e.register_id("r1").unwrap()).unwrap();
    assert_eq!((r1.value, r1.known_mask), (Some(0x0cd), Some(0xfff)));
}
