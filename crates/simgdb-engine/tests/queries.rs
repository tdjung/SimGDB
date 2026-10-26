//! Set-layer queries against answers computed independently of SimGDB
//! (tests/queries/make_expected.py): brute force over the canonical QEMU log
//! of each run, plus tarmac_to_cachegrind's per-PC counts and line
//! attribution. Every Tarmac dialect must give the same answers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value as Json;
use simgdb_engine::{Engine, Target};
use simgdb_store::{IndexOptions, build};

const CORES: [&str; 2] = ["m4", "m35p"];
const DIALECTS: [&str; 3] = ["fm", "m4rtl", "es"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests")
}

fn expected(core: &str) -> Json {
    let text = std::fs::read_to_string(root().join(format!("queries/expected/state_{core}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn open(core: &str, dialect: &str, dir: &std::path::Path) -> Engine {
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: root().join(format!("firmware/traces/state_{core}.{dialect}.log.gz")),
            elf: Some(root().join(format!("firmware/state_{core}.elf"))),
            out: dir.to_path_buf(),
            checkpoint_interval: 128,
            ..Default::default()
        },
    )
    .unwrap();
    Engine::open(dir).unwrap()
}

fn each(f: impl Fn(&str, &str, &Engine, &Json)) {
    for core in CORES {
        let want = expected(core);
        for dialect in DIALECTS {
            let dir = tempfile::tempdir().unwrap();
            let engine = open(core, dialect, dir.path());
            assert_eq!(engine.end(), want["instructions"].as_u64().unwrap());
            f(core, dialect, &engine, &want);
        }
    }
}

fn opt(v: &Json) -> Option<u64> {
    v.as_u64()
}

#[test]
fn line_counts_match_the_converter() {
    each(|core, dialect, e, want| {
        let all = e.range(None, None).unwrap();
        let mut checked = 0;
        for (key, ir) in want["line_ir"].as_object().unwrap() {
            let Target::Line { file, line } = Target::parse(key) else { panic!("{key}") };
            if line == 0 || file.starts_with('?') {
                continue;
            }
            let got = e.find_executions(&Target::Line { file, line }, all, 0, 0).unwrap();
            assert_eq!(got.instructions, ir.as_u64().unwrap(), "{core} {dialect} {key}: executions");
            checked += 1;
        }
        assert!(checked > 50);
        for (key, visits) in want["line_visits"].as_object().unwrap() {
            let got = e.find_executions(&Target::parse(key), all, 1_000_000, 0).unwrap();
            let want: Vec<u64> = visits.as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            assert_eq!(got.entries.items, want, "{core} {dialect} {key}: entries");
            // Paging gives the same list.
            if want.len() > 7 {
                let first = e.find_executions(&Target::parse(key), all, 7, 0).unwrap();
                let next = first.entries.next_cursor.unwrap();
                let second = e.find_executions(&Target::parse(key), all, 7, next).unwrap();
                assert_eq!([first.entries.items, second.entries.items].concat(), want[..14.min(want.len())]);
            }
        }
    });
}

#[test]
fn exceptions_match_the_run() {
    each(|core, dialect, e, want| {
        let list = e.exception_list(e.range(None, None).unwrap(), None, 10_000, 0).unwrap();
        let want = want["exceptions"].as_array().unwrap();
        assert_eq!(list.exceptions.total as usize, want.len(), "{core} {dialect}");
        for (got, w) in list.exceptions.items.iter().zip(want) {
            let ctx = format!("{core} {dialect} exception at {}", got.entered_at);
            assert_eq!(got.entered_at, w["entry"].as_u64().unwrap(), "{ctx}");
            assert_eq!(got.number, w["number"].as_u64().map(|n| n as u32), "{ctx}");
            assert_eq!(got.returned_at, opt(&w["return"]), "{ctx}");
            assert_eq!(got.interrupted_pc, opt(&w["interrupted_pc"]), "{ctx}");
        }
        let names: BTreeSet<&str> = list.by_exception.keys().map(String::as_str).collect();
        assert!(names.contains("SysTick") && names.contains("SVCall"), "{names:?}");
        // Filtering by name or number.
        let svc: Vec<u64> = want.iter().filter(|w| w["number"] == 11).map(|w| w["entry"].as_u64().unwrap()).collect();
        for kind in ["svcall", "11"] {
            let got = e.exception_list(e.range(None, None).unwrap(), Some(kind), 100, 0).unwrap();
            let got: Vec<u64> = got.exceptions.items.iter().map(|x| x.entered_at).collect();
            assert_eq!(got, svc, "{core} {dialect} kind {kind}");
        }
        for x in &list.exceptions.items {
            assert_eq!(x.taken_after.as_ref().unwrap().index + 1, x.entered_at);
        }
    });
}

#[test]
fn calls_match_the_run() {
    each(|core, dialect, e, want| {
        let tree = e.call_tree(e.range(None, None).unwrap(), 4).unwrap();
        let got: BTreeMap<&str, (u64, u64, u64)> = tree
            .functions
            .iter()
            .filter(|f| f.kind == "call")
            .map(|f| (f.function.as_str(), (f.calls, f.returned, f.inclusive_instructions)))
            .collect();
        let want: BTreeMap<&str, (u64, u64, u64)> = want["calls"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str(),
                    (v["calls"].as_u64().unwrap(), v["returned"].as_u64().unwrap(), v["inclusive"].as_u64().unwrap()),
                )
            })
            .collect();
        assert_eq!(got, want, "{core} {dialect}");
        // The tree holds every call once.
        fn total(nodes: &[simgdb_engine::CallNode]) -> u64 {
            nodes.iter().map(|n| n.calls + n.deeper_calls + total(&n.children)).sum()
        }
        let calls: u64 = want.values().map(|v| v.0).sum();
        let exceptions = e.exception_list(e.range(None, None).unwrap(), None, 0, 0).unwrap().exceptions.total;
        assert_eq!(total(&tree.roots), calls + exceptions, "{core} {dialect}");
    });
}

#[test]
fn last_writes_match_the_run() {
    each(|core, dialect, e, want| {
        for w in want["last_write"].as_array().unwrap() {
            let (addr, point) = (w["addr"].as_u64().unwrap(), w["point"].as_u64().unwrap());
            let got = e.last_write(addr, 1, point).unwrap();
            let ctx = format!("{core} {dialect} last_write({addr:#x}) at {point}");
            match (got.byte_writer[0], opt(&w["written_by"])) {
                (None, None) => {}
                (Some(k), Some(by)) => {
                    let rec = &got.writes[k];
                    assert_eq!(rec.written_by, by, "{ctx}");
                    assert_eq!(rec.visible_from, by + 1, "{ctx}");
                    let byte = (rec.value.unwrap() >> (8 * (addr - rec.addr))) & 0xff;
                    assert_eq!(byte, w["byte"].as_u64().unwrap(), "{ctx}");
                }
                (g, w) => panic!("{ctx}: {g:?} vs {w:?}"),
            }
        }
        for w in want["writes_to"].as_array().unwrap() {
            let r = e.range(opt(&w["from"]), opt(&w["to"])).unwrap();
            // The oracle's range is the writing instructions from..to (exclusive).
            let r = simgdb_engine::Range { from: r.from, to: r.to - 1 };
            let got = e.writes_to(w["addr"].as_u64().unwrap(), w["len"].as_u64().unwrap(), r, 100_000, 0).unwrap();
            let got: Vec<(u64, u64, u32, u64)> =
                got.writes.items.iter().map(|x| (x.written_by, x.addr, x.size, x.value.unwrap())).collect();
            let want: Vec<(u64, u64, u32, u64)> = w["writes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    (
                        x["index"].as_u64().unwrap(),
                        x["addr"].as_u64().unwrap(),
                        x["size"].as_u64().unwrap() as u32,
                        x["value"].as_u64().unwrap(),
                    )
                })
                .collect();
            assert!(!want.is_empty());
            assert_eq!(got, want, "{core} {dialect} writes_to {}", w["name"]);
        }
    });
}

#[test]
fn diffs_match_the_run() {
    each(|core, dialect, e, want| {
        let regions: Vec<(u64, u64)> = want["diff_regions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()))
            .collect();
        let in_regions = |a: u64| regions.iter().any(|&(lo, size)| a >= lo && a < lo + size);
        for w in want["diff"].as_array().unwrap() {
            let (a, b) = (w["a"].as_u64().unwrap(), w["b"].as_u64().unwrap());
            let ctx = format!("{core} {dialect} diff {a} {b}");
            let d = e.diff(a, b, 100_000, 0).unwrap();
            // Registers SimGDB knows at both points must change exactly as in the run.
            let known = |i: u64| -> BTreeSet<String> {
                e.registers(i)
                    .unwrap()
                    .into_iter()
                    .filter(|r| r.value.value.is_some() && r.value.known_mask.is_none())
                    .map(|r| r.name.to_string())
                    .collect()
            };
            let (ka, kb) = (known(a), known(b));
            let want_regs: BTreeSet<&str> = w["registers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap())
                .filter(|r| ka.contains(*r) && kb.contains(*r))
                .collect();
            let known_both: BTreeSet<&str> = d
                .registers
                .iter()
                .filter(|r| {
                    r.a.known_mask.is_none() && r.b.known_mask.is_none() && r.a.value.is_some() && r.b.value.is_some()
                })
                .map(|r| r.name.as_str())
                .filter(|n| *n != "pc")
                .collect();
            assert_eq!(known_both, want_regs, "{ctx}: registers");
            let unknown: BTreeSet<u64> =
                w["oracle_unknown"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            let mut changed = BTreeSet::new();
            for c in &d.memory.items {
                if c.kind == "changed" {
                    changed.extend((c.addr..c.addr + c.len).filter(|a| in_regions(*a) && !unknown.contains(a)));
                }
            }
            let want_bytes: BTreeSet<u64> =
                w["bytes"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            assert_eq!(changed, want_bytes, "{ctx}: memory");
        }
    });
}

#[test]
fn find_first_matches_brute_force() {
    use simgdb_engine::{Direction, Range};
    each(|core, dialect, e, want| {
        let mut undecided = 0;
        for w in want["find_first"].as_array().unwrap() {
            let cond = w["condition"].as_str().unwrap();
            let range = Range { from: w["from"].as_u64().unwrap(), to: w["to"].as_u64().unwrap() };
            let dir = if w["direction"] == "forward" { Direction::Forward } else { Direction::Backward };
            let got = e.find_first(cond, range, dir).unwrap_or_else(|err| panic!("{cond}: {err}"));
            // Small windows (many passes) give the same answer (and, forward,
            // the same work; backward a single window evaluates more).
            for window in [1, 37, 500] {
                let w = e.find_first_windowed(cond, range, dir, window).unwrap();
                assert_eq!(
                    (w.found.map(|p| p.index), w.first_undecided, &w.undecided_ranges),
                    (got.found.as_ref().map(|p| p.index), got.first_undecided, &got.undecided_ranges),
                    "{core} {dialect} {cond} {dir:?} window {window}"
                );
                if dir == Direction::Forward {
                    assert_eq!((w.evaluated, w.undecided), (got.evaluated, got.undecided), "{cond} window {window}");
                }
            }
            let ctx = format!("{core} {dialect} find_first({cond}, {range:?}, {dir:?})");
            let found = got.found.as_ref().map(|p| p.index);
            let expected = opt(&w["found"]);
            // A miss is acceptable only where SimGDB cannot decide the
            // expected point (values the trace does not show there).
            if found != expected {
                let x = expected.unwrap_or_else(|| panic!("{ctx}: found {found:?}, expected none"));
                let at = e.find_first(cond, Range { from: x, to: x }, dir).unwrap();
                assert!(
                    at.found.is_none() && at.first_undecided == Some(x),
                    "{ctx}: {found:?}, expected {x} ({got:?})"
                );
                if let Some(f) = found {
                    assert!(if dir == Direction::Forward { f > x } else { f < x }, "{ctx}");
                }
                undecided += 1;
            }
        }
        eprintln!("{core} {dialect}: find_first cases answered exactly except {undecided} undecided");
    });
}

#[test]
fn conditions_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let e = open("m4", "fm", dir.path());
    let all = e.range(None, None).unwrap();
    use simgdb_engine::Direction::Forward;
    for bad in [
        "reg(\"r99\") == 1",
        "var(\"nope\") == 1",
        "var(\"pool\") == 1",
        "line == 12",
        "\"x\" == 1",
        "pc ==",
        "pc == 1 &&",
        "(pc == 1",
        "pc == 1)",
        "func == \"no_such_function\"",
        "mem32(sym(\"nope\")) == 0",
        "system(\"rm -rf /\")",
    ] {
        assert!(e.find_first(bad, all, Forward).is_err(), "{bad} should be rejected");
    }
    let f = e.find_first("pc < 0x100", all, Forward).unwrap();
    assert_eq!(f.found.unwrap().index, 0);
    // r0 is unknown until the trace writes it: undecided, not false.
    let f = e.find_first("pc < 0x100 && reg(\"r0\") == 0", all, Forward).unwrap();
    assert_eq!(f.first_undecided, Some(0));
    assert!(f.found.unwrap().index > 0);
    // Undecided stretches and every-point conditions across windows.
    use simgdb_engine::Direction::Backward;
    for cond in ["pc < 0x100 && reg(\"r4\") == 0", "var(\"svc_calls\") == 3", "reg(\"pc\") > 0x200 && reg(\"r7\") < 5"]
    {
        for dir in [Forward, Backward] {
            let one = e.find_first(cond, all, dir).unwrap();
            for window in [1, 3, 64, 999] {
                let w = e.find_first_windowed(cond, all, dir, window).unwrap();
                assert_eq!(
                    (w.found.map(|p| p.index), w.first_undecided, w.undecided_ranges),
                    (one.found.as_ref().map(|p| p.index), one.first_undecided, one.undecided_ranges.clone()),
                    "{cond} {dir:?} window {window}"
                );
            }
        }
    }
}

#[test]
fn register_histories_match_the_run() {
    each(|core, dialect, e, want| {
        let all = e.range(None, None).unwrap();
        for (reg, changes) in want["register_changes"].as_object().unwrap() {
            let got = e.register_history(reg, all, 1_000_000, 0).unwrap();
            // The trace may repeat a value; the run lists changes only.
            let mut last = None;
            let got: Vec<(u64, u64)> = got
                .items
                .iter()
                .filter_map(|w| {
                    let v = w.value.unwrap();
                    let changed = last != Some(v);
                    last = Some(v);
                    changed.then_some((w.written_by, v))
                })
                .collect();
            let want: Vec<(u64, u64)> =
                changes.as_array().unwrap().iter().map(|c| (c[0].as_u64().unwrap(), c[1].as_u64().unwrap())).collect();
            assert_eq!(got, want, "{core} {dialect} {reg}");
        }
    });
}

#[test]
fn wide_writes_show_the_part_inside() {
    let dir = tempfile::tempdir().unwrap();
    let e = open("m4", "m4rtl", dir.path());
    let all = e.range(None, None).unwrap();
    for var in ["result", "head", "wide", "ring[2]"] {
        let p = e.variable(var).unwrap();
        let size = p.size.unwrap();
        for w in e.writes_to(p.addr, size, all, 100_000, 0).unwrap().writes.items {
            let (lo, hi) = (w.addr, w.addr + w.size as u64);
            match &w.inside {
                None => assert!(lo >= p.addr && hi <= p.addr + size, "{var}: {w:?}"),
                Some(i) => {
                    assert!(lo < p.addr || hi > p.addr + size);
                    assert_eq!((i.addr, i.len), (lo.max(p.addr), hi.min(p.addr + size) - lo.max(p.addr)));
                    assert_eq!(i.hex.len() as u64, 2 * i.len);
                }
            }
        }
    }
}

#[test]
fn single_instruction_ranges_and_edges() {
    let dir = tempfile::tempdir().unwrap();
    let e = open("m4", "fm", dir.path());
    let all = e.range(None, None).unwrap();
    let pool = e.variable("pool").unwrap();
    let writes = e.writes_to(pool.addr, pool.size.unwrap(), all, 100_000, 0).unwrap().writes.items;
    assert!(writes.len() > 10);
    // A range of one instruction holds exactly that instruction's writes.
    for w in writes.iter().step_by(7) {
        let r = simgdb_engine::Range { from: w.written_by, to: w.written_by };
        let got = e.writes_to(pool.addr, pool.size.unwrap(), r, 100, 0).unwrap().writes.items;
        assert!(got.contains(w), "{w:?} missing from its own range");
        assert!(got.iter().all(|g| g.written_by == w.written_by));
    }
    // Exception-frame rows are longer than 8 bytes: reading them must not overflow.
    let sp = e.register_id("sp").unwrap();
    let exc = e.exception_list(all, None, 5, 0).unwrap().exceptions.items;
    for x in exc {
        let s = e.register(x.entered_at, sp).unwrap().value.unwrap();
        e.last_write(s, 32, x.entered_at).unwrap();
        e.writes_to(s.saturating_sub(64), 128, all, 10, 0).unwrap();
    }
    // Addresses past the address space are refused, not wrapped.
    assert!(e.last_write(0xffff_fffe, 4, 0).is_err());
    assert!(e.writes_to(u64::MAX - 1, 4, all, 10, 0).is_err());
    assert!(e.find_first("mem32(0xfffffffe) == 0", all, simgdb_engine::Direction::Forward).is_err());
}

#[test]
fn undecided_ranges_match_point_by_point() {
    use simgdb_engine::{Direction, Range};
    let dir = tempfile::tempdir().unwrap();
    let e = open("m4", "es", dir.path());
    for cond in ["reg(\"r4\") == 0x20000018", "var(\"ticks\") == 2 || reg(\"r9\") != 0"] {
        // The condition at every point, evaluated alone.
        let at: Vec<Option<bool>> = (0..=e.end())
            .map(|i| {
                let f = e.find_first(cond, Range { from: i, to: i }, Direction::Forward).unwrap();
                if f.found.is_some() {
                    Some(true)
                } else if f.undecided > 0 {
                    None
                } else {
                    Some(false)
                }
            })
            .collect();
        for d in [Direction::Forward, Direction::Backward] {
            let got = e.find_first(cond, e.range(None, None).unwrap(), d).unwrap();
            let order: Vec<u64> =
                if d == Direction::Forward { (0..=e.end()).collect() } else { (0..=e.end()).rev().collect() };
            let hit = order.iter().copied().find(|&i| at[i as usize] == Some(true));
            assert_eq!(got.found.map(|p| p.index), hit, "{cond} {d:?}");
            // Undecided stretches before the match in search order.
            let mut want: Vec<[u64; 2]> = Vec::new();
            for &i in order.iter().take_while(|&&i| Some(i) != hit) {
                if at[i as usize].is_none() {
                    match want.last_mut() {
                        Some(r) if d == Direction::Forward && r[1] + 1 == i => r[1] = i,
                        Some(r) if d == Direction::Backward && r[0] == i + 1 => r[0] = i,
                        _ => want.push([i, i]),
                    }
                }
            }
            want.truncate(5);
            assert_eq!(got.undecided_ranges, want, "{cond} {d:?}");
            let first = want.first().map(|r| if d == Direction::Forward { r[0] } else { r[1] });
            assert_eq!(got.first_undecided, first, "{cond} {d:?}");
        }
    }
}
