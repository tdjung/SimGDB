#!/usr/bin/env python3
"""Expected variable addresses, sizes and types, read by gdb from the ELF.

Test-data generator, not part of SimGDB: gdb's own DWARF reader is the
reference for simgdb-elf's (tests: crates/simgdb-engine/tests/variables.rs).

Usage: make_expected.py   (GDB overrides the gdb binary)
"""
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent
FIRMWARE = ROOT.parent / "firmware"
PATHS = [
    "banner", "bytes", "head", "ops", "pool", "primes", "psp_stack", "result", "ring", "seed",
    "svc_calls", "table", "ticks", "vectors", "wide",
    "ring[3]", "primes[7]", "pool[2]", "pool[2].value", "pool[5].stamp", "pool[1].next",
    "pool[0].kind", "pool[3].len", "bytes[23]", "vectors[15]", "table[3]", "ops[1]", "psp_stack[255]",
]



# Layouts built with DWARF 2, 4 and 5 (tests/variables/elf, from src/layout.c).
LAYOUT_PATHS = [
    "gs", "gs.a", "gs.b", "gs.c", "gs.d", "gs.d[2]", "gs.in", "gs.in[1]", "gs.in[1].value", "gs.e",
    "gu", "gu.word", "gu.bytes[3]", "big", "big[255]", "grid", "grid[2]", "grid[2][4]", "names", "names[3]",
    "counter", "use::calls",
]


def main():
    gdb = os.environ.get("GDB", "gdb-multiarch")
    targets = [(FIRMWARE / "state_{}.elf".format(c), "state_{}".format(c), PATHS) for c in ("m4", "m35p", "m4be")]
    targets += [(ROOT / "elf" / "layout_{}.elf".format(v), "layout_{}".format(v), LAYOUT_PATHS)
                for v in ("dwarf2", "dwarf4", "dwarf5", "dwarf4_be")]
    for elf, stem, paths in targets:
        args = [gdb, "-q", "-nx", "-batch", str(elf)]
        for p in paths:
            args += ["-ex", "echo @@{}\\n".format(p), "-ex", "whatis {}".format(p),
                     "-ex", "print sizeof({})".format(p), "-ex", "print/x (unsigned)&{}".format(p)]
        out = subprocess.run(args, check=True, stdout=subprocess.PIPE, universal_newlines=True).stdout
        expected = {}
        for block in out.split("@@")[1:]:
            lines = block.strip().splitlines()
            ty = re.match(r"type = (.*)", lines[1]).group(1)
            size = int(re.match(r"\$\d+ = (\d+)", lines[2]).group(1))
            addr = int(re.match(r"\$\d+ = (0x[0-9a-f]+)", lines[3]).group(1), 16)
            expected[lines[0]] = {"type": ty, "size": size, "addr": addr}
        path = ROOT / "expected" / "{}.json".format(stem)
        path.write_text(json.dumps(expected, indent=1, sort_keys=True) + "\n")
        print("{}: {} paths".format(path.name, len(expected)))


if __name__ == "__main__":
    main()
