#!/usr/bin/env python3
"""An IDE-style debug session against `simgdb serve`, over gdb's machine
interface (MI), the way VS Code debug adapters (cortex-debug, cppdbg) drive
gdb: extended-remote connect, `monitor halt`, function and line
breakpoints, continue/next/step/finish, reverse next, stack frames,
variables (expressions and variable objects with children), registers,
memory, and SimGDB monitor commands.

Values are cross-checked with `simgdb query` on the same index (the RSP path
against the engine); state correctness itself is covered by gdb_tests.py and
the engine tests.

Usage: ide_session.py --simgdb target/release/simgdb [--gdb G] [--core m4] [--dialect m4rtl]
"""

import argparse
import json
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
FIRMWARE = HERE.parent / "firmware"


class Mi:
    def __init__(self, gdb, elf):
        self.p = subprocess.Popen([gdb, "-q", "-nx", "--interpreter=mi2", str(elf)], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
        self.token = 100
        self.console = []
        self.read_until(lambda l: l.startswith("(gdb)"))

    def read_until(self, done):
        lines = []
        for line in self.p.stdout:
            line = line.rstrip("\n")
            lines.append(line)
            if line.startswith("~") or line.startswith("@"):
                self.console.append(json.loads(line[1:]))
            if done(line):
                return lines
        raise SystemExit("gdb exited:\n" + "\n".join(lines[-20:]))

    def cmd(self, text, stop=False, ok=("done", "connected", "running")):
        """Run an MI command; with `stop`, also wait for *stopped. Returns
        (result record, stopped record or None)."""
        self.token += 1
        t = str(self.token)
        self.console = []
        self.p.stdin.write("{}{}\n".format(t, text))
        self.p.stdin.flush()
        lines = self.read_until(lambda l: l.startswith(t + "^"))
        result = lines[-1][len(t) + 1:]
        cls = result.split(",", 1)[0]
        if cls not in ok:
            raise AssertionError("{} -> {}".format(text, result))
        stopped = None
        if stop:
            stopped = self.read_until(lambda l: l.startswith("*stopped"))[-1]
        return result, stopped

    def console_text(self):
        return "".join(self.console)

    def close(self):
        try:
            self.cmd("-gdb-exit", ok=("exit",))
        except Exception:
            pass
        self.p.kill()


def field(record, name):
    m = re.search(r'\b{}="((?:[^"\\]|\\.)*)"'.format(name), record or "")
    return m.group(1) if m else None


def query(simgdb, idx, *args):
    out = subprocess.run([simgdb, "query", idx] + list(args), check=True, stdout=subprocess.PIPE,
                         universal_newlines=True).stdout
    return json.loads(out)


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--simgdb", required=True)
    ap.add_argument("--gdb", default="gdb-multiarch")
    ap.add_argument("--core", default="m4")
    ap.add_argument("--dialect", default="m4rtl")
    a = ap.parse_args(argv[1:])
    elf = FIRMWARE / "state_{}.elf".format(a.core)
    trace = FIRMWARE / "traces" / "state_{}.{}.log.gz".format(a.core, a.dialect)
    tmp = tempfile.mkdtemp()
    idx = str(Path(tmp) / "idx")
    subprocess.run([a.simgdb, "index", str(trace), "--elf", str(elf), "-o", idx], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    stacking = json.loads(subprocess.run([a.simgdb, "info", idx], check=True, stdout=subprocess.PIPE,
                                         universal_newlines=True).stdout)["capabilities"]["exception_stacking_writes"]
    server = subprocess.Popen([a.simgdb, "serve", idx, "--listen", "127.0.0.1:{}".format(port), "--once"],
                              stderr=subprocess.PIPE, universal_newlines=True)
    assert "waiting for gdb" in server.stderr.readline()
    mi = Mi(a.gdb, elf)
    checks = 0
    try:
        mi.cmd("-gdb-set mi-async on")
        mi.cmd("-enable-pretty-printing")
        mi.cmd("-interpreter-exec console \"set mem inaccessible-by-default off\"")
        mi.cmd("-target-select extended-remote 127.0.0.1:{}".format(port))
        mi.cmd("-interpreter-exec console \"monitor halt\"")
        # Stopped at the first point.
        r, _ = mi.cmd("-stack-info-frame")
        assert field(r, "func") == "Reset_Handler", r
        # Function breakpoint, continue.
        r, _ = mi.cmd("-break-insert work")
        assert field(r, "func") == "work", r
        _, st = mi.cmd("-exec-continue", stop=True)
        assert field(st, "reason") == "breakpoint-hit" and field(st, "func") == "work", st
        checks += 1
        r, _ = mi.cmd("-stack-list-frames")
        funcs = re.findall(r'func="(\w+)"', r)
        assert funcs[:2] == ["work", "main"], funcs
        # Where SimGDB says we are (index for cross-checks).
        mi.cmd("-interpreter-exec console \"monitor time\"")
        index = int(re.search(r"index (\d+)", mi.console_text()).group(1))
        loc = query(a.simgdb, idx, "locate", "--at", str(index))
        assert loc["source"]["function"] == "work", loc
        checks += 1
        # Expressions and variable objects.
        r, _ = mi.cmd("-data-evaluate-expression ticks")
        want = query(a.simgdb, idx, "var", "ticks", "--at", str(index))["decoded"]
        assert field(r, "value") == str(want), (r, want)
        r, _ = mi.cmd("-var-create - * pool[1]")
        name = field(r, "name")
        assert field(r, "numchild") == "5", r
        r, _ = mi.cmd("-var-list-children --all-values {}".format(name))
        kids = dict(re.findall(r'exp="(\w+)",numchild="\d+",value="([^"]*)"', r))
        want = query(a.simgdb, idx, "var", "pool[1].len", "--at", str(index))["decoded"]
        assert kids.get("len") == str(want), (kids, want)
        checks += 2
        mi.cmd("-stack-list-variables --all-values")
        # Registers and memory.
        r, _ = mi.cmd("-data-list-register-values x 15")
        pc = int(re.search(r'number="15",value="(0x[0-9a-f]+)"', r).group(1), 16)
        assert pc == int(query(a.simgdb, idx, "registers", "--at", str(index))["registers"][15]["value"]), r
        r, _ = mi.cmd("-data-read-memory-bytes &ticks 4")
        want = query(a.simgdb, idx, "var", "ticks", "--at", str(index))["hex"]
        assert field(r, "contents") == want, (r, want)
        checks += 2
        # Stepping both ways.
        _, st = mi.cmd("-exec-next", stop=True)
        assert field(st, "reason") == "end-stepping-range", st
        line_after_next = field(st, "line")
        _, st = mi.cmd("-exec-step", stop=True)
        assert field(st, "func") == "build_list", st
        _, st = mi.cmd("-exec-finish", stop=True)
        assert field(st, "reason") == "function-finished" and field(st, "func") == "work", st
        _, st = mi.cmd("-exec-next --reverse", stop=True)
        assert field(st, "func") == "work" and field(st, "line") == line_after_next, st
        checks += 4
        # Line breakpoint in an interrupt handler; its backtrace crosses the exception.
        mi.cmd("-break-delete")
        r, _ = mi.cmd("-break-insert state.c:111")
        _, st = mi.cmd("-exec-continue", stop=True)
        assert field(st, "func") == "SysTick_Handler", st
        # Before the firmware first writes PSP, gdb's M-profile unwinder cannot
        # read it and gives up on the stack (docs/LIMITATIONS.md).
        r, _ = mi.cmd("-stack-list-frames", ok=("done", "error"))
        assert r.startswith("done") or "is not available" in r, r
        # After it (a SysTick once the process stack is in use) the stack
        # crosses the exception frame.
        mi.cmd('-interpreter-exec console "monitor find pc == sym(\\"SysTick_Handler\\") && reg(\\"psp\\") != 0"')
        later = int(re.search(r"found at index (\d+)", mi.console_text()).group(1))
        mi.cmd('-interpreter-exec console "monitor goto {}"'.format(later))
        mi.cmd('-interpreter-exec console "maintenance flush register-cache"')
        r, _ = mi.cmd("-stack-list-frames")
        assert "<signal handler called>" in r, r
        if stacking:
            # The stacked frame is in the trace: the interrupted code is unwound.
            assert 'func="main"' in r, r
        checks += 2
        # SimGDB monitor commands through the console.
        mi.cmd("-interpreter-exec console \"monitor where\"")
        assert "#0 SysTick_Handler" in mi.console_text(), mi.console_text()
        mi.cmd("-interpreter-exec console \"monitor hits mix 1\"")
        calls = query(a.simgdb, idx, "executions", "mix", "--limit", "0")["entries"]["total"]
        assert "{} entries".format(calls) in mi.console_text(), mi.console_text()
        checks += 2
        # Writes are refused (the target is a recording).
        r, _ = mi.cmd("-data-evaluate-expression \"ticks = 1\"", ok=("error",))
        checks += 1
        # Restart as IDEs do: monitor reset, then continue to the breakpoint again.
        mi.cmd("-interpreter-exec console \"monitor reset halt\"")
        mi.cmd("-interpreter-exec console \"maintenance flush register-cache\"")
        _, st = mi.cmd("-exec-continue", stop=True)
        assert field(st, "func") == "SysTick_Handler", st
        checks += 1
    finally:
        mi.close()
        server.wait(timeout=10)
    print("{} {} ide session: ok ({} checks)".format(a.core, a.dialect, checks))


if __name__ == "__main__":
    main(sys.argv)
