//! `simgdb track` writes a VCD whose signals at each truth point agree with
//! what gdb saw there through QEMU's gdbstub (tests/firmware): the PC, the
//! exception being handled (IPSR) and the function (gdb's innermost
//! non-inline frame, read as text from the VCD itself). Time stamps are the
//! trace's own (ps), or cycles times the given period.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

fn simgdb(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_simgdb")).args(args).output().unwrap();
    assert!(out.status.success(), "simgdb {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A vector value as a number (at most 64 bits).
fn number(bits: &str) -> u64 {
    u64::from_str_radix(bits, 2).unwrap()
}

/// A vector value as text: 8-bit characters, right aligned (leading zero
/// bytes are padding), as a viewer's ASCII radix shows it.
fn as_text(bits: &str) -> String {
    let pad = (8 - bits.len() % 8) % 8;
    let full = format!("{}{bits}", "0".repeat(pad));
    let bytes: Vec<u8> =
        full.as_bytes().chunks(8).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap()).collect();
    String::from_utf8(bytes.into_iter().skip_while(|&b| b == 0).collect()).unwrap()
}

/// Signal name -> list of (time, value bits), from a VCD.
fn parse_vcd(path: &Path) -> (String, BTreeMap<String, Vec<(u64, String)>>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut names = HashMap::new();
    let mut scale = String::new();
    let mut out: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    let mut now = 0;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["$timescale", n, u, "$end"] => scale = format!("{n} {u}"),
            ["$var", "wire", _, id, name, "$end"] => {
                names.insert(id.to_string(), name.to_string());
            }
            [t] if t.starts_with('#') => now = t[1..].parse().unwrap(),
            [v, id] if v.starts_with('b') => {
                out.entry(names[*id].clone()).or_default().push((now, v[1..].to_string()));
            }
            [x] if x.len() >= 2 && (x.starts_with('0') || x.starts_with('1')) && names.contains_key(&x[1..]) => {
                out.entry(names[&x[1..]].clone()).or_default().push((now, x[..1].to_string()));
            }
            _ => {}
        }
    }
    (scale, out)
}

fn at(signal: &[(u64, String)], t: u64) -> &str {
    let k = signal.partition_point(|(x, _)| *x <= t);
    &signal[k - 1].1
}

#[test]
fn track_matches_gdb() {
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(root().join("traces/state_m4.truth.json.gz")).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    let truth: serde_json::Value = serde_json::from_str(&text).unwrap();
    for (dialect, extra, unit, scale) in
        [("fm", vec![], "ps", "1 ps"), ("m4rtl", vec!["--cycle-period", "10ns"], "cycles", "1 ns")]
    {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("idx");
        let vcd = dir.path().join("sw.vcd");
        let trace = root().join(format!("traces/state_m4.{dialect}.log.gz"));
        simgdb(&[
            "index",
            trace.to_str().unwrap(),
            "--elf",
            root().join("state_m4.elf").to_str().unwrap(),
            "-o",
            idx.to_str().unwrap(),
        ]);
        let mut args = vec!["track", idx.to_str().unwrap(), "-o", vcd.to_str().unwrap(), "--mark", "state.c:111"];
        args.extend(extra);
        let msg = simgdb(&args);
        assert!(msg.contains("4748 instructions"), "{msg}");
        let (got_scale, sig) = parse_vcd(&vcd);
        assert_eq!(got_scale, scale, "{dialect}");
        // One file: nothing written next to the VCD.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "{dialect}: idx and sw.vcd only");
        // The time of each point from the index signal itself.
        let index_at: HashMap<u64, u64> = sig["index"].iter().map(|(t, i)| (number(i), *t)).collect();
        let mut checked = 0;
        for p in truth["points"].as_array().unwrap() {
            let i = p["index"].as_u64().unwrap();
            let t = index_at[&i];
            let ctx = format!("{dialect} ({unit}) index {i}");
            assert_eq!(number(at(&sig["pc"], t)), p["pc"].as_u64().unwrap(), "{ctx}: pc");
            let ipsr = p["registers"]["xpsr"].as_u64().unwrap() & 0x1ff;
            assert_eq!(number(at(&sig["exception"], t)), ipsr, "{ctx}: exception");
            let bt = p["backtrace"].as_array().unwrap();
            // Skip inlined code: gdb's innermost frame there is the inline function.
            if bt.len() > 1 && bt[0]["pc"] == bt[1]["pc"] {
                continue;
            }
            assert_eq!(as_text(at(&sig["function"], t)), bt[0]["function"].as_str().unwrap(), "{ctx}: function");
            let file = as_text(at(&sig["file"], t));
            assert!(file.ends_with(".c") || file.ends_with(".s") || file.ends_with(".S"), "{ctx}: file {file}");
            checked += 1;
        }
        assert!(checked > 60);
        // The marker is high exactly on the line's instructions.
        let marks = &sig["mark_state_c_111"];
        assert!(marks.iter().filter(|(_, v)| v == "1").count() >= 144);
    }
}

#[test]
fn cycle_traces_need_a_period() {
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("idx");
    let trace = root().join("traces/state_m4.m4rtl.log.gz");
    simgdb(&["index", trace.to_str().unwrap(), "-o", idx.to_str().unwrap()]);
    let out = Command::new(env!("CARGO_BIN_EXE_simgdb"))
        .args(["track", idx.to_str().unwrap(), "-o", dir.path().join("x.vcd").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--cycle-period"));
}

#[test]
fn time_map_from_the_config_matches_a_period() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("site.toml");
    std::fs::write(&config, "[time]\nunit = \"ns\"\nsegments = [{ from_cycle = 0, time = 0, period = 10 }]\n").unwrap();
    let trace = root().join("traces/state_m4.m4rtl.log.gz");
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    let s = |p: &Path| p.to_str().unwrap().to_string();
    simgdb(&["index", &s(&trace), "-o", &s(&a), "--config", &s(&config)]);
    simgdb(&["index", &s(&trace), "-o", &s(&b)]);
    let (va, vb) = (dir.path().join("a.vcd"), dir.path().join("b.vcd"));
    simgdb(&["track", &s(&a), "-o", &s(&va)]);
    simgdb(&["track", &s(&b), "-o", &s(&vb), "--cycle-period", "10ns"]);
    let body = |p: &Path| {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("$comment"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(body(&va), body(&vb));
    // A bad config is refused with the reason.
    std::fs::write(&config, "[time]\nunit = \"cycles\"\nsegments = []\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_simgdb"))
        .args(["index", &s(&trace), "-o", &s(&dir.path().join("c")), "--config", &s(&config)])
        .output()
        .unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("time map unit"));
}
