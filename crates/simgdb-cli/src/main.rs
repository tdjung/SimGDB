//! `simgdb`: index traces and query them. JSON on stdout, messages on stderr.

mod vcd;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::json;
use simgdb_engine::Engine;
use simgdb_store::{IndexOptions, build};

#[derive(Parser)]
#[command(name = "simgdb", version, about = "Post-simulation debugger for trace logs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build an index from a Tarmac trace (plain or gzip).
    Index {
        trace: PathBuf,
        /// Program ELF: initial memory image and symbols.
        #[arg(long)]
        elf: Option<PathBuf>,
        /// Output directory for the index.
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long, default_value = "arm-m")]
        isa: String,
        /// Core to index when lines carry `cpuN` (default: the first seen).
        #[arg(long)]
        core: Option<u32>,
        #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u64).range(1..))]
        checkpoint_interval: u64,
        /// Print the diagnostics report (record counts, unrecognised lines, capabilities) as JSON.
        #[arg(long)]
        report: bool,
        /// Hide addresses and data in report samples.
        #[arg(long)]
        mask: bool,
        /// Stop at the first line that does not parse.
        #[arg(long)]
        strict: bool,
        /// Site configuration (TOML): Tarmac notation, register aliases, memory regions, time map. See docs/USAGE.md.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Memory for each index sort before it spills to disk (MiB); the index directory needs room for the spill.
        #[arg(long, default_value_t = 384)]
        sort_memory_mb: usize,
        /// Data byte order: little or big (BE8). Default: the ELF header, else little.
        #[arg(long, value_parser = parse_endian)]
        endian: Option<simgdb_isa::Endian>,
    },
    /// Dataset information: size, time unit, capabilities, limitations.
    Info { index: PathBuf },
    /// Serve the index to GDB (remote serial protocol over TCP).
    Serve {
        index: PathBuf,
        /// Address to listen on. Use 0.0.0.0 to accept connections from other hosts.
        #[arg(long, default_value = "127.0.0.1:3333")]
        listen: String,
        /// Start at this instruction index.
        #[arg(long, default_value_t = 0)]
        at: u64,
        /// Exit after the first GDB session ends.
        #[arg(long)]
        once: bool,
        /// Arm M-profile: do not let GDB depend on MSP/PSP when either is
        /// unknown somewhere (before the first PSP write). Stack queries
        /// there no longer fail (IDE front ends end the session on them),
        /// but backtraces through exception frames on PSP become unreliable.
        /// See docs/USAGE.md.
        #[arg(long)]
        no_psp_unwind: bool,
        /// Report reaching an end of the trace as a plain stop, which IDEs
        /// show as paused (VS Code cppdbg shows GDB's "no more history" as an
        /// exception).
        #[arg(long)]
        end_as_pause: bool,
    },
    /// Serve the index to AI clients over MCP (stdio).
    Mcp {
        index: PathBuf,
        /// Bookmarks and the tool-call log (default: annotations.sqlite in the index directory).
        #[arg(long)]
        annotations: Option<PathBuf>,
    },
    /// Write the SW track as VCD (function, line, PC, call depth, exception, marker lines) to view with HW waveforms.
    Track {
        index: PathBuf,
        /// Output VCD file. `<out>.names.csv` (function/file ids) and `<out>.gtkw-filter` are written next to it.
        #[arg(short, long)]
        out: PathBuf,
        #[command(flatten)]
        range: Span,
        /// A line (`file.c:123`), function or PC to show as a 1-bit signal; repeat for more.
        #[arg(long = "mark")]
        marks: Vec<String>,
        /// For traces timed in cycles: the clock period, e.g. `10ns` (time = cycles x period).
        #[arg(long)]
        cycle_period: Option<String>,
        /// Use the instruction index as time instead of the trace's time stamps.
        #[arg(long)]
        index_time: bool,
    },
    /// Print the skill document for AI clients (save as .claude/skills/simgdb-debug/SKILL.md).
    Skill,
    /// Query the state at a point.
    Query {
        index: PathBuf,
        #[command(subcommand)]
        what: Query,
    },
}

#[derive(Args, Clone, Copy)]
struct At {
    /// Point as an instruction index (state before that instruction).
    #[arg(long, conflicts_with = "time")]
    at: Option<u64>,
    /// Point as a trace time stamp (first instruction at or after it).
    #[arg(long)]
    time: Option<u64>,
}

#[derive(Subcommand)]
enum Query {
    /// All registers.
    Registers(At),
    /// PC, source line (with inline frames) and call stack.
    Locate(At),
    /// Memory bytes.
    Memory {
        #[arg(long, value_parser = parse_u64)]
        addr: u64,
        #[arg(long, default_value_t = 16)]
        len: u64,
        #[command(flatten)]
        at: At,
    },
    /// Where a PC, line (`file.c:123`) or function executed: count and points.
    Executions {
        target: String,
        #[command(flatten)]
        range: Span,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Exceptions entered in a range.
    Exceptions {
        /// Only this kind: a name (SysTick, IRQ3) or number.
        #[arg(long)]
        kind: Option<String>,
        #[command(flatten)]
        range: Span,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Calls and exceptions in a range as a tree, plus per-function totals.
    Calls {
        #[command(flatten)]
        range: Span,
        #[arg(long, default_value_t = 6)]
        depth: u32,
    },
    /// Which records last wrote each byte, as seen at a point.
    LastWrite {
        #[command(flatten)]
        loc: Location,
        #[command(flatten)]
        at: At,
    },
    /// Writes to an address range by instructions in a range.
    Writes {
        #[command(flatten)]
        loc: Location,
        #[command(flatten)]
        range: Span,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Writes of one register in a range.
    RegHistory {
        name: String,
        #[command(flatten)]
        range: Span,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Registers and memory that differ between two points.
    Diff {
        #[arg(long, value_parser = parse_u64)]
        a: u64,
        #[arg(long, value_parser = parse_u64)]
        b: u64,
        #[command(flatten)]
        page: PageArgs,
    },
    /// First point where a condition holds (language: docs/USAGE.md).
    Find {
        condition: String,
        #[command(flatten)]
        range: Span,
        /// Search backward: the last point at or before --to.
        #[arg(long)]
        backward: bool,
    },
    /// A variable's value (`name`, `name[3]`, `name.member`).
    Var {
        path: String,
        #[command(flatten)]
        at: At,
    },
    /// All fixed-address variables with address, size and type.
    Variables,
}

#[derive(Args, Clone, Copy)]
struct Span {
    /// First point (default 0).
    #[arg(long, value_parser = parse_u64)]
    from: Option<u64>,
    /// Last point (default: the end).
    #[arg(long, value_parser = parse_u64)]
    to: Option<u64>,
}

#[derive(Args, Clone, Copy)]
struct PageArgs {
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// `next_cursor` of the previous page.
    #[arg(long, default_value_t = 0)]
    cursor: u64,
}

#[derive(Args, Clone)]
struct Location {
    #[arg(long, value_parser = parse_u64, conflicts_with = "var")]
    addr: Option<u64>,
    /// Variable instead of an address (its size is the length).
    #[arg(long)]
    var: Option<String>,
    #[arg(long)]
    len: Option<u64>,
}

fn location(engine: &Engine, loc: &Location) -> Result<(u64, u64), String> {
    match (&loc.var, loc.addr) {
        (Some(v), _) => {
            let p = engine.variable(v).map_err(|e| e.to_string())?;
            Ok((p.addr, loc.len.or(p.size).ok_or("variable size unknown; give --len")?))
        }
        (None, Some(a)) => Ok((a, loc.len.unwrap_or(4))),
        (None, None) => Err("give --addr or --var".into()),
    }
}

fn parse_u64(s: &str) -> Result<u64, String> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|e| e.to_string())
}

fn resolve(engine: &Engine, at: At) -> Result<u64, String> {
    match (at.at, at.time) {
        (Some(i), _) => Ok(i),
        (None, Some(t)) => engine.index_at_time(t).ok_or_else(|| format!("no instruction at or after time {t}")),
        (None, None) => Err("give --at INDEX or --time TIME".into()),
    }
}

fn parse_endian(s: &str) -> Result<simgdb_isa::Endian, String> {
    simgdb_isa::Endian::parse(s).ok_or_else(|| format!("{s}: expected little or big"))
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Index {
            trace,
            elf,
            out,
            isa,
            core,
            checkpoint_interval,
            report,
            mask,
            strict,
            config,
            sort_memory_mb,
            endian,
        } => {
            let config = match config {
                Some(p) => Some(simgdb_store::config::SiteConfig::load(&p).map_err(|e| e.to_string())?),
                None => None,
            };
            let isa = simgdb_isa::by_name(&isa).ok_or_else(|| format!("unknown ISA {isa}"))?;
            let started = std::time::Instant::now();
            let meta = build(
                isa.as_ref(),
                &IndexOptions {
                    trace,
                    elf,
                    out: out.clone(),
                    core,
                    checkpoint_interval,
                    mask_samples: mask,
                    strict,
                    sort_records: sort_memory_mb.max(1) * (1 << 20) / 24,
                    config,
                    endian,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
            let r = &meta.report;
            eprintln!(
                "indexed {} instructions from {} lines in {:.2} s into {}",
                meta.instructions,
                r.lines,
                started.elapsed().as_secs_f64(),
                out.display()
            );
            if meta.endian == simgdb_isa::Endian::Big || r.endian_conflict.is_some() {
                eprintln!(
                    "data byte order: {} (from {})",
                    meta.endian.name(),
                    r.endian_source.as_deref().unwrap_or("?")
                );
            }
            if let Some(c) = &r.endian_conflict {
                eprintln!("warning: {c}");
            }
            if r.parse_errors.count > 0 || !r.unrecognised.is_empty() {
                eprintln!(
                    "warning: {} lines did not parse, {} record types not recognised (see --report)",
                    r.parse_errors.count,
                    r.unrecognised.len()
                );
            }
            if report {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"capabilities": meta.capabilities, "report": meta.report}))
                        .unwrap()
                );
            }
            Ok(())
        }
        Command::Serve { index, listen, at, once, no_psp_unwind, end_as_pause } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            let mut session =
                simgdb_gdb::Session::with_stack_registers(engine, no_psp_unwind).with_end_as_pause(end_as_pause);
            match simgdb_store::annotations::Annotations::open(&index.join("annotations.sqlite")) {
                Ok(a) => session = session.with_annotations(a),
                Err(e) => eprintln!("simgdb: bookmarks disabled ({e})"),
            }
            session.set_index(at);
            let listener = std::net::TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
            eprintln!(
                "simgdb: waiting for gdb on {listen} (target extended-remote is not needed: use `target remote {listen}`)"
            );
            simgdb_gdb::serve(&mut session, listener, once)
        }
        Command::Info { index } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&engine.info()).unwrap());
            Ok(())
        }
        Command::Track { index, out, range, marks, cycle_period, index_time } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            let unit = engine.info().time_unit;
            let time = match (index_time, &cycle_period, &unit) {
                (true, _, _) => vcd::Time::Index,
                (false, Some(p), Some(simgdb_model::TimeUnit::Cycle(_))) => {
                    let split =
                        p.find(|c: char| !c.is_ascii_digit()).ok_or("--cycle-period wants a unit, e.g. 10ns")?;
                    let (n, u) = p.split_at(split);
                    if !["fs", "ps", "ns", "us", "ms", "s"].contains(&u) {
                        return Err(format!("--cycle-period unit must be fs, ps, ns, us, ms or s, not {u}"));
                    }
                    vcd::Time::Cycles { period: n.parse().map_err(|_| format!("bad period {p}"))?, unit: u.into() }
                }
                (false, None, Some(simgdb_model::TimeUnit::Cycle(_))) if engine.time_map().is_some() => {
                    vcd::Time::Map(engine.time_map().unwrap().clone())
                }
                (false, None, Some(simgdb_model::TimeUnit::Cycle(u))) => {
                    return Err(format!(
                        "this trace is timed in cycles ({u}); give --cycle-period (e.g. 10ns), a [time] map in the \
                         index --config file, or --index-time"
                    ));
                }
                (false, Some(_), _) => return Err("--cycle-period is for traces timed in cycles".into()),
                (false, None, Some(simgdb_model::TimeUnit::Time(u))) => vcd::Time::Trace { unit: u.clone() },
                (false, None, _) => return Err("this trace has no time stamps; use --index-time".into()),
            };
            let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
            let targets: Vec<simgdb_engine::Target> = marks.iter().map(|m| simgdb_engine::Target::parse(m)).collect();
            let (rows, names) = engine.track(r, &targets).map_err(|e| e.to_string())?;
            let pc_bits = 64 - (engine.isa().address_limit() - 1).leading_zeros();
            let file = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            let mut w = std::io::BufWriter::new(file);
            let comment = format!("SW track of {}", engine.info().trace);
            let s = vcd::write(&mut w, rows, &marks, &time, pc_bits, &comment).map_err(|e| e.to_string())?;
            std::io::Write::flush(&mut w).map_err(|e| e.to_string())?;
            let side = |ext: &str| {
                let mut p = out.clone().into_os_string();
                p.push(ext);
                PathBuf::from(p)
            };
            let mut csv = std::fs::File::create(side(".names.csv")).map_err(|e| e.to_string())?;
            vcd::write_names(&mut csv, &names).map_err(|e| e.to_string())?;
            let mut filt = std::fs::File::create(side(".gtkw-filter")).map_err(|e| e.to_string())?;
            vcd::write_gtkwave_filter(&mut filt, &names).map_err(|e| e.to_string())?;
            eprintln!(
                "wrote {} ({} instructions; {} share a time stamp with the previous one, {} went backwards, \
                 {} left out without a time)",
                out.display(),
                s.samples,
                s.same_time,
                s.backwards,
                s.untimed
            );
            Ok(())
        }
        Command::Skill => {
            print!("{}", simgdb_mcp::SKILL);
            Ok(())
        }
        Command::Mcp { index, annotations } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            let path = annotations.unwrap_or_else(|| index.join("annotations.sqlite"));
            let notes = match simgdb_store::annotations::Annotations::open(&path) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("simgdb mcp: bookmarks and call log disabled ({}: {e})", path.display());
                    None
                }
            };
            let server = simgdb_mcp::SimGdbMcp::new(std::sync::Arc::new(engine), notes);
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
            rt.block_on(simgdb_mcp::serve_stdio(server)).map_err(|e| e.to_string())
        }
        Command::Query { index, what } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            let out = match what {
                Query::Registers(at) => {
                    let i = resolve(&engine, at)?;
                    json!({"point": engine.point(i).map_err(|e| e.to_string())?,
                           "registers": engine.registers(i).map_err(|e| e.to_string())?})
                }
                Query::Locate(at) => {
                    let i = resolve(&engine, at)?;
                    serde_json::to_value(engine.locate(i).map_err(|e| e.to_string())?).unwrap()
                }
                Query::Memory { addr, len, at } => {
                    let i = resolve(&engine, at)?;
                    let bytes = engine.memory(addr, len, i).map_err(|e| e.to_string())?;
                    let hex: String = bytes
                        .iter()
                        .map(|b| b.value.map(|v| format!("{v:02x}")).unwrap_or_else(|| "??".into()))
                        .collect();
                    json!({"point": engine.point(i).map_err(|e| e.to_string())?, "addr": addr, "hex": hex, "bytes": bytes})
                }
                Query::Executions { target, range, page } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    let t = simgdb_engine::Target::parse(&target);
                    to_json(engine.find_executions(&t, r, page.limit, page.cursor))?
                }
                Query::Exceptions { kind, range, page } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    to_json(engine.exception_list(r, kind.as_deref(), page.limit, page.cursor))?
                }
                Query::Calls { range, depth } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    to_json(engine.call_tree(r, depth))?
                }
                Query::LastWrite { loc, at } => {
                    let i = resolve(&engine, at)?;
                    let (addr, len) = location(&engine, &loc)?;
                    to_json(engine.last_write(addr, len, i))?
                }
                Query::Writes { loc, range, page } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    let (addr, len) = location(&engine, &loc)?;
                    to_json(engine.writes_to(addr, len, r, page.limit, page.cursor))?
                }
                Query::RegHistory { name, range, page } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    to_json(engine.register_history(&name, r, page.limit, page.cursor))?
                }
                Query::Diff { a, b, page } => to_json(engine.diff(a, b, page.limit, page.cursor))?,
                Query::Find { condition, range, backward } => {
                    let r = engine.range(range.from, range.to).map_err(|e| e.to_string())?;
                    let d =
                        if backward { simgdb_engine::Direction::Backward } else { simgdb_engine::Direction::Forward };
                    to_json(engine.find_first(&condition, r, d))?
                }
                Query::Var { path, at } => {
                    let i = resolve(&engine, at)?;
                    to_json(engine.read_variable(&path, i))?
                }
                Query::Variables => {
                    let v = engine.variables();
                    let list: Vec<_> = v
                        .variables
                        .iter()
                        .map(|x| json!({"name": x.name, "addr": x.addr, "size": x.size, "type": v.name(x.ty)}))
                        .collect();
                    json!({"variables": list})
                }
            };
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            Ok(())
        }
    }
}

fn to_json<T: serde::Serialize>(r: Result<T, simgdb_engine::EngineError>) -> Result<serde_json::Value, String> {
    r.map(|v| serde_json::to_value(v).unwrap()).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
