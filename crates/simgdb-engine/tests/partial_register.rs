//! A register record with `-` digits writes only the other bits; the rest
//! keep their value (Tarmac partial updates such as `R D2 --------37800000`,
//! which writes S4 only).

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
         4 ps R D2 --------37800000\n\
         5 ps IT (5) 00000108 2001 T thread : MOVS r0,#1\n\
         5 ps R D2 3f800000--------\n\
         6 ps IT (6) 0000010a 2001 T thread : MOVS r0,#1\n\
         6 ps R s5 40000000\n\
         6 ps R D3 0123456789abcdef\n\
         7 ps IT (7) 0000010c 2001 T thread : MOVS r0,#1\n",
    )
    .unwrap();
    let out = dir.path().join("idx");
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let meta = build(isa.as_ref(), &IndexOptions { trace, out: out.clone(), ..Default::default() }).unwrap();
    assert_eq!(meta.report.parse_errors.count, 0);
    assert!(meta.report.unmodelled_registers.is_empty());
    let e = Engine::open(&out).unwrap();
    let r0 = e.register_id("r0").unwrap();
    assert_eq!(e.register(1, r0).unwrap().value, Some(0x1234_5678));
    assert_eq!(e.register(2, r0).unwrap().value, Some(0x1234_abcd));
    // Bits never known stay unknown; written `x` digits become unknown.
    let r1 = e.register(3, e.register_id("r1").unwrap()).unwrap();
    assert_eq!((r1.value, r1.known_mask), (Some(0x0cd), Some(0xfff)));
    // A D register is S<2n+1>:S<2n>; dashes leave a half unwritten.
    let s = |n: u32, at: u64| e.register(at, e.register_id(&format!("s{n}")).unwrap()).unwrap().value;
    assert_eq!((s(4, 4), s(5, 4)), (Some(0x3780_0000), None));
    assert_eq!((s(4, 5), s(5, 5)), (Some(0x3780_0000), Some(0x3f80_0000)));
    assert_eq!((s(5, 6), s(6, 6), s(7, 6)), (Some(0x4000_0000), Some(0x89ab_cdef), Some(0x0123_4567)));
}
