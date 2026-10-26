#!/usr/bin/env python3
"""Source attribution expected by tarmac_to_cachegrind, for SimGDB to match.

Uses the converter's own functions (instruction_pcs, resolve_pcs, Listing,
attribute_line_zero), exactly as it attributes every ELF instruction, once
with GNU addr2line and once with llvm-addr2line (the converter accepts
either), and writes expected/<elf stem>.{gnu,llvm}.json:
  {"<pc hex>": [file, function, line], ...}

The two differ: GNU names code without a DWARF function after the nearest
symbol, LLVM gives ???; GNU 2.42 misses inline frames described with DWARF 5
range lists (clang), LLVM sees them. SimGDB must take the function as GNU
does and the line as LLVM does where GNU has line 0.

Usage: make_expected.py --converter PATH/tarmac_to_cachegrind.py ELF...
Needs arm-none-eabi-objdump, arm-none-eabi-addr2line and llvm-addr2line.
"""

import argparse
import importlib.util
import json
from pathlib import Path
import sys


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--converter", required=True, type=Path)
    ap.add_argument("elf", nargs="+", type=Path)
    args = ap.parse_args(argv[1:])
    spec = importlib.util.spec_from_file_location("converter", str(args.converter))
    c = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(c)
    objdump = c.find_objdump("arm-none-eabi-objdump", None)
    out_dir = Path(__file__).resolve().parent / "expected"
    out_dir.mkdir(exist_ok=True)
    for flavour, tool in (("gnu", "arm-none-eabi-addr2line"), ("llvm", "llvm-addr2line")):
        addr2line = c.find_addr2line(tool)
        for elf in args.elf:
            dump = c.disassemble(elf, objdump)
            pcs = c.instruction_pcs(dump)
            inline = {}
            sources = c.resolve_pcs(pcs, elf, addr2line, inline=inline)
            placed = c.attribute_line_zero(sources, inline, c.Listing(dump), targets=pcs)
            result = {"{:x}".format(pc): [s.file, s.function, s.line] for pc, s in sorted(sources.items())}
            (out_dir / "{}.{}.json".format(elf.stem, flavour)).write_text(json.dumps(result, indent=0, sort_keys=True) + "\n")
            print("{} {}: {} instructions, line 0 placed {}".format(flavour, elf.name, len(result), dict(placed)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
