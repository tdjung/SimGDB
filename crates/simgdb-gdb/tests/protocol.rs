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
    let mut session = simgdb_gdb::Session::new(Engine::open(dir.path()).unwrap());
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
    assert_eq!(g.len(), 23 * 8);
    assert_eq!(&g[15 * 8..16 * 8], "44000000", "pc");
    assert_eq!(&g[0..8], "xxxxxxxx", "r0 unknown before the trace writes it");
    assert_eq!(&g[13 * 8..14 * 8], "00000120", "sp from the reset record");
    assert_eq!(c.send("p0"), "xxxxxxxx");
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
    // Continuing past the last hit stops at the end of the history.
    assert_eq!(c.send("z0,240,2"), "OK");
    let end = c.send("vCont;c");
    assert!(end.contains("replaylog:end"), "{end}");

    c.send("D");
    server.join().unwrap();
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}
