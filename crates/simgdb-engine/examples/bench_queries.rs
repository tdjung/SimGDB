//! Query latency on an existing index: `cargo run --release --example
//! bench_queries -- INDEX_DIR`. Prints median and 99th percentile per query.

use std::time::Instant;

use simgdb_engine::Engine;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: bench_queries INDEX_DIR");
    let t = Instant::now();
    let e = Engine::open(std::path::Path::new(&dir)).unwrap();
    println!("open: {:.1} ms, {} instructions", t.elapsed().as_secs_f64() * 1e3, e.end());
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let points: Vec<u64> = (0..1000).map(|_| rnd(e.end())).collect();
    let sp = e.register_id("sp").unwrap();
    let report = |name: &str, mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{name:<40} median {:>8.3} ms   p99 {:>8.3} ms   ({} runs)",
            v[v.len() / 2],
            v[v.len() * 99 / 100],
            v.len()
        );
    };
    let time = |f: &mut dyn FnMut(u64)| -> Vec<f64> {
        points
            .iter()
            .map(|&i| {
                let t = Instant::now();
                f(i);
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect()
    };
    report("registers (all) at random point", time(&mut |i| drop(e.registers(i).unwrap())));
    report(
        "memory 64 B at SP, random point",
        time(&mut |i| {
            let s = e.register(i, sp).unwrap().value.unwrap_or(0x2000_ff00);
            drop(e.memory(s, 64, i).unwrap())
        }),
    );
    report("memory 256 B of .bss, random point", time(&mut |i| drop(e.memory(0x2000_0018, 256, i).unwrap())));
    let mut depth = 0;
    report(
        "locate (source line + call stack)",
        time(&mut |i| depth = depth.max(e.locate(i).unwrap().call_stack.len())),
    );
    println!("{:<40} {depth}", "deepest call stack seen");
    let pcs: Vec<u64> = points.iter().map(|&i| e.pc(i).unwrap().value.unwrap()).collect();
    report(
        "next execution of a PC (breakpoint)",
        time(&mut |i| {
            let _ = e.next_execution(&pcs[(i % 100) as usize..(i % 100) as usize + 1], i, true);
        }),
    );
    report(
        "previous execution of 4 PCs (reverse)",
        time(&mut |i| {
            let _ = e.next_execution(&pcs[(i % 100) as usize..(i % 100) as usize + 4], i, false);
        }),
    );
    let t = Instant::now();
    let n = e.executions(pcs[0]).count();
    println!("{:<40} {:.3} ms ({n} hits)", "all executions of one PC", t.elapsed().as_secs_f64() * 1e3);

    // Set-layer queries over the whole trace, once each.
    use simgdb_engine::{Direction, Target};
    let all = e.range(None, None).unwrap();
    let once = |name: &str, f: &mut dyn FnMut() -> String| {
        let t = Instant::now();
        let what = f();
        println!("{name:<40} {:>10.1} ms   {what}", t.elapsed().as_secs_f64() * 1e3);
    };
    once("executions of a line (entries)", &mut || {
        let r = e.find_executions(&Target::parse("state.c:111"), all, 10, 0).unwrap();
        format!("{} instructions, {} entries", r.instructions, r.entries.total)
    });
    once("executions of a function (calls)", &mut || {
        let r = e.find_executions(&Target::parse("mix"), all, 10, 0).unwrap();
        format!("{} instructions, {} calls", r.instructions, r.entries.total)
    });
    once("exception list", &mut || format!("{} exceptions", e.exception_list(all, 10, 0).unwrap().exceptions.total));
    once("call tree, depth 6", &mut || format!("{} functions", e.call_tree(all, 6).unwrap().functions.len()));
    let mid = e.end() / 2;
    once("last write of a variable", &mut || {
        format!("{} records", e.last_write(0x2000_00b0, 4, mid).unwrap().writes.len())
    });
    once("writes to a 32-byte array", &mut || {
        format!("{} writes", e.writes_to(0x2000_00dc, 32, all, 10, 0).unwrap().writes.total)
    });
    once("diff over 1,000 instructions", &mut || {
        let d = e.diff(mid, mid + 1000, 10, 0).unwrap();
        format!("{} registers, {} memory runs", d.registers.len(), d.memory.total)
    });
    for (cond, dir) in [
        ("var(\"ticks\") == 100000", Direction::Forward),
        ("var(\"ticks\") == 100000", Direction::Backward),
        ("func == \"depth\" && reg(\"r0\") == 0", Direction::Forward),
        ("line == \"state.c:111\" && var(\"svc_calls\") > 9000", Direction::Forward),
        ("reg(\"sp\") < 0x2000ff00", Direction::Forward),
        ("var(\"result\") == 1", Direction::Forward),
    ] {
        once(&format!("find {cond} {dir:?}"), &mut || {
            let f = e.find_first(cond, all, dir).unwrap();
            format!("found {:?}, {} points evaluated", f.found.map(|p| p.index), f.evaluated)
        });
    }
}
