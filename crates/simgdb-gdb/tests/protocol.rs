//! Remote serial protocol behaviour, checked with raw packets (no gdb needed).
//! Script tests with a real arm-none-eabi-gdb are in tests/gdb.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use simgdb_engine::Engine;
use simgdb_store::{IndexOptions, build};

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/firmware")
}

struct Client {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Client {
    fn send(&mut self, payload: &str) -> String {
        let sum = payload.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        write!(self.w, "${payload}#{sum:02x}").unwrap();
        self.w.flush().unwrap();
        self.reply()
    }

    /// `monitor CMD`: console output packets (`O<hex>`) until `OK`.
    fn monitor(&mut self, cmd: &str) -> String {
        let mut text = String::new();
        let mut reply = self.send(&format!("qRcmd,{}", hex(cmd)));
        while reply != "OK" {
            assert!(reply.starts_with('O'), "{reply}");
            text.push_str(&hex_decode(&reply[1..]));
            reply = self.reply();
        }
        text
    }

    fn reply(&mut self) -> String {
        let mut byte = [0u8; 1];
        // Skip acks until the start of a packet.
        loop {
            self.r.read_exact(&mut byte).unwrap();
            if byte[0] == b'$' {
                break;
            }
        }
        let mut body = Vec::new();
        loop {
            self.r.read_exact(&mut byte).unwrap();
            if byte[0] == b'#' {
                break;
            }
            body.push(byte[0]);
        }
        let mut sum = [0u8; 2];
        self.r.read_exact(&mut sum).unwrap();
        self.w.write_all(b"+").unwrap();
        // Run-length encoding: `c*n` repeats `c` (n - 29) more times.
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < body.len() {
            if body[i] == b'*' && i + 1 < body.len() {
                let last = *out.last().unwrap();
                out.extend(std::iter::repeat_n(last, (body[i + 1] - 29) as usize));
                i += 2;
            } else {
                out.push(body[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }
}

fn hex_decode(s: &str) -> String {
    (0..s.len() / 2).map(|k| u8::from_str_radix(&s[2 * k..2 * k + 2], 16).unwrap() as char).collect()
}

#[test]
fn packets() {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join("traces/state_m4.m4rtl.log.gz"),
            elf: Some(firmware().join("state_m4.elf")),
            out: dir.path().to_path_buf(),
            ..Default::default()
        },
    )
    .unwrap();
    let check = Engine::open(dir.path()).unwrap();
    let (ticks, seed) = (check.variable("ticks").unwrap().addr, check.variable("seed").unwrap().addr);
    let notes = simgdb_store::annotations::Annotations::open(&dir.path().join("annotations.sqlite")).unwrap();
    let mut session = simgdb_gdb::Session::new(Engine::open(dir.path()).unwrap()).with_annotations(notes);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        simgdb_gdb::serve_connection(&mut session, stream).unwrap();
    });
    let stream = TcpStream::connect(addr).unwrap();
    let mut c = Client { r: BufReader::new(stream.try_clone().unwrap()), w: stream };

    let features = c.send("qSupported:swbreak+;hwbreak+;vContSupported+");
    for f in ["qXfer:features:read+", "ReverseStep+", "ReverseContinue+", "swbreak+"] {
        assert!(features.contains(f), "{f} in {features}");
    }
    let xml = c.send("qXfer:features:read:target.xml:0,ffff");
    assert!(xml.starts_with('l') || xml.starts_with('m'));
    assert!(xml.contains("org.gnu.gdb.arm.m-profile") && xml.contains("name=\"control\""));

    // Point 0: PC known, most registers not yet written (xx), SP known.
    let g = c.send("g");
    // r0-r12, sp, lr, pc, xpsr; msp, psp, primask, basepri, faultmask,
    // control, msplim, psplim (4 bytes each); d0-d15 (8 bytes); fpscr.
    assert_eq!(g.len(), 2 * (25 * 4 + 16 * 8 + 4));
    assert_eq!(&g[15 * 8..16 * 8], "44000000", "pc");
    assert_eq!(&g[0..8], "xxxxxxxx", "r0 unknown before the trace writes it");
    assert_eq!(&g[13 * 8..14 * 8], "00000120", "sp from the reset record");
    assert_eq!(c.send("p0"), "xxxxxxxx");
    assert_eq!(&g[25 * 8..], "x".repeat(2 * (16 * 8 + 4)), "this firmware never writes FP registers");
    assert_eq!(c.send("p1a"), "x".repeat(16), "d1: 8 bytes, unknown");
    assert_eq!(c.send("pf"), "44000000");

    // Memory: flash from the ELF image; RAM never written is an error.
    assert_eq!(c.send("m0,4"), "00000120");
    assert!(c.send("m20000100,4").starts_with('E'));

    // Read-only: writes are refused.
    assert!(c.send("M20000000,4:00000000").starts_with('E'));
    assert!(c.send("P0=01000000").starts_with('E'));

    // Breakpoint at SysTick_Handler (0x240), continue, step, reverse.
    assert_eq!(c.send("Z0,240,2"), "OK");
    let stop = c.send("vCont;c");
    assert!(stop.starts_with("T05") && stop.contains("swbreak"), "{stop}");
    assert_eq!(&c.send("pf"), "40020000");
    let time = c.monitor("time");
    assert!(time.contains("pc 0x240"), "{time}");
    let step = c.send("vCont;s");
    assert!(step.starts_with("S05") || step.starts_with("T05"), "{step}");
    assert_eq!(&c.send("pf"), "42020000");
    let back = c.send("bs");
    assert!(back.starts_with("S05") || back.starts_with("T05"), "{back}");
    assert_eq!(&c.send("pf"), "40020000");
    // Reverse-continue from the first hit runs to the start of the history.
    let begin = c.send("bc");
    assert!(begin.contains("replaylog:begin"), "{begin}");
    assert_eq!(&c.send("pf"), "44000000");

    // monitor goto moves the session.
    let out = c.monitor("goto 100");
    assert!(out.contains("index 100 of 4748"), "{out}");

    // Queries from GDB (answers checked against tests/queries in the engine tests).
    let hits = c.monitor("hits mix 2");
    assert!(hits.contains("708 instructions executed, 118 entries") && hits.contains("index 1034"), "{hits}");
    let found = c.monitor("find var(\"ticks\") == 5");
    assert!(found.contains("found at index 1397"), "{found}");
    let back = c.monitor("find -back pc == sym(\"Reset_Handler\")");
    assert!(back.contains("found at index 0"), "{back}");
    c.monitor("goto 1080");
    let at = c.monitor("where");
    assert!(at.contains("#0 SysTick_Handler") && at.contains("#1 build_list") && at.contains("[exception 15]"), "{at}");
    let src = c.monitor("info source");
    assert!(src.contains("registers from trace:") && src.contains("pc"), "{src}");
    let bytes = c.monitor("info source ticks");
    assert_eq!(bytes.lines().count(), 4, "{bytes}");
    assert!(bytes.contains("Trace index"), "{bytes}");
    assert!(c.monitor("bookmark first SysTick").contains("at index 1080"));
    assert!(c.monitor("bookmarks").contains("index 1080: first SysTick (gdb)"));
    assert!(c.monitor("halt").contains("always halted"));
    assert!(c.monitor("reset halt").contains("index 0 of"));
    c.monitor("goto 100");
    // Continuing past the last hit stops at the end of the history.
    assert_eq!(c.send("z0,240,2"), "OK");
    let end = c.send("vCont;c");
    assert!(end.contains("replaylog:end"), "{end}");

    watchpoints(&mut c, &check, ticks, seed);

    c.send("D");
    server.join().unwrap();
}

fn index_of(c: &mut Client) -> u64 {
    let t = c.monitor("time");
    t.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{t}"))
}

/// Z2/Z3: the stop comes before the accessing instruction (GDB on Arm steps
/// over it itself), going backward after it. Checked against the engine's
/// own write records of the same index.
fn watchpoints(c: &mut Client, check: &Engine, ticks: u64, seed: u64) {
    let writes = |from: u64, to: u64| -> Vec<u64> {
        let range = simgdb_engine::Range { from, to };
        let page = check.writes_to(ticks, 4, range, 100, 0).unwrap().writes;
        page.items.iter().filter(|w| w.kind == "write").map(|w| w.written_by).collect()
    };
    c.monitor("goto 100");
    assert_eq!(c.send(&format!("Z2,{ticks:x},4")), "OK");
    let stop = c.send("vCont;c");
    assert!(stop.starts_with("T05") && stop.contains(&format!("watch:{ticks:x}")), "{stop}");
    let i = index_of(c);
    assert_eq!(writes(100, i), vec![i], "the first write after index 100 is by instruction {i}");
    // A step with the watchpoint in place stops at the access again.
    assert!(c.send("vCont;s").contains("watch:"));
    assert_eq!(index_of(c), i);
    // GDB removes it to step over.
    assert_eq!(c.send(&format!("z2,{ticks:x},4")), "OK");
    c.send("vCont;s");
    assert_eq!(index_of(c), i + 1);
    // Backward: the stop is after the access, here at once.
    assert_eq!(c.send(&format!("Z2,{ticks:x},4")), "OK");
    assert!(c.send("bc").contains("watch:"));
    assert_eq!(index_of(c), i + 1);
    assert_eq!(c.send(&format!("z2,{ticks:x},4")), "OK");
    // Read watchpoint on `seed` (read, never written after startup).
    assert_eq!(c.send(&format!("Z3,{seed:x},4")), "OK");
    let stop = c.send("vCont;c");
    assert!(stop.contains(&format!("rwatch:{seed:x}")), "{stop}");
    assert!(index_of(c) > i);
    assert_eq!(c.send(&format!("z3,{seed:x},4")), "OK");
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// `serve --end-as-pause`: continuing to either end of the trace stops with
/// a plain trap (IDEs show "paused"); stepping there still reports "no more
/// history", because GDB takes a trap that left no step range as "step
/// again" and would loop.
#[test]
fn end_as_pause() {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: firmware().join("traces/state_m4.m4rtl.log.gz"),
            elf: Some(firmware().join("state_m4.elf")),
            out: dir.path().to_path_buf(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut session = simgdb_gdb::Session::new(Engine::open(dir.path()).unwrap()).with_end_as_pause(true);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        simgdb_gdb::serve_connection(&mut session, stream).unwrap();
    });
    let stream = TcpStream::connect(addr).unwrap();
    let mut c = Client { r: BufReader::new(stream.try_clone().unwrap()), w: stream };
    c.send("qSupported:swbreak+;hwbreak+;vContSupported+");

    let begin = c.send("bc");
    assert!(begin.starts_with("S05") || begin.starts_with("T05"), "{begin}");
    assert!(!begin.contains("replaylog"), "{begin}");
    let end = c.send("vCont;c");
    assert!((end.starts_with("S05") || end.starts_with("T05")) && !end.contains("replaylog"), "{end}");
    assert!(c.monitor("time").contains("(last 4747)"));
    let step = c.send("vCont;s");
    assert!(step.contains("replaylog:end"), "{step}");
    let back = c.send("bs");
    assert!(!back.contains("replaylog"), "{back}");
}
