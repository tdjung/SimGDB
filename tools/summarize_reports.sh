#!/bin/bash
# One table row per trace from the reports tools/index_all.sh wrote.
#
#   tools/summarize_reports.sh TESTS_DIR [OUT.tsv]      (default OUT: summary.tsv)
#
# Columns: test folder, trace, instructions, parse errors, unrecognised record
# types, registers never written, whether the trace starts at the reset
# handler, system registers the executed instructions wrote (MSR/CPS, with
# counts), where the byte order came from, bus-side
# writes with no core record ("-": no bus copies), seconds reading the trace,
# of which file input, and total seconds. Runs while indexing goes on too.
# Needs python3 (3.6 or later).
set -u
ROOT=${1:?usage: summarize_reports.sh TESTS_DIR [OUT.tsv]}
OUT=${2:-summary.tsv}
find "$ROOT" -name '*.report.json' -type f | sort | python3 -c '
import json, sys
print("\t".join(["test", "log", "instructions", "parse_errors", "unrecognised", "never_written",
                 "starts_at_reset", "sysreg_writes", "endian", "unmatched_writes", "read_s", "input_s", "total_s"]))
for p in (l.strip() for l in sys.stdin):
    parts = p.rsplit("/", 2)
    name = [parts[-2], parts[-1].replace(".report.json", "")]
    try:
        d = json.load(open(p))
    except Exception:
        # Indexing failed: its message is in the .err file next to it.
        try:
            err = [l for l in open(p[: -len(".report.json")] + ".err") if l.startswith("error")][:1]
        except OSError:
            err = []
        print("\t".join(name + ["FAILED: " + (err[0].strip() if err else "no report")]))
        continue
    r, t = d["report"], d.get("timings_ms", {})
    b = r.get("bus_copies") or {}
    row = name + [r["instructions"], r["parse_errors"]["count"], ",".join(sorted(r["unrecognised"])),
                  ",".join(r["registers_never_written"]), r.get("starts_at_reset", "-"),
                  ",".join("%s:%d" % kv for kv in sorted(r.get("system_register_writes_by_instruction", {}).items())),
                  r.get("endian_source", ""),
                  b.get("unmatched_writes", "-"), t.get("read_trace", 0) / 1000,
                  t.get("read_trace.input", 0) / 1000, sum(v for k, v in t.items() if "." not in k) / 1000]
    print("\t".join(str(x) for x in row))
' > "$OUT"
echo "$OUT: $(($(wc -l < "$OUT") - 1)) traces"
