#!/bin/bash
# A few lines from summary.tsv (tools/summarize_reports.sh) that can be read
# off a screen and typed: per trace name, counts that decide what SimGDB may
# infer, plus the first few exceptions by test name.
#
#   tools/brief_summary.sh [summary.tsv]
#
# Output per trace name (e.g. log1), one line each:
#   N   traces, F failed to index or summarize
#   R   start at the reset handler (True/False/unknown)
#   C   CONTROL never written by the trace; Cm of them have no MSR CONTROL
#       in the executed code; Cr of those also start at reset (CONTROL = 0
#       could be inferred for these)
#   P   PSP never written; Pm with MSR PSP executed (written by code, not
#       recorded)
#   E   parse errors > 0; U unrecognised record types; W bus writes unmatched
#   T   total seconds: average / maximum
#   S   most common never-written sets (count: registers), top 3
#   X   up to 3 tests that do not fit the majority, with why
# Needs python3 (3.6 or later).
set -u
IN=${1:-summary.tsv}
python3 - "$IN" <<'EOF'
import sys
from collections import Counter, defaultdict

rows = defaultdict(list)
with open(sys.argv[1], "rb") as f:
    lines = f.read().decode("utf-8", "replace").splitlines()
for line in lines[1:]:
    c = line.split("\t")
    if len(c) >= 2:
        rows[c[1]].append(c)

def has(field, reg):
    return reg in field.split(",")

def sysregs(field):
    return dict(kv.split(":") for kv in field.split(",") if ":" in kv)

for log in sorted(rows):
    rs = rows[log]
    ok = [c for c in rs if len(c) >= 13]
    failed = [c for c in rs if len(c) < 13]
    reset = Counter(c[6] for c in ok)
    ctrl = [c for c in ok if has(c[5], "control")]
    ctrl_no_msr = [c for c in ctrl if "control" not in sysregs(c[7])]
    ctrl_infer = [c for c in ctrl_no_msr if c[6] == "True"]
    psp = [c for c in ok if has(c[5], "psp")]
    psp_msr = [c for c in psp if "psp" in sysregs(c[7])]
    perr = sum(1 for c in ok if c[3] not in ("0", ""))
    unrec = sum(1 for c in ok if c[4])
    unmatched = [int(c[9]) for c in ok if c[9].isdigit()]
    totals = [float(c[12]) for c in ok if c[12]]
    print("%s: N %d, F %d" % (log, len(rs), len(failed)))
    print("  R True %d / False %d / unknown %d" % (reset["True"], reset["False"], len(ok) - reset["True"] - reset["False"]))
    print("  C %d, Cm %d, Cr %d" % (len(ctrl), len(ctrl_no_msr), len(ctrl_infer)))
    print("  P %d, Pm %d" % (len(psp), len(psp_msr)))
    print("  E %d, U %d, W %s" % (perr, unrec,
          "-" if not unmatched else "%d traces, max %d" % (sum(1 for u in unmatched if u), max(unmatched))))
    if totals:
        print("  T %.1f / %.1f s" % (sum(totals) / len(totals), max(totals)))
    sets = Counter(c[5] for c in ok)
    for k, (s, n) in enumerate(sets.most_common(3)):
        print("  S%d %d: %s" % (k + 1, n, s or "(none)"))
    odd = [(c[0], "failed") for c in failed]
    odd += [(c[0], "reset " + c[6]) for c in ok if c[6] != "True"]
    odd += [(c[0], "MSR CONTROL") for c in ctrl if c not in ctrl_no_msr]
    odd += [(c[0], "parse errors " + c[3]) for c in ok if c[3] not in ("0", "")]
    for test, why in odd[:3]:
        print("  X %s: %s" % (test, why))
    if len(odd) > 3:
        print("  X ... %d more" % (len(odd) - 3))
EOF
