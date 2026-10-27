//! Cross-check against tarmac-trace-utilities' own parse of real-world lines
//! (tests/fixtures/README.md). Instruction and memory events must match field
//! by field; register, exception and text-only events must map to the same
//! kind of record.

use simgdb_tarmac::{Event, Parser, Record};

/// Lines where SimGDB deliberately differs. In `MR8 addr:phys_RL Device-nGnRnE
/// (StronglyOrdered) data` the reference takes the attribute word as the data
/// (and so reports it unknown); SimGDB takes the last token, the actual data.
const KNOWN_DIFFERENCES: &[&str] = &["Device-nGnRnE (StronglyOrdered)"];

struct Case {
    line: String,
    expected: Vec<String>,
}

fn cases() -> Vec<Case> {
    let text = include_str!("fixtures/ttu_parsertest.ref");
    let mut out: Vec<Case> = Vec::new();
    for l in text.lines() {
        if let Some(line) = l.strip_prefix("--- Tarmac line: ") {
            out.push(Case { line: line.to_string(), expected: vec![] });
        } else if let Some(ev) = l.strip_prefix("* ") {
            out.last_mut().unwrap().expected.push(ev.to_string());
        }
    }
    out
}

fn field<'a>(event: &'a str, name: &str) -> &'a str {
    let key = format!("{name}=");
    let start = event.find(&key).unwrap_or_else(|| panic!("{name} in {event}")) + key.len();
    event[start..].split(' ').next().unwrap()
}

fn hexv(s: &str) -> u64 {
    u64::from_str_radix(s, 16).unwrap()
}

#[test]
fn matches_tarmac_trace_utilities() {
    let mut parser = Parser::new();
    let (mut checked, mut failures) = (0, Vec::new());
    for case in cases() {
        let parsed = parser.parse(&case.line);
        let record = match parsed {
            Ok(l) => l.record,
            Err(e) => {
                if !case.expected.is_empty() {
                    failures.push(format!("{}: parse error {e}", case.line));
                }
                continue;
            }
        };
        let kinds: Vec<&str> = case.expected.iter().map(|e| e.split(' ').next().unwrap()).collect();
        match &record {
            Record::Instruction(i) => {
                if kinds != ["InstructionEvent"] {
                    failures.push(format!("{}: got instruction, expected {kinds:?}", case.line));
                    continue;
                }
                let e = &case.expected[0];
                let effect = match (i.opcode, i.failed, i.event) {
                    (None, _, _) => "fetchfail",
                    (_, true, _) | (_, _, Event::Is) => "ccfail",
                    _ => "executed",
                };
                let mut ok = hexv(field(e, "pc")) == i.pc && field(e, "effect") == effect;
                if let Some(op) = i.opcode {
                    ok &= hexv(field(e, "instruction")) == op as u64;
                    ok &= field(e, "width").parse::<u8>().unwrap() == i.width * 8;
                }
                if !ok {
                    failures.push(format!("{}: {i:?} vs {e}", case.line));
                }
            }
            Record::Memory(m) => {
                if !kinds.iter().all(|k| *k == "MemoryEvent") || kinds.is_empty() {
                    failures.push(format!("{}: got {} accesses, expected {kinds:?}", case.line, m.len()));
                    continue;
                }
                // Compare byte by byte: the reference groups diagram bytes
                // into accesses from the other end.
                if KNOWN_DIFFERENCES.iter().any(|d| case.line.contains(d)) {
                    checked += 1;
                    continue;
                }
                let mut ours = std::collections::BTreeMap::new();
                for a in m.iter() {
                    for k in 0..a.size as u64 {
                        let known = a.known & (1 << k) != 0;
                        ours.insert(a.addr + k, (a.write, known.then_some((a.value >> (8 * k)) as u8)));
                    }
                }
                let mut theirs = std::collections::BTreeMap::new();
                for e in &case.expected {
                    let (addr, size) = (hexv(field(e, "addr")), field(e, "size").parse::<u64>().unwrap());
                    let known = field(e, "known") == "true";
                    let value = hexv(field(e, "contents"));
                    for k in 0..size {
                        theirs
                            .insert(addr + k, (field(e, "read") == "false", known.then_some((value >> (8 * k)) as u8)));
                    }
                }
                if ours != theirs {
                    failures.push(format!("{}: {ours:x?} vs {theirs:x?}", case.line));
                }
            }
            Record::Register(_) => {
                if kinds != ["RegisterEvent"] && !kinds.iter().all(|k| *k == "RegisterEvent") {
                    failures.push(format!("{}: got register, expected {kinds:?}", case.line));
                }
            }
            Record::Exception { .. } => {
                if kinds != ["ExceptionEvent"] {
                    failures.push(format!("{}: got exception, expected {kinds:?}", case.line));
                }
            }
            Record::Ignored(_) | Record::Unrecognised(_) | Record::Header | Record::Blank | Record::BusCopy(_) => {
                // The reference shows these as text-only events, nothing, or
                // (for register lines without a value) no-op register events.
                let fine = kinds.iter().all(|k| *k == "TextOnlyEvent")
                    || case.line.contains(" R ") && kinds.iter().all(|k| *k == "RegisterEvent");
                if !fine {
                    failures.push(format!("{}: got {record:?}, expected {kinds:?}", case.line));
                }
            }
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{} of {checked} lines differ:\n{}", failures.len(), failures.join("\n"));
    assert!(checked > 150, "only {checked} lines checked");
}
