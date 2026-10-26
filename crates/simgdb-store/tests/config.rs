//! The site configuration file changes how a trace is read without a
//! rebuild: a trace written with site-specific names, read with a config
//! that maps them, gives the same dataset as the standard trace.

use std::io::{Read, Write};
use std::path::PathBuf;

use simgdb_store::config::SiteConfig;
use simgdb_store::{Dataset, IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

#[test]
fn site_names_read_through_the_config() {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(firmware().join("traces/state_m4.fm.log.gz")).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    // The same trace in a site's own notation: a different register name and
    // memory write keyword, plus a record type SimGDB does not know (at the
    // end, so trace line numbers stay the same).
    let mut site: String =
        text.lines().map(|l| l.replace(" R r4 ", " R GPR4 ").replace(" MW4 ", " XW4 ")).collect::<Vec<_>>().join("\n");
    site.push_str("\n99999999 ps CSR mstatus 00000000\n");
    assert!(site.contains("GPR4") && site.contains("XW4") && site.contains("CSR"));
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("site.log");
    std::fs::File::create(&trace).unwrap().write_all(site.as_bytes()).unwrap();
    let config = SiteConfig::parse(
        r#"
        [tarmac]
        record_aliases = { XW4 = "MW4" }
        ignore = ["CSR"]
        [registers]
        aliases = { gpr4 = "r4" }
        "#,
    )
    .unwrap();
    let opts = |trace: PathBuf, out: PathBuf, config| IndexOptions {
        trace,
        elf: Some(firmware().join("state_m4.elf")),
        out,
        config,
        ..Default::default()
    };
    let (std_dir, site_dir, raw_dir) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    build(isa.as_ref(), &opts(firmware().join("traces/state_m4.fm.log.gz"), std_dir.path().into(), None)).unwrap();
    let site_meta = build(isa.as_ref(), &opts(trace.clone(), site_dir.path().into(), Some(config.clone()))).unwrap();
    let raw_meta = build(isa.as_ref(), &opts(trace, raw_dir.path().into(), None)).unwrap();
    // Without the config the site names are reported, not silently dropped.
    assert!(raw_meta.report.unrecognised.contains_key("XW4") && raw_meta.report.unrecognised.contains_key("CSR"));
    assert!(raw_meta.report.unmodelled_registers.contains_key("gpr4"));
    // With it nothing is left unknown.
    assert!(site_meta.report.unrecognised.is_empty(), "{:?}", site_meta.report.unrecognised);
    assert!(site_meta.report.unmodelled_registers.is_empty());
    assert_eq!(site_meta.config, Some(config));
    let a = Dataset::open(std_dir.path()).unwrap();
    let b = Dataset::open(site_dir.path()).unwrap();
    macro_rules! same {
        ($f:ident) => {
            assert_eq!(a.$f.len(), b.$f.len(), "{}", stringify!($f));
            for i in 0..a.$f.len() {
                assert_eq!(a.$f.get(i), b.$f.get(i), "{}[{i}]", stringify!($f));
            }
        };
    }
    same!(insn_pc);
    same!(insn_opcode);
    same!(insn_width);
    same!(insn_flags);
    same!(insn_time);
    same!(insn_line);
    same!(rw_at);
    same!(rw_reg);
    same!(rw_value);
    same!(rw_known);
    same!(rw_line);
    same!(rw_derived);
    same!(mem_at);
    same!(mem_addr);
    same!(mem_size);
    same!(mem_kind);
    same!(mem_value);
    same!(mem_known);
    same!(mem_line);
    same!(mi_addr);
    same!(mi_at);
    same!(mi_row);
    same!(pi_pc);
    same!(pi_index);
    same!(cp_index);
    same!(cp_rw_row);
    same!(exc_at);
    same!(exc_number);
    same!(exc_kind);
    same!(exc_target);
    same!(exc_line);
    same!(insn_frame);
    same!(call_kind);
    same!(call_index);
    same!(call_entry);
    same!(call_return);
    same!(call_pc);
    same!(call_return_pc);
    same!(call_number);
    same!(call_parent);
    same!(call_depth);
    same!(src_pc);
    same!(src_file);
    same!(src_function);
    same!(src_line);
    same!(src_placement);
    same!(src_frame_start);
    same!(src_frame_count);
    same!(fr_function);
    same!(fr_file);
    same!(fr_line);
}

#[test]
fn time_map_gives_absolute_time() {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = SiteConfig::parse(
        "[time]\nunit = \"ns\"\nsegments = [{ from_cycle = 0, time = 0, period = 10 }, { from_cycle = 5000, time = 50000, period = 20 }]",
    )
    .unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join("traces/state_m4.m4rtl.log.gz"),
            out: dir.path().into(),
            config: Some(config),
            ..Default::default()
        },
    )
    .unwrap();
    let e = simgdb_engine::Engine::open(dir.path()).unwrap();
    let mut seen_second = false;
    for i in (0..e.end()).step_by(97) {
        let p = e.point(i).unwrap();
        let c = p.sim_time.unwrap().value;
        let want = if c < 5000 { c * 10 } else { 50000 + (c - 5000) * 20 };
        seen_second |= c >= 5000;
        let t = p.time.unwrap();
        assert_eq!((t.value, t.unit.as_str(), t.source.as_str()), (want, "ns", "time_map"), "index {i}");
    }
    assert!(seen_second);
}
