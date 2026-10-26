#!/usr/bin/env python3
"""Stack queries at every point, the way IDE front ends issue them.

VS Code's cppdbg asks `-stack-list-frames` at every stop and ends the session
when it fails. GDB's M-profile unwinder fails that query whenever MSP or PSP
is unavailable, which is the case before the first PSP write of a trace.

  stack_scan.py --simgdb target/release/simgdb [--gdb G] [--core m4] [--dialect m4rtl] [--step 1]

Runs two servers on the same index and prints how many points fail:
  default            expected to fail only where PSP is unknown
  --no-psp-unwind    expected to fail nowhere
Exit status 1 if --no-psp-unwind fails anywhere, or the default fails at a
point where PSP is known.
"""

import argparse
from pathlib import Path
import re
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
FIRMWARE = HERE.parent / "firmware"
sys.path.insert(0, str(HERE))
from gdb_tests import free_port  # noqa: E402


def scan(args, idx, elf, extra, points):
    port = free_port()
    s = subprocess.Popen([args.simgdb, "serve", str(idx), "--listen", "127.0.0.1:{}".format(port), "--once"] + extra,
                         stderr=subprocess.PIPE, universal_newlines=True)
    try:
        if "waiting for gdb" not in s.stderr.readline():
            raise RuntimeError("simgdb serve did not start")
        cmds = ["-file-exec-and-symbols {}".format(elf), "-target-select remote 127.0.0.1:{}".format(port)]
        for i in points:
            cmds += ['-interpreter-exec console "monitor goto {}"'.format(i),
                     '-interpreter-exec console "maintenance flush register-cache"',
                     "{}-stack-list-frames 0 1000".format(i)]
        cmds.append("-gdb-exit")
        r = subprocess.run([args.gdb, "--interpreter=mi", "-nx"], input="\n".join(cmds) + "\n",
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True, timeout=1800)
    finally:
        s.wait(timeout=30)
    errors = {}
    for m in re.finditer(r'^(\d+)\^error,msg="([^"]*)"', r.stdout, re.M):
        errors[int(m.group(1))] = m.group(2)
    done = len(re.findall(r'^\d+\^done,stack=', r.stdout, re.M))
    return errors, done


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--simgdb", required=True)
    ap.add_argument("--gdb", default="arm-none-eabi-gdb")
    ap.add_argument("--core", default="m4")
    ap.add_argument("--dialect", default="m4rtl")
    ap.add_argument("--step", type=int, default=1)
    args = ap.parse_args(argv)
    elf = FIRMWARE / "state_{}.elf".format(args.core)
    trace = FIRMWARE / "traces" / "state_{}.{}.log.gz".format(args.core, args.dialect)
    with tempfile.TemporaryDirectory() as tmp:
        idx = Path(tmp) / "idx"
        subprocess.run([args.simgdb, "index", str(trace), "--elf", str(elf), "-o", str(idx)], check=True,
                       stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        info = subprocess.run([args.simgdb, "info", str(idx)], check=True, stdout=subprocess.PIPE,
                              universal_newlines=True).stdout
        end = int(re.search(r'"instructions":\s*(\d+)', info).group(1))
        hist = subprocess.run([args.simgdb, "query", str(idx), "reg-history", "psp", "--limit", "1"], check=True,
                              stdout=subprocess.PIPE, universal_newlines=True).stdout
        m = re.search(r'"visible_from":\s*(\d+)', hist)
        psp_known_from = int(m.group(1)) if m else end
        points = list(range(0, end, args.step))
        status = 0
        for label, extra in (("default", []), ("--no-psp-unwind", ["--no-psp-unwind"])):
            errors, done = scan(args, idx, elf, extra, points)
            late = [i for i in errors if i >= psp_known_from]
            print("{} {} {}: {} points, {} failed ({} after PSP is known at {}){}".format(
                args.core, args.dialect, label, len(points), len(errors), len(late), psp_known_from,
                "; e.g. " + next(iter(errors.values())) if errors else ""))
            if done + len(errors) != len(points):
                print("  missing answers: {}".format(len(points) - done - len(errors)))
                status = 1
            if late or (extra and errors):
                status = 1
        return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
