#!/usr/bin/env python3
"""Script tests for the SimGDB GDB server with a real arm-none-eabi-gdb.

The same gdb command sequence runs against two targets and the outputs are
compared:

  expected  QEMU's gdbstub, replaying a recorded run of the firmware (the run
            the traces in tests/firmware/traces were made from). The output is
            committed under expected/<core>/<scenario>.txt. Needs QEMU.
  check     `simgdb serve` on an index of one of those traces. Needs only the
            simgdb binary and gdb.

Scenario files (scenarios/*.gdb) hold one gdb command per line, plus:
  @cores A B      run this scenario only for these cores (first such line)
  @dump LABEL     print a labelled state block (registers, backtrace, args, locals)
  @reverse CMD    run CMD on SimGDB only (QEMU has no reverse execution); every
                  labelled block printed after a reverse command must equal the
                  block with the same label from the forward run.

A difference is accepted only where SimGDB reports a value as unknown
(`<unavailable>`, `Cannot access memory`, a backtrace that stops because
memory is unknown). Any other difference fails: SimGDB must never show a
value that differs from the real target.

Usage:
  gdb_tests.py expected --core m4 [--qemu Q]
  gdb_tests.py check --core m4 --dialect m4rtl --simgdb target/release/simgdb [--gdb G]
"""

import argparse
import difflib
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
FIRMWARE = HERE.parent / "firmware"
# m4be: the m4 firmware built big-endian (BE8). Its expected output needs a
# QEMU with tests/firmware/qemu/qemu-m-bigend.patch (run with QEMU_ARM_M_BIGEND).
MACHINES = {"m4": ("mps2-an386", "cortex-m4"), "m35p": ("mps2-an505", "cortex-m33"),
            "m4be": ("mps2-an386", "cortex-m4"),
            # m4f: the m4 firmware built hard-float with tests/firmware/src/fp.c.
            "m4f": ("mps2-an386", "cortex-m4")}
ICOUNT = "shift=5,align=off,sleep=off"
DUMP = ["info registers r0 r1 r2 r3 r4 r5 r6 r7 r8 r9 r10 r11 r12 sp lr pc xpsr msp psp control",
        "bt", "info args", "info locals"]
# GDB reads FPCCR (memory-mapped, 0xE000EF34) to unwind an extended (FP)
# exception frame; SimGDB does not know that peripheral register's value.
UNKNOWN = re.compile(r"<unavailable>|Cannot access memory|<error: |Backtrace stopped|is not available|"
                     r"Could not fetch required FPCCR content")
# GDB prints `x=x@entry=V` when it can prove the value equals the entry value,
# which needs registers SimGDB may not know; `x=V` says the same value.
ENTRY = re.compile(r"\b(\w+)=\1@entry=")
NOISE = re.compile(r"^(Remote debugging using|\[Inferior \d|Kill the program|Detaching from|Ending remote|"
                   r"\[Switching to|warning: (?!Could not fetch required FPCCR)|A program is being debugged)")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_port(port, proc):
    for _ in range(100):
        if proc.poll() is not None:
            raise RuntimeError("target exited early")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
            return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("target did not listen on port {}".format(port))


def scenarios(core):
    """Scenario files that apply to `core` (see `@cores`)."""
    out = []
    for path in sorted((HERE / "scenarios").glob("*.gdb")):
        cores = [l.split()[1:] for l in path.read_text().splitlines() if l.startswith("@cores ")]
        if not cores or core in cores[0]:
            out.append(path)
    return out


def commands(scenario, reverse):
    out = []
    marked = False
    for line in scenario.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("@cores "):
            continue
        if line.startswith("@reverse "):
            if not reverse:
                continue
            if not marked:
                out.append("echo ### reverse\\n")
                marked = True
            line = line[len("@reverse "):]
        if line.startswith("@dump "):
            out.append("echo === {}\\n".format(line.split()[1]))
            out.extend(DUMP)
            out.append("echo === /\\n")
        else:
            out.append(line)
    return out


def run_gdb(gdb, elf, port, cmds, final):
    args = [gdb, "-q", "-nx", "-batch", str(elf),
            "-ex", "set pagination off", "-ex", "set confirm off", "-ex", "set width 0",
            "-ex", "set height 0", "-ex", "target remote :{}".format(port)]
    for c in cmds + [final]:
        args += ["-ex", c]
    r = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True, timeout=300)
    return "\n".join(ENTRY.sub(r"\1=", l.rstrip()) for l in r.stdout.splitlines() if not NOISE.match(l)) + "\n"


def expected(args):
    elf = FIRMWARE / "state_{}.elf".format(args.core)
    machine, cpu = MACHINES[args.core]
    out_dir = HERE / "expected" / args.core
    out_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        rr = Path(tmp) / "run.rr"
        base = [args.qemu, "-M", machine, "-cpu", cpu, "-nographic", "-monitor", "none", "-serial", "none",
                "-semihosting-config", "enable=on,target=native", "-kernel", str(elf),
                "-accel", "tcg,one-insn-per-tb=on"]
        env = dict(os.environ)
        if args.core.endswith("be"):
            env["QEMU_ARM_M_BIGEND"] = "1"
        subprocess.run(base + ["-icount", "{},rr=record,rrfile={}".format(ICOUNT, rr)], check=True, timeout=300,
                       env=env)
        for scenario in scenarios(args.core):
            port = free_port()
            q = subprocess.Popen(base + ["-icount", "{},rr=replay,rrfile={}".format(ICOUNT, rr),
                                         "-gdb", "tcp::{}".format(port), "-S"], stderr=subprocess.DEVNULL, env=env)
            try:
                wait_port(port, q)
                text = run_gdb(args.gdb, elf, port, commands(scenario, reverse=False), "kill")
            finally:
                q.kill()
                q.wait()
            (out_dir / (scenario.stem + ".txt")).write_text(text)
            print("expected/{}/{}.txt: {} lines".format(args.core, scenario.stem, text.count("\n")))
    return 0


def blocks(text):
    """label -> list of block texts, in order."""
    out = {}
    label, lines = None, []
    for line in text.splitlines() + ["=== /"]:
        if line.startswith("=== ") or line.startswith("### "):
            if label is not None:
                out.setdefault(label, []).append(lines)
            name = line[4:] if line.startswith("=== ") else None
            label, lines = (None if name == "/" else name), []
        elif label is not None:
            lines.append(line)
    return out


def psp_unknown(lines):
    """The most recent register dump shows PSP as unavailable."""
    for line in reversed(lines):
        if line.startswith("psp "):
            return "<unavailable>" in line
    return False


def before_backtrace_unknown(lines, end):
    """GDB warned about something unknown just before printing the backtrace
    that ends at `end` (it warns before frame #0, e.g. about FPCCR)."""
    k = end
    while k > 0 and lines[k - 1].startswith("#"):
        k -= 1
    return k > 0 and k < end and bool(UNKNOWN.search(lines[k - 1]))


def compare(want, got, truncated_ok):
    """(failures, tolerated) between two line lists. With `truncated_ok`, a
    backtrace may end early (the trace has no exception stacking records, so
    the frame below a handler is unknown)."""
    failures, tolerated = [], 0
    sm = difflib.SequenceMatcher(a=want, b=got, autojunk=False)
    for op, a0, a1, b0, b1 in sm.get_opcodes():
        if op == "equal":
            continue
        theirs, ours = want[a0:a1], got[b0:b1]
        if ours and all(UNKNOWN.search(l) for l in ours):
            tolerated += len(ours)
            continue
        # Frames missing after our backtrace stopped on unknown memory, or
        # (some gdb builds stop silently) below a handler while PSP is still
        # unknown: GDB's M-profile unwinder needs PSP for every exception frame.
        if op == "delete" and all(l.startswith("#") for l in theirs) and b0 > 0 and (
                UNKNOWN.search(got[b0 - 1]) or before_backtrace_unknown(got, b0)
                or (got[b0 - 1].startswith("#") and (truncated_ok or psp_unknown(got[:b0])))):
            tolerated += len(theirs)
            continue
        failures.append("  qemu:    " + "\n           ".join(theirs or ["(nothing)"]) +
                        "\n  simgdb:  " + "\n           ".join(ours or ["(nothing)"]))
    return failures, tolerated


def check(args):
    elf = FIRMWARE / "state_{}.elf".format(args.core)
    trace = FIRMWARE / "traces" / "state_{}.{}.log.gz".format(args.core, args.dialect)
    total_fail, total_tol = 0, 0
    with tempfile.TemporaryDirectory() as tmp:
        idx = Path(tmp) / "idx"
        subprocess.run([args.simgdb, "index", str(trace), "--elf", str(elf), "-o", str(idx)], check=True,
                       stderr=subprocess.DEVNULL)
        for scenario in scenarios(args.core):
            want_text = (HERE / "expected" / args.core / (scenario.stem + ".txt")).read_text()
            port = free_port()
            s = subprocess.Popen([args.simgdb, "serve", str(idx), "--listen", "127.0.0.1:{}".format(port), "--once"],
                                 stderr=subprocess.PIPE, universal_newlines=True)
            try:
                # The server accepts one connection (--once): wait for its
                # "waiting for gdb" line instead of probing the port.
                if "waiting for gdb" not in s.stderr.readline():
                    raise RuntimeError("simgdb serve did not start")
                got_text = run_gdb(args.gdb, elf, port, commands(scenario, reverse=True), "detach")
            finally:
                s.wait(timeout=30)
            if args.save:
                Path(args.save).mkdir(parents=True, exist_ok=True)
                (Path(args.save) / "{}.{}.{}.txt".format(args.core, args.dialect, scenario.stem)).write_text(got_text)
            forward, _, reverse = got_text.partition("### reverse\n")
            # Only traces without exception stacking stores may end a
            # backtrace at a handler frame.
            truncated_ok = args.dialect != "m4rtl"
            failures, tolerated = compare(want_text.splitlines(), forward.splitlines(), truncated_ok)
            if reverse:
                want_blocks = blocks(want_text)
                for label, occurrences in blocks("### \n" + reverse).items():
                    for got_block in occurrences:
                        if label not in want_blocks:
                            failures.append("  reverse block {} has no forward counterpart".format(label))
                            continue
                        f, t = compare(want_blocks[label][0], got_block, truncated_ok)
                        failures += ["  [{} after reverse]\n{}".format(label, x) for x in f]
                        tolerated += t
            status = "ok" if not failures else "FAIL"
            print("{} {} {}: {} ({} lines differ only as unknown)".format(
                args.core, args.dialect, scenario.stem, status, tolerated))
            for f in failures[:20]:
                print(f)
            total_fail += len(failures)
            total_tol += tolerated
    return 1 if total_fail else 0


def main(argv):
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode")
    sub.required = True  # Python 3.6 has no required= argument here
    e = sub.add_parser("expected")
    e.add_argument("--core", choices=MACHINES, required=True)
    e.add_argument("--qemu", default=os.environ.get("QEMU", "qemu-system-arm"))
    e.add_argument("--gdb", default=os.environ.get("GDB", "arm-none-eabi-gdb"))
    c = sub.add_parser("check")
    c.add_argument("--core", choices=MACHINES, required=True)
    c.add_argument("--dialect", choices=("fm", "m4rtl", "es"), required=True)
    c.add_argument("--simgdb", required=True)
    c.add_argument("--gdb", default=os.environ.get("GDB", "arm-none-eabi-gdb"))
    c.add_argument("--save", help="directory to keep SimGDB's gdb output")
    args = ap.parse_args(argv[1:])
    return expected(args) if args.mode == "expected" else check(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
