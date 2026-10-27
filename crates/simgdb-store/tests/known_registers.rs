//! `registers_not_always_known` in the index report: registers unknown at
//! some point. GDB's M-profile unwinder fails every stack query while MSP or
//! PSP is unavailable, so `simgdb serve --no-psp-unwind` relies on this.

use std::path::PathBuf;

use simgdb_isa::RegState;
use simgdb_store::{Dataset, IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

#[test]
fn registers_unknown_somewhere_are_reported() {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    for dialect in ["fm", "m4rtl", "es"] {
        let out = tempfile::tempdir().unwrap();
        let opts = IndexOptions {
            trace: firmware().join(format!("traces/state_m4.{dialect}.log.gz")),
            elf: Some(firmware().join("state_m4.elf")),
            out: out.path().to_path_buf(),
            ..Default::default()
        };
        build(isa.as_ref(), &opts).unwrap();
        let ds = Dataset::open(out.path()).unwrap();
        let report = &ds.meta.report;
        let listed = report.registers_not_always_known.clone().expect("recorded by this indexer");

        // Recompute from the write table: a register is always known when the
        // writes visible at the first point leave it fully known and no later
        // write has unknown bits.
        let regs = isa.registers();
        let mut known_at_start = vec![false; regs.len()];
        let mut partial_write = vec![false; regs.len()];
        for row in 0..ds.rw_at.len() {
            let r = ds.rw_reg.get(row) as usize;
            let full = ds.rw_known.get(row) == RegState::full_mask(regs[r].bits);
            if ds.rw_at.get(row) == 0 {
                known_at_start[r] = full;
            }
            if !full {
                partial_write[r] = true;
            }
        }
        let expected: Vec<String> = regs
            .iter()
            .enumerate()
            .filter(|(r, _)| *r as u16 != isa.pc() && (!known_at_start[*r] || partial_write[*r]))
            .map(|(_, d)| d.name.to_string())
            .collect();
        assert_eq!(listed, expected, "{dialect}");

        // The firmware switches to PSP part way: PSP is written, yet unknown
        // before that, while MSP is known from reset on.
        assert!(!report.registers_never_written.contains(&"psp".to_string()), "{dialect}");
        assert!(listed.contains(&"psp".to_string()), "{dialect}");
        assert!(!listed.contains(&"msp".to_string()), "{dialect}");
        for never in &report.registers_never_written {
            assert!(listed.contains(never), "{dialect}: {never}");
        }
        // The firmware's own MSR PSP and MSR CONTROL, counted from the
        // instructions whether or not the trace records the registers; the
        // trace starts at the reset handler (vector table word 1).
        let w = &report.system_register_writes_by_instruction;
        assert_eq!((w.get("psp"), w.get("control"), w.len()), (Some(&1), Some(&1), 2), "{dialect}");
        assert_eq!(
            (report.first_pc, report.reset_vector, report.starts_at_reset),
            (Some(0x44), Some(0x44), Some(true))
        );
    }
}
