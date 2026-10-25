#!/usr/bin/env python3
"""Generate SimGDB test traces and independent ground truth from QEMU.

Test-data generator, not part of SimGDB. Steps:

1. Run the firmware under QEMU with trace_plugin.so: a canonical log with the
   register state before every instruction, data accesses with values, and
   exception entries with the stacked frame.
2. Render that log as Tarmac in three dialects (see DIALECTS). Register
   records come from differences between consecutive register states; an
   exception entry is split into the instruction's own effects and the entry's
   effects using the stacked frame.
3. Record ground truth with a *separate* QEMU run through its gdbstub and gdb:
   at chosen points (the k-th execution of a PC) registers, RAM and the
   backtrace as gdb sees them. SimGDB output is never used.

The traced run is recorded with QEMU record/replay and the truth runs replay
that recording, so interrupts arrive at the same instruction however gdb
stops and steps (a plain second run drifts: gdb's step over a breakpoint
holds the timer for one instruction). Every truth point re-checks its
registers against the canonical log and the script fails if they differ.

Usage: make_traces.py ELF {m4|m35p} OUT_DIR [--points N]
Environment: QEMU, PLUGIN, GDB, OBJDUMP override the tool paths.
"""

import argparse
import gzip
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time

MACHINES = {"m4": ("mps2-an386", "cortex-m4"), "m35p": ("mps2-an505", "cortex-m33")}
ICOUNT = "shift=5,align=off,sleep=off"
REGS = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12",
        "sp", "lr", "xpsr", "msp", "psp", "primask", "basepri", "faultmask", "control"]
CONDITIONS = ("eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le")
ALIASES = {"hs": "cs", "lo": "cc"}


def tool(name, default):
    return os.environ.get(name, default)


# ---------------------------------------------------------------- canonical log

class Insn:
    __slots__ = ("pc", "op", "size", "before", "mem", "exc")

    def __init__(self, pc, op, size, before):
        self.pc, self.op, self.size, self.before = pc, op, size, before
        self.mem = []   # (rw, addr, size, value)
        self.exc = None  # dict(kind, frm, to, post, frame_addr, frame)


def run_plugin(elf, core, log, rrfile):
    machine, cpu = MACHINES[core]
    subprocess.run([tool("QEMU", "qemu-system-arm"), "-M", machine, "-cpu", cpu, "-nographic",
                    "-monitor", "none", "-serial", "none",
                    "-semihosting-config", "enable=on,target=native", "-kernel", str(elf),
                    "-accel", "tcg,one-insn-per-tb=on",
                    "-icount", "{},rr=record,rrfile={}".format(ICOUNT, rrfile),
                    "-plugin", "{},out={}".format(tool("PLUGIN", "trace_plugin.so"), log)],
                   check=True, timeout=300)


def read_canonical(log):
    insns, state, pending = [], {}, None
    with open(log) as f:
        for line in f:
            tag, rest = line[0], line[2:].rstrip("\n")
            if tag == "S":
                name, value = rest.split()
                state[name] = int(value, 16)
            elif tag == "I":
                parts = rest.split(" ", 3)
                pending = Insn(int(parts[0], 16), parts[1], int(parts[2]), dict(state))
                insns.append(pending)
            elif tag == "M":
                rw, addr, size, value = rest.split()
                pending.mem.append((rw, int(addr, 16), int(size), int(value, 16)))
            elif tag == "X":
                kind, frm, to = rest.split()
                pending.exc = {"kind": kind, "frm": int(frm, 16), "to": int(to, 16), "post": {}}
            elif tag == "P":
                name, value = rest.split()
                pending.exc["post"][name] = int(value, 16)
            elif tag == "F":
                addr, data = rest.split()
                pending.exc["frame_addr"] = int(addr, 16)
                pending.exc["frame"] = bytes.fromhex(data)
    return insns


def listing(elf):
    """pc -> (mnemonic, operands) from objdump."""
    out = subprocess.run([tool("OBJDUMP", "arm-none-eabi-objdump"), "-d", "--no-show-raw-insn", str(elf)],
                         check=True, stdout=subprocess.PIPE, universal_newlines=True).stdout
    table = {}
    for line in out.splitlines():
        m = re.match(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$", line)
        if m and not m.group(2).startswith("."):
            table[int(m.group(1), 16)] = (m.group(2), m.group(3).split("\t@")[0].split(";")[0].strip())
    return table


def word(frame, k):
    return int.from_bytes(frame[4 * k:4 * k + 4], "little")


def pre_entry(insn, after):
    """Register state between the instruction and the exception entry.

    `after` is the state after entry. The frame holds r0-r3, r12, lr, the
    return address and xPSR as they were; xPSR bit 9 says a 4-byte alignment
    pad was inserted above the frame.
    """
    exc = insn.exc
    frame = exc["frame"]
    pre = dict(after)
    for k, name in enumerate(("r0", "r1", "r2", "r3", "r12", "lr")):
        pre[name] = word(frame, k)
    stacked_psr = word(frame, 7)
    pre["xpsr"] = stacked_psr & ~(1 << 9)
    on_psp = after["lr"] & 4
    bank = "psp" if on_psp else "msp"
    pre[bank] = exc["frame_addr"] + 32 + (4 if stacked_psr & (1 << 9) else 0)
    pre["sp"] = pre[bank]
    pre["control"] = insn.before["control"]
    return pre


# ---------------------------------------------------------------- condition failure

def condition_passes(cond, xpsr):
    n, z, c, v = (xpsr >> 31) & 1, (xpsr >> 30) & 1, (xpsr >> 29) & 1, (xpsr >> 28) & 1
    return bool({
        "eq": z, "ne": not z, "cs": c, "cc": not c, "mi": n, "pl": not n,
        "vs": v, "vc": not v, "hi": c and not z, "ls": not c or z,
        "ge": n == v, "lt": n != v, "gt": not z and n == v, "le": z or n != v,
    }[cond])


def it_condition(xpsr):
    it = ((xpsr >> 25) & 0x3) | (((xpsr >> 10) & 0x3F) << 2)
    if it & 0xF == 0:
        return None
    code = (it >> 4) & 0xF
    return CONDITIONS[code] if code < 14 else None


def branch_condition(mnemonic):
    base = mnemonic.split(".")[0].lower()
    if not base.startswith("b") or base in ("bl", "blx", "bx", "bkpt", "bic", "bics", "bfi", "bfc"):
        return None
    cond = base[1:]
    cond = ALIASES.get(cond, cond)
    return cond if cond in CONDITIONS else None


def failed(insn, table):
    mnemonic = table.get(insn.pc, ("?", ""))[0]
    cond = it_condition(insn.before["xpsr"])
    if cond is None:
        cond = branch_condition(mnemonic)
    return cond is not None and not condition_passes(cond, insn.before["xpsr"])


# ---------------------------------------------------------------- dialects

def diff(old, new):
    return [(name, new[name]) for name in REGS if name in new and old.get(name) != new[name]]


class Dialect:
    """Common rendering; subclasses decide the textual form of each record."""

    stacking = False   # write the exception frame as memory stores
    header = None

    def __init__(self, out):
        self.out = out

    def emit(self, line):
        self.out.append(line)

    def stamp(self, i):
        raise NotImplementedError

    def render(self, insns, table):
        if self.header:
            self.emit(self.header)
        first = insns[0].before
        # Reset-defined state before the first instruction: SP, xPSR, CONTROL.
        self.registers(self.stamp(0), [(n, first[n]) for n in ("msp", "sp", "xpsr", "control")], first)
        for i, insn in enumerate(insns):
            t = self.stamp(i)
            mnem, ops = table.get(insn.pc, ("UNKNOWN", ""))
            self.instruction(t, i, insn, (mnem.upper() + " " + ops).strip(), failed(insn, table))
            for rw, addr, size, value in insn.mem:
                self.memory(t, rw, addr, size, value)
            if i + 1 >= len(insns):
                break
            after = insns[i + 1].before
            if insn.exc:
                pre = pre_entry(insn, after)
                self.registers(t, diff(insn.before, pre), pre)
                number = after["xpsr"] & 0x1FF
                self.exception(t, number, insn.exc)
                if self.stacking:
                    for k in range(8):
                        self.memory(t, "W", insn.exc["frame_addr"] + 4 * k, 4, word(insn.exc["frame"], k))
                self.registers(t, diff(pre, after), after)
            else:
                self.registers(t, diff(insn.before, after), after)

    def registers(self, t, changes, state):
        for name, value in changes:
            self.register(t, name, value, state)


class FastModels(Dialect):
    """IT/IS lines with a sequence number, picosecond times, MRn/MWn records.

    The clock period halves at instruction 2000 (a frequency change), so time
    is not proportional to the instruction count.
    """

    names = {"sp": "r13", "lr": "r14", "xpsr": "XPSR", "msp": "MSP", "psp": "PSP",
             "control": "CONTROL", "primask": "PRIMASK", "basepri": "BASEPRI", "faultmask": "FAULTMASK"}

    def stamp(self, i):
        return 1_000_000 + (i * 10_000 if i < 2000 else 2000 * 10_000 + (i - 2000) * 5_000)

    def instruction(self, t, i, insn, text, fail):
        self.emit("{} ps {} ({}) {:08x} {} T thread : {}".format(
            t, "IS" if fail else "IT", i, insn.pc, insn.op, text))

    def register(self, t, name, value, state):
        self.emit("{} ps R {} {:08x}".format(t, self.names.get(name, name), value))

    def memory(self, t, rw, addr, size, value):
        self.emit("{} ps M{}{} {:08x} {:0{}x}".format(t, rw, size, addr, value, size * 2))

    def exception(self, t, number, exc):
        self.emit("{} ps E {:08x} {:08x} CoreEvent_{}".format(
            t, exc["to"], number, "IRQ" if exc["kind"] == "irq" else "EXCEPTION"))


class CortexM4Rtl(Dialect):
    """Cortex-M4 RTL style: IT (pc:seq), cycle stamps, MNW4___D records in
    memory byte order, r13 written with its bank, an IF repeat now and then,
    and the exception frame written as memory stores."""

    stacking = True

    def __init__(self, out):
        super().__init__(out)
        self.cycles = []

    def stamp(self, i):
        while len(self.cycles) <= i:
            prev = self.cycles[-1] if self.cycles else 3000
            self.cycles.append(prev + 1 + (len(self.cycles) * 7919) % 3)
        return self.cycles[i]

    def instruction(self, t, i, insn, text, fail):
        state = "T16" if insn.size == 2 else "T32"
        line = "{:>10} cyc {} ({:08x}:{:08x}) {:08x} {:>8} {} {}".format(
            t, "IS" if fail else "IT", insn.pc, i, insn.pc, insn.op, state, text)
        self.emit(line)
        if i % 13 == 5 and not fail:
            self.emit(line.replace(" IT (", " IF (", 1))

    def register(self, t, name, value, state):
        if name in ("msp", "psp"):
            self.emit("{:>10} cyc R r13 {:08x} ({})".format(t, value, name.upper()))
        elif name == "sp":
            return  # the banked write above carries it
        elif name == "xpsr":
            self.emit("{:>10} cyc R psr {:08x}".format(t, value))
        else:
            self.emit("{:>10} cyc R {} {:08x}".format(t, name, value))

    def memory(self, t, rw, addr, size, value):
        data = value.to_bytes(size, "little").hex()  # memory order
        self.emit("{:>10} cyc MN{}{}___D {:08x} {}".format(t, rw, size, addr, data))

    def exception(self, t, number, exc):
        self.emit("{:>10} cyc E {:08x} {:08x} CoreEvent_IRQ".format(t, exc["to"], number))


class EmbeddedTrace(Dialect):
    """ES lines with tic stamps, CCFAIL markers, LD/ST 16-byte diagrams,
    upper-case register names and ES EXC records."""

    header = "Tarmac Text Rev 3t"
    names = {"sp": "SP", "lr": "LR", "xpsr": "XPSR", "msp": "SP_main", "psp": "SP_process",
             "control": "CONTROL", "primask": "PRIMASK", "basepri": "BASEPRI", "faultmask": "FAULTMASK"}

    def stamp(self, i):
        return 100 + i * 2

    def instruction(self, t, i, insn, text, fail):
        self.emit("{:>8} tic ES ({:08x}:{}) T thrd: {}{}".format(
            t, insn.pc, insn.op, "CCFAIL " if fail else "", text))

    def register(self, t, name, value, state):
        self.emit("                   R {} {:08x}".format(self.names.get(name, name.upper()), value))

    def memory(self, t, rw, addr, size, value):
        base = addr & ~15
        cells = ["..."] * 16
        cells = [".."] * 16
        for k, byte in enumerate(value.to_bytes(size, "little")):
            off = addr + k - base
            if off >= 16:  # access crosses a 16-byte line: second diagram
                self.memory(t, rw, base + 16, size - k, value >> (8 * k))
                break
            cells[15 - off] = "{:02x}".format(byte)
        words = " ".join("".join(cells[w * 4:w * 4 + 4]) for w in range(4))
        self.emit("                   {} {:08x}  {}".format("ST" if rw == "W" else "LD", base, words))

    def exception(self, t, number, exc):
        name = {11: "SVCall", 15: "SysTick"}.get(number, "IRQ{}".format(number - 16))
        self.emit("{:>8} tic ES EXC [0x{:02x}] {}".format(t, number, name))


DIALECTS = {"fm": FastModels, "m4rtl": CortexM4Rtl, "es": EmbeddedTrace}


# ---------------------------------------------------------------- ground truth

GDB_SCRIPT = r'''
import gdb, json
gdb.execute("set pagination off")
gdb.execute("set confirm off")
gdb.execute("target remote :{port}")
gdb.execute("break *0x{pc:x}")
if {k} > 1:
    gdb.execute("ignore 1 {skip}")
gdb.execute("continue")
frame = gdb.selected_frame()
regs = {{}}
for name in {regs!r}:
    try:
        regs[name] = int(frame.read_register(name)) & 0xffffffff
    except gdb.error:
        pass
inferior = gdb.selected_inferior()
mem = {{}}
for lo, hi in {regions!r}:
    mem["%08x" % lo] = bytes(inferior.read_memory(lo, hi - lo)).hex()
bt = []
f = gdb.newest_frame()
while f is not None and len(bt) < 32:
    sal = f.find_sal()
    bt.append({{"pc": f.pc() & 0xffffffff, "function": f.name(),
               "file": sal.symtab.filename if sal.symtab else None, "line": sal.line}})
    try:
        f = f.older()
    except gdb.error:
        break
with open({out!r}, "w") as fh:
    json.dump({{"registers": regs, "memory": mem, "backtrace": bt}}, fh)
gdb.execute("kill")
'''


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def truth_point(elf, core, pc, k, regions, tmp, rrfile):
    machine, cpu = MACHINES[core]
    port = free_port()
    qemu = subprocess.Popen([tool("QEMU", "qemu-system-arm"), "-M", machine, "-cpu", cpu, "-nographic",
                             "-monitor", "none", "-serial", "none",
                             "-semihosting-config", "enable=on,target=native", "-kernel", str(elf),
                             "-accel", "tcg,one-insn-per-tb=on",
                             "-icount", "{},rr=replay,rrfile={}".format(ICOUNT, rrfile),
                             "-gdb", "tcp::{}".format(port), "-S"],
                            stderr=subprocess.DEVNULL)
    try:
        out = Path(tmp) / "truth.json"
        script = Path(tmp) / "truth.py"
        script.write_text(GDB_SCRIPT.format(port=port, pc=pc, k=k, skip=k - 1, regs=REGS,
                                            regions=regions, out=str(out)))
        for _ in range(50):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
                break
            except OSError:
                time.sleep(0.1)
        subprocess.run([tool("GDB", "gdb-multiarch"), "-q", "-nx", "-batch", str(elf), "-x", str(script)],
                       check=True, timeout=120, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        return json.loads(out.read_text())
    finally:
        qemu.kill()
        qemu.wait()


def symbol(elf, name):
    out = subprocess.run([tool("NM", "arm-none-eabi-nm"), "-S", str(elf)],
                         check=True, stdout=subprocess.PIPE, universal_newlines=True).stdout
    for line in out.splitlines():
        p = line.split()
        if len(p) == 4 and p[3] == name:
            return int(p[0], 16), int(p[1], 16)
    raise KeyError(name)


def ram_regions(elf, insns):
    """Memory worth comparing: .data/.bss, every stack byte the run touched,
    and two flash ranges (vector table, a .rodata table) so values taken from
    the ELF load image are checked too."""
    out = subprocess.run([tool("OBJDUMP", "arm-none-eabi-objdump"), "-h", str(elf)],
                         check=True, stdout=subprocess.PIPE, universal_newlines=True).stdout
    regions = []
    for line in out.splitlines():
        p = line.split()
        if len(p) >= 4 and p[1] in (".data", ".bss"):
            vma, size = int(p[3], 16), int(p[2], 16)
            regions.append((vma, vma + size))
    vectors, vsize = symbol(elf, "vectors")
    table, tsize = symbol(elf, "table")
    regions = [(vectors, vectors + vsize), (table, table + tsize)] + regions
    low_msp = min(i.before["msp"] for i in insns)
    top_msp = insns[0].before["msp"]
    regions.append(((low_msp - 64) & ~15, top_msp))
    return regions


def arrivals(insns, index):
    """gdb's hit count for a breakpoint at insns[index].pc when it stops there.

    gdb counts every arrival at the PC, including arrivals where an exception
    preempts the instruction before it executes (the stacked return address
    is that PC); the trace only shows executions."""
    pc = insns[index].pc
    count = 0
    for i in range(index + 1):
        if insns[i].pc == pc:
            count += 1
        if i < index and insns[i].exc and word(insns[i].exc["frame"], 6) == pc \
                and insns[i].pc != pc:
            count += 1
    return count


def choose_points(insns, count):
    """Evenly spaced points plus the instruction before and after each kind of
    event the engine must get right (exception entry, first handler insn,
    return to the interrupted code), as (index, pc, k)."""
    wanted = set(range(1, len(insns) - 1, max(1, (len(insns) - 2) // count)))
    for i, insn in enumerate(insns[:-1]):
        if insn.exc and len([j for j in wanted if insns[j].exc]) < 8:
            wanted.update((i, i + 1, i + 2))
    for i in range(1, len(insns)):
        # Return from an exception: IPSR goes back to 0.
        if insns[i - 1].before["xpsr"] & 0x1FF and not insns[i].before["xpsr"] & 0x1FF:
            if len([j for j in wanted if j >= 2 and insns[j - 1].before["xpsr"] & 0x1FF
                    and not insns[j].before["xpsr"] & 0x1FF]) < 6:
                wanted.update((i - 1, i, i + 1))
    wanted.add(len(insns) - 1)
    seen, points = {}, []
    for i, insn in enumerate(insns):
        seen[insn.pc] = seen.get(insn.pc, 0) + 1
        if i in wanted:
            points.append((i, insn.pc, seen[insn.pc]))
    return points


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("elf", type=Path)
    ap.add_argument("core", choices=MACHINES)
    ap.add_argument("out", type=Path)
    ap.add_argument("--points", type=int, default=40)
    args = ap.parse_args(argv[1:])
    args.out.mkdir(parents=True, exist_ok=True)
    stem = args.out / args.elf.stem
    table = listing(args.elf)
    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "canonical.log"
        rrfile = Path(tmp) / "run.rr"
        run_plugin(args.elf, args.core, log, rrfile)
        insns = read_canonical(log)
        for name, cls in DIALECTS.items():
            lines = []
            cls(lines).render(insns, table)
            with gzip.GzipFile("{}.{}.log.gz".format(stem, name), "wb", mtime=0) as f:
                f.write(("\n".join(lines) + "\n").encode())
        regions = ram_regions(args.elf, insns)
        points = []
        for index, pc, k in choose_points(insns, args.points):
            hit = arrivals(insns, index)
            t = truth_point(args.elf, args.core, pc, hit, regions, tmp, rrfile)
            expected = insns[index].before
            bad = [n for n in REGS if n in t["registers"] and t["registers"][n] != expected[n]]
            if bad:
                sys.exit("runs diverged at index {} (pc {:#x}, gdb hit {}): {}".format(index, pc, hit, bad))
            points.append({"index": index, "pc": pc, "hit": k, "gdb_hit": hit, **t})
            print("truth: index {:5d} pc {:#010x} hit {:3d}".format(index, pc, k), file=sys.stderr)
    truth = {"elf": args.elf.name, "core": args.core, "instructions": len(insns),
             "exceptions": sum(1 for i in insns if i.exc), "regions": regions, "points": points}
    with gzip.GzipFile("{}.truth.json.gz".format(stem), "wb", mtime=0) as f:
        f.write(json.dumps(truth, indent=1, sort_keys=True).encode())
    print("{}: {} instructions, {} exceptions, {} truth points".format(
        stem.name, len(insns), truth["exceptions"], len(points)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
