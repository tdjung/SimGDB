use std::path::PathBuf;

use simgdb_elf::Elf;

fn elf(name: &str) -> Elf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware").join(name);
    Elf::load(&path).unwrap()
}

#[test]
fn load_image_uses_load_addresses() {
    let e = elf("state_m4.elf");
    // Vector table: initial SP at 0, reset handler (Thumb) at 4.
    let sp = u32::from_le_bytes([0, 1, 2, 3].map(|k| e.initial_byte(k).unwrap()));
    assert_eq!(sp, 0x2001_0000);
    // .data lives in RAM at 0x20000000 but its initial image is in flash:
    // the RAM address has no initial value.
    let seed = e.symbol("seed").unwrap();
    assert_eq!(seed.addr, 0x2000_0000);
    assert_eq!(e.initial_byte(seed.addr), None);
    // .bss has no image.
    assert_eq!(e.initial_byte(e.symbol("ticks").unwrap().addr), None);
    assert!(e.function_at(e.symbol("mix").unwrap().addr + 2).is_some_and(|s| s.name == "mix"));
}
