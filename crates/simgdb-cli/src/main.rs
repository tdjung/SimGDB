//! `simgdb`: index traces and query them. JSON on stdout, messages on stderr.

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
    },
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
    /// Points at which a PC executed.
    Executions {
        #[arg(long, value_parser = parse_u64)]
        pc: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
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

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Index { trace, elf, out, isa, core, checkpoint_interval, report, mask, strict } => {
            let isa = simgdb_isa::by_name(&isa).ok_or_else(|| format!("unknown ISA {isa}"))?;
            let started = std::time::Instant::now();
            let meta = build(
                isa.as_ref(),
                &IndexOptions { trace, elf, out: out.clone(), core, checkpoint_interval, mask_samples: mask, strict },
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
        Command::Serve { index, listen, at, once } => {
            let engine = Engine::open(&index).map_err(|e| e.to_string())?;
            let mut session = simgdb_gdb::Session::new(engine);
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
                Query::Executions { pc, limit } => {
                    let all: Vec<u64> = engine.executions(pc).collect();
                    json!({"pc": pc, "count": all.len(), "indexes": &all[..all.len().min(limit)], "truncated": all.len() > limit})
                }
            };
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            Ok(())
        }
    }
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
