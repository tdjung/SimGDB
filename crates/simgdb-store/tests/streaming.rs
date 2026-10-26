//! The streaming indexer gives the same dataset whatever its buffer sizes:
//! many small record batches and many spilled sort runs (as for traces far
//! larger than memory) against one batch and in-memory sorts.

use std::path::PathBuf;

use simgdb_store::{Dataset, IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

#[test]
fn small_buffers_give_the_same_dataset() {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    for dialect in ["fm", "m4rtl", "es"] {
        let opts = |out: PathBuf, batch_rows, sort_records| IndexOptions {
            trace: firmware().join(format!("traces/state_m4.{dialect}.log.gz")),
            elf: Some(firmware().join("state_m4.elf")),
            out,
            checkpoint_interval: 100,
            batch_rows,
            sort_records,
            ..Default::default()
        };
        let big = tempfile::tempdir().unwrap();
        let small = tempfile::tempdir().unwrap();
        build(isa.as_ref(), &opts(big.path().to_path_buf(), 1 << 20, 1 << 30)).unwrap();
        build(isa.as_ref(), &opts(small.path().to_path_buf(), 256, 1000)).unwrap();
        assert!(!small.path().join(".sort").exists(), "sort runs removed");
        let a = Dataset::open(big.path()).unwrap();
        let b = Dataset::open(small.path()).unwrap();
        // The small build really has many batches.
        assert!(b.mi_addr.chunks().count() > 50, "{dialect}");
        macro_rules! same {
            ($f:ident) => {
                assert_eq!(a.$f.len(), b.$f.len(), "{dialect} {}", stringify!($f));
                for i in 0..a.$f.len() {
                    assert_eq!(a.$f.get(i), b.$f.get(i), "{dialect} {}[{i}]", stringify!($f));
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
        assert_eq!(a.meta.report, b.meta.report);
    }
}
