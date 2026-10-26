//! Data byte order of an index: where it comes from, and what it changes.
//! (Values against gdb for a big-endian run: simgdb-engine's qemu_truth,
//! variables, queries and call_stack tests with core `m4be`.)

use std::path::PathBuf;

use simgdb_isa::Endian;
use simgdb_store::config::SiteConfig;
use simgdb_store::{Dataset, IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

fn index(core: &str, elf: bool, endian: Option<Endian>, config: Option<&str>) -> Dataset {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let out = tempfile::tempdir().unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join(format!("traces/state_{core}.fm.log.gz")),
            elf: elf.then(|| firmware().join(format!("state_{core}.elf"))),
            out: out.path().to_path_buf(),
            endian,
            config: config.map(|c| SiteConfig::parse(c).unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let ds = Dataset::open(out.path()).unwrap();
    std::mem::forget(out); // the dataset maps files in it
    ds
}

#[test]
fn byte_order_comes_from_option_config_elf_default() {
    let ds = index("m4be", true, None, None);
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Big, Some("elf")));
    assert_eq!(ds.meta.report.endian_conflict, None);

    let ds = index("m4", true, None, None);
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Little, Some("elf")));

    let ds = index("m4be", false, None, None);
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Little, Some("isa default")));

    // An explicit choice wins, and a disagreement with the ELF is reported.
    let ds = index("m4be", true, Some(Endian::Little), None);
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Little, Some("option")));
    assert!(ds.meta.report.endian_conflict.as_deref().unwrap().contains("ELF header says big"));

    let ds = index("m4be", false, None, Some("[memory]\nendian = \"big\"\n"));
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Big, Some("config")));
    let ds = index("m4be", true, Some(Endian::Big), Some("[memory]\nendian = \"little\"\n"));
    assert_eq!((ds.meta.endian, ds.meta.report.endian_source.as_deref()), (Endian::Big, Some("option")));
}

/// `MW4 addr value` gives a number: a big-endian core stores its most
/// significant byte first, except in the PPB (always little-endian).
#[test]
fn logical_values_are_stored_by_address() {
    let ds = index("m4be", true, None, None);
    let first_row = |addr: u64| (0..ds.mem_addr.len()).find(|&r| ds.mem_addr.get(r) == addr);
    // SysTick reload (PPB) written with 61: byte 0x3d at the lowest address.
    let r = first_row(0xE000_E014).expect("SysTick RVR write");
    assert_eq!((ds.mem_size.get(r), ds.mem_value.get(r)), (4, 0x3d));
    // The first non-zero word store in RAM: the byte at the lowest address
    // must be the value's most significant one.
    let text = {
        use std::io::Read;
        let mut s = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(firmware().join("traces/state_m4be.fm.log.gz")).unwrap())
            .read_to_string(&mut s)
            .unwrap();
        s
    };
    let (addr, value) = text
        .lines()
        .find_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            (p.get(2) == Some(&"MW4") && p[4] != "00000000")
                .then(|| (u64::from_str_radix(p[3], 16).unwrap(), u64::from_str_radix(p[4], 16).unwrap()))
        })
        .unwrap();
    let r = first_row(addr).unwrap();
    // Stored by address: byte k at addr + k is (stored >> 8k).
    assert_eq!(ds.mem_value.get(r), (value as u32).swap_bytes() as u64, "{addr:#x} {value:#x}");
}
