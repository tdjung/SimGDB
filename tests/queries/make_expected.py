#!/usr/bin/env python3
"""Expected answers for SimGDB set-layer queries, from sources independent of
SimGDB.

Test-data generator, not part of SimGDB. Inputs:

- the canonical QEMU log of each test run (tests/firmware/traces/
  *.canonical.log.gz): the register state before every instruction, every data
  access with its value, and exception entries with the stacked frame. It is
  the run itself, before any Tarmac rendering.
- tarmac_to_cachegrind's per-PC execution counts and line attribution
  (`--pc-counts`) for the same run (fm dialect).
- the ELF's symbols (arm-none-eabi-nm) and instruction listing (objdump).

Everything is computed here by brute force with its own definitions (e.g. a
call returns when execution reaches the return address with the stack
pointer it had at the call), never with SimGDB.

Point convention (ARCHITECTURE §2.1): point i is the state before
instruction i; the effects of instruction i are visible from point i + 1.

Usage: make_expected.py --converter PATH/tarmac_to_cachegrind.py
Environment: NM, OBJDUMP, ADDR2LINE override tool paths.
"""

import argparse
import csv
import gzip
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent
FIRMWARE = ROOT.parent / "firmware"
sys.path.insert(0, str(FIRMWARE / "qemu"))
from make_traces import read_canonical, word, REGS  # noqa: E402


def tool(name, default):
    return os.environ.get(name, default)


def symbols(elf):
    out = subprocess.run([tool("NM", "arm-none-eabi-nm"), "-S", str(elf)], check=True,
                         stdout=subprocess.PIPE, universal_newlines=True).stdout
    funcs, objects = [], {}
    for line in out.splitlines():
        p = line.split()
        if len(p) == 4:
            addr, size, kind, name = int(p[0], 16), int(p[1], 16), p[2], p[3]
            if kind in "Tt":
                funcs.append((addr & ~1, size, name))
            elif kind in "BbDdRr":
                objects[name] = (addr, size)
    funcs.sort()
    return funcs, objects


def function_at(funcs, pc):
    best = None
    for addr, size, name in funcs:
        if addr <= pc:
            best = name
        else:
            break
    return best


def mnemonics(elf):
    out = subprocess.run([tool("OBJDUMP", "arm-none-eabi-objdump"), "-d", "--no-show-raw-insn", str(elf)],
                         check=True, stdout=subprocess.PIPE, universal_newlines=True).stdout
    table = {}
    for line in out.splitlines():
        m = re.match(r"^\s*([0-9a-f]+):\s+(\S+)", line)
        if m:
            table[int(m.group(1), 16)] = m.group(2)
    return table


def converter_counts(converter, core):
    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "fm.log"
        with gzip.open(FIRMWARE / "traces" / "state_{}.fm.log.gz".format(core)) as src:
            log.write_bytes(src.read())
        counts = Path(tmp) / "pc.csv"
        subprocess.run([sys.executable, str(converter), str(log), "--elf", str(FIRMWARE / "state_{}.elf".format(core)),
                        "-o", str(Path(tmp) / "out.cg"), "--pc-counts", str(counts),
                        "--addr2line", tool("ADDR2LINE", "arm-none-eabi-addr2line"),
                        "--objdump", tool("OBJDUMP", "arm-none-eabi-objdump")],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        with open(counts) as f:
            return [r for r in csv.DictReader(f)]


class Run:
    def __init__(self, core):
        self.core = core
        self.elf = FIRMWARE / "state_{}.elf".format(core)
        with gzip.open(FIRMWARE / "traces" / "state_{}.canonical.log.gz".format(core), "rt") as f:
            tmp = Path(tempfile.mkdtemp()) / "c.log"
            tmp.write_text(f.read())
        self.insns = read_canonical(tmp)
        self.funcs, self.objects = symbols(self.elf)
        self.mnem = mnemonics(self.elf)
        self.n = len(self.insns)
        self.exceptions = self.find_exceptions()

    # Exceptions from the exception level (IPSR) of every point: an entry
    # record (X) pushes the new level; a change of level that is not an entry
    # either returns to the enclosing level or, if it goes to another level,
    # is a tail-chained entry the log has no record for. The interrupted PC is
    # the return address in the stacked frame.
    def find_exceptions(self):
        out, stack = [], []
        ipsr = [insn.before["xpsr"] & 0x1FF for insn in self.insns]
        for j in range(1, self.n):
            prev = self.insns[j - 1]
            if prev.exc:
                e = {"entry": j, "number": ipsr[j], "return": None,
                     "interrupted_pc": word(prev.exc["frame"], 6), "inferred": False}
                out.append(e)
                stack.append(e)
                continue
            if ipsr[j] == ipsr[j - 1] or not stack:
                continue
            top = stack.pop()
            top["return"] = j
            outer = stack[-1]["number"] if stack else 0
            if ipsr[j] != outer:
                e = {"entry": j, "number": ipsr[j], "return": None,
                     "interrupted_pc": top["interrupted_pc"], "inferred": True}
                out.append(e)
                stack.append(e)
        return out

    def calls(self):
        """Executed bl/blx: callee, entry point, return point."""
        out = []
        for i, insn in enumerate(self.insns[:-1]):
            if self.mnem.get(insn.pc) not in ("bl", "blx"):
                continue
            ret = insn.pc + insn.size
            nxt = self.insns[i + 1]
            if insn.exc:
                taken = word(insn.exc["frame"], 5) == ret | 1
                target = word(insn.exc["frame"], 6)
            else:
                taken = nxt.before["lr"] == ret | 1
                target = nxt.pc
            if not taken:
                continue
            sp = insn.before["sp"]
            back = None
            for j in range(i + 1, self.n):
                if self.insns[j].pc == ret and self.insns[j].before["sp"] == sp:
                    back = j
                    break
            if back is None:
                # The callee switched stacks (use_process_stack): first return
                # to the address with the other stack selected.
                for j in range(i + 1, self.n):
                    if self.insns[j].pc == ret and self.insns[j].before["control"] != insn.before["control"]:
                        back = j
                        break
            out.append({"call": i, "entry": i + 1, "return": back, "callee": function_at(self.funcs, target),
                        "caller": function_at(self.funcs, insn.pc), "call_pc": insn.pc})
        return out

    def memory_states(self):
        """Known bytes after each instruction's accesses: yields (point, dict)."""
        mem = {}
        yield 0, mem
        for i, insn in enumerate(self.insns):
            for rw, addr, size, value in insn.mem:
                for k in range(size):
                    mem[addr + k] = (value >> (8 * k)) & 0xFF
            yield i + 1, mem


def line_key(file, line):
    return "{}:{}".format(Path(file).name, line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--converter", type=Path, required=True)
    args = ap.parse_args()
    for core in ("m4", "m35p"):
        run = Run(core)
        insns = run.insns
        counts = converter_counts(args.converter, core)
        pc_line = {int(r["pc"], 16): line_key(r["file"], r["line"]) for r in counts}
        pc_func = {int(r["pc"], 16): r["function"] for r in counts}

        # Executions per line (Ir) and line visits.
        line_ir = {}
        for r in counts:
            k = line_key(r["file"], r["line"])
            line_ir[k] = line_ir.get(k, 0) + int(r["Ir"])
        # Point an exception returned at -> earliest entry returning there
        # (tail-chained handlers return together).
        returns = {}
        for e in run.exceptions:
            if e["return"] is not None:
                returns[e["return"]] = min(returns.get(e["return"], e["entry"]), e["entry"])

        def previous(i):
            """Instruction before i in the same context: for the first
            instruction after an exception returned, the instruction that ran
            before the exception was taken."""
            return returns[i] - 1 if i in returns else i - 1

        visits = {}
        for line in ("state.c:44", "state.c:62", "state.c:111", "state.c:139", "state.c:80"):
            pcs = {pc for pc, k in pc_line.items() if k == line}
            pts = [i for i, insn in enumerate(insns)
                   if insn.pc in pcs and (previous(i) < 0 or insns[previous(i)].pc not in pcs)]
            visits[line] = pts

        # Calls by callee: count, inclusive instructions of returned calls.
        calls = run.calls()
        by_callee = {}
        for c in calls:
            s = by_callee.setdefault(c["callee"], {"calls": 0, "returned": 0, "inclusive": 0})
            s["calls"] += 1
            if c["return"] is not None:
                s["returned"] += 1
                s["inclusive"] += c["return"] - c["entry"]

        # Memory: last writes and writes to ranges, on globals.
        writes = []  # (index, addr, size, value)
        for i, insn in enumerate(insns):
            for rw, addr, size, value in insn.mem:
                if rw == "W":
                    writes.append((i, addr, size, value))
        last_write = []
        probes = [run.objects[n][0] for n in ("ticks", "svc_calls", "seed", "head", "result")]
        probes += [run.objects["ring"][0] + 12, run.objects["pool"][0] + 2 * 24 + 4, run.objects["bytes"][0] + 7,
                   run.objects["wide"][0] + 5]
        for addr in probes:
            for point in range(0, run.n + 1, run.n // 9):
                hit = None
                for (i, a, size, value) in writes:
                    if i < point and a <= addr < a + size:
                        hit = (i, (value >> (8 * (addr - a))) & 0xFF)
                last_write.append({"addr": addr, "point": point,
                                   "written_by": hit[0] if hit else None, "byte": hit[1] if hit else None})
        writes_to = []
        for name, lo_frac, hi_frac in (("ring", 0, 1), ("ticks", 0.25, 0.75), ("pool", 0.3, 0.5), ("bytes", 0, 1)):
            addr, size = run.objects[name]
            lo, hi = int(run.n * lo_frac), int(run.n * hi_frac)
            items = [{"index": i, "addr": a, "size": s, "value": v} for (i, a, s, v) in writes
                     if lo <= i < hi and a < addr + size and addr < a + s]
            writes_to.append({"name": name, "addr": addr, "len": size, "from": lo, "to": hi, "writes": items})

        # Memory and registers at chosen points, for find_first and diff.
        regions = [run.objects[n] for n in ("seed", "primes", "ticks", "svc_calls", "ring", "pool", "head",
                                            "bytes", "wide", "result")]
        snapshots = {}
        wanted = sorted({run.n // 5, run.n // 3, run.n // 2, 2 * run.n // 3, run.n - 1})
        for point, mem in run.memory_states():
            if point in wanted:
                snapshots[point] = dict(mem)
        diffs = []
        for a, b in zip(wanted, wanted[1:]):
            regs = sorted(r for r in REGS if insns[a].before[r] != insns[b].before[r])
            changed = sorted(addr for lo, size in regions for addr in range(lo, lo + size)
                             if addr in snapshots[a] and addr in snapshots[b]
                             and snapshots[a][addr] != snapshots[b][addr])
            unknown = sorted(addr for lo, size in regions for addr in range(lo, lo + size)
                             if addr not in snapshots[a] or addr not in snapshots[b])
            diffs.append({"a": a, "b": b, "registers": regs, "bytes": changed, "oracle_unknown": unknown})

        # find_first: brute force over every point.
        def var(mem, name, off=0, size=4, signed=False):
            addr = run.objects[name][0] + off
            if any(addr + k not in mem for k in range(size)):
                return None
            v = sum(mem[addr + k] << (8 * k) for k in range(size))
            if signed and v >= 1 << (8 * size - 1):
                v -= 1 << (8 * size)
            return v

        sym = {n: a for a, _, n in run.funcs}
        conditions = [
            ('pc == sym("SysTick_Handler")', lambda i, r, m: insns[i].pc == sym["SysTick_Handler"]),
            ('pc == sym("SVC_Handler") && reg("r0") != 0', lambda i, r, m: insns[i].pc == sym["SVC_Handler"] and r["r0"] != 0),
            ('var("ticks") == 5', lambda i, r, m: none_eq(var(m, "ticks"), 5)),
            ('var("ticks") >= 3 && func == "depth"', lambda i, r, m: and3(cmp(var(m, "ticks"), 3, "ge"), pc_func.get(insns[i].pc) == "depth")),
            ('line == "state.c:111" && var("ticks") > 10', lambda i, r, m: and3(pc_line.get(insns[i].pc) == "state.c:111", cmp(var(m, "ticks"), 10, "gt"))),
            ('reg("sp") < 0x2000ff80', lambda i, r, m: r["sp"] < 0x2000FF80),
            ('mem32(0x{:x}) != 0'.format(run.objects["ring"][0]), lambda i, r, m: not3(none_eq(var(m, "ring"), 0))),
            ('var("pool[3].len") == 700', lambda i, r, m: none_eq(var(m, "pool", 3 * 24 + 2, 2), 700)),
            ('!(reg("r0") == 0) && pc == sym("mix")', lambda i, r, m: r["r0"] != 0 and insns[i].pc == sym["mix"]),
            ('var("head") == 0 && func == "build_list"', lambda i, r, m: and3(none_eq(var(m, "head"), 0), pc_func.get(insns[i].pc) == "build_list")),
            ('reg("lr") == 0xfffffff9 || reg("lr") == 0xfffffffd', lambda i, r, m: r["lr"] in (0xFFFFFFF9, 0xFFFFFFFD)),
            ('var("svc_calls") == 3', lambda i, r, m: none_eq(var(m, "svc_calls"), 3)),
        ]
        find_first = []
        mems = [dict(mem) for _, mem in run.memory_states()]
        for text, f in conditions:
            values = [f(i, insns[i].before, mems[i]) for i in range(run.n)]
            for start, end, direction in ((0, run.n - 1, "forward"), (run.n // 2, run.n - 1, "forward"),
                                          (0, run.n - 1, "backward"), (0, run.n // 2, "backward")):
                order = range(start, end + 1) if direction == "forward" else range(end, start - 1, -1)
                found = next((i for i in order if values[i] is True), None)
                find_first.append({"condition": text, "from": start, "to": end, "direction": direction,
                                   "found": found})

        expected = {
            "instructions": run.n,
            "line_ir": line_ir,
            "line_visits": visits,
            "exceptions": run.exceptions,
            "calls": by_callee,
            "call_list": calls[:200],
            "last_write": last_write,
            "writes_to": writes_to,
            "diff": diffs,
            "diff_regions": regions,
            "find_first": find_first,
        }
        path = ROOT / "expected" / "state_{}.json".format(core)
        path.parent.mkdir(exist_ok=True)
        path.write_text(json.dumps(expected, indent=1, sort_keys=True) + "\n")
        print("{}: {} lines, {} exceptions, {} calls, {} find_first cases".format(
            path.name, len(line_ir), len(run.exceptions), len(calls), len(find_first)))


def none_eq(v, x):
    return None if v is None else v == x


def cmp(v, x, op):
    if v is None:
        return None
    return v >= x if op == "ge" else v > x


def not3(v):
    return None if v is None else not v


def and3(a, b):
    if a is False or b is False:
        return False
    if a is None or b is None:
        return None
    return True


if __name__ == "__main__":
    main()
