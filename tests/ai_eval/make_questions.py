#!/usr/bin/env python3
"""AI evaluation questions for the SimGDB MCP tools, with expected answers
computed without SimGDB (from tests/queries/expected and the canonical QEMU
log of the run; see tests/queries/README.md).

Test-data generator, not part of SimGDB.
Usage: make_questions.py      (NM, OBJDUMP override tool paths)
"""
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT.parent / "queries"))
from make_expected import Run, function_at  # noqa: E402

CORE = "m4"


def main():
    run = Run(CORE)
    exp = json.loads((ROOT.parent / "queries" / "expected" / "state_{}.json".format(CORE)).read_text())
    insns = run.insns
    func = lambda i: function_at(run.funcs, insns[i].pc)  # noqa: E731
    questions = []

    def add(qid, question, accept, note):
        questions.append({"id": qid, "question": question, "accept": accept, "note": note})

    systick = [e for e in exp["exceptions"] if e["number"] == 15]
    add("systick_count", "How many SysTick interrupts were taken during the run?",
        [[str(len(systick))]], "count of exception entries with number 15 (one is a tail-chain without a record)")

    ff = next(f for f in exp["find_first"]
              if f["condition"] == 'var("svc_calls") == 3' and f["direction"] == "forward" and f["from"] == 0)
    i = ff["found"]
    add("svc_calls_3", "At which instruction index does the global svc_calls first hold the value 3, "
        "and which function wrote it?",
        [[str(i), str(i - 1)], [func(i - 1)]], "first point where svc_calls == 3 (the writer is index-1)")

    top = max(exp["calls"].items(), key=lambda kv: kv[1]["calls"])
    add("most_called", "Which function was called the most times, and how many times?",
        [[top[0]], [str(top[1]["calls"])]], "calls by bl/blx")

    result = run.objects["result"][0]
    ticks = run.objects["ticks"][0]
    # The last store (the first one is startup clearing .bss).
    w = max(k for k, insn in enumerate(insns) for (rw, a, s, v) in insn.mem if rw == "W" and a == result)
    mem = {}
    for k in range(w + 1):
        for (rw, a, s, v) in insns[k].mem:
            for b in range(s):
                mem[a + b] = (v >> (8 * b)) & 0xFF
    tval = sum(mem[ticks + b] << (8 * b) for b in range(4))
    add("ticks_at_result", "What was the value of the global ticks when main stored its final value to the global result?",
        [[str(tval)]], "ticks at the point after the store to result (index {})".format(w + 1))

    pool3 = run.objects["pool"][0] + 3 * 24 + 2
    last = None
    for k in range(3000):
        for (rw, a, s, v) in insns[k].mem:
            if rw == "W" and a <= pool3 < a + s:
                last = (k, insns[k].pc, (v >> (8 * (pool3 - a))) & 0xFFFF if s >= 2 else v)
    add("pool3_len_writer", "Before instruction index 3000, which instruction last wrote pool[3].len "
        "(give its index and PC) and what value did it write?",
        [[str(last[0])], [hex(last[1]), str(last[1])], [str(last[2])]], "last store covering pool[3].len before point 3000")

    add("line44", "How many times was source line state.c:44 entered, and how many instructions on that line "
        "executed in total?",
        [[str(len(exp["line_visits"]["state.c:44"]))], [str(exp["line_ir"]["state.c:44"])]],
        "entries per tests/queries definition; instruction count = tarmac_to_cachegrind Ir")

    svc = next(e for e in exp["exceptions"] if e["number"] == 11)
    add("first_svc", "At which instruction index was the first SVC exception taken, and which function was "
        "executing the SVC?",
        [[str(svc["entry"])], [function_at(run.funcs, svc["interrupted_pc"])]], "first SVCall entry")

    sp = next(f for f in exp["find_first"]
              if f["condition"].startswith('reg("sp") <') and f["direction"] == "forward" and f["from"] == 0)
    add("sp_low", "When does the stack pointer first go below 0x2000ff80, and in which function?",
        [[str(sp["found"])], [func(sp["found"])]], "first point with sp < 0x2000ff80")

    d = exp["diff"][0]
    names = sorted({n for n, (a, s) in run.objects.items() for b in d["bytes"] if a <= b < a + s})
    add("diff_vars", "Which global variables changed between instruction index {} and {}?".format(d["a"], d["b"]),
        [[n] for n in names], "variables covering bytes that changed (known at both points)")

    first_r4 = next(k for k in range(1, run.n) if insns[k].before["r4"] != insns[k - 1].before["r4"])
    add("r4_unknown", "What is the value of register r4 at instruction index 10?",
        [["unknown", "not known", "not recorded", "unavailable"]],
        "the trace first writes r4 at index {} (renderer emits a register only when it changes)".format(first_r4))

    out = {"core": CORE, "trace": "tests/firmware/traces/state_m4.m4rtl.log.gz", "elf": "tests/firmware/state_m4.elf",
           "questions": questions}
    (ROOT / "questions.json").write_text(json.dumps(out, indent=1) + "\n")
    print("{} questions".format(len(questions)))


if __name__ == "__main__":
    main()
