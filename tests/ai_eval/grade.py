#!/usr/bin/env python3
"""Grade answers to questions.json.

  grade.py answers.json

answers.json maps question id to the answer text. An answer passes when, for
every group in `accept`, it contains at least one of the group's strings
(case-insensitive; numbers must stand alone, not inside a longer number).
"""
import json
from pathlib import Path
import re
import sys


def contains(text, token):
    if re.fullmatch(r"0x[0-9a-fA-F]+|\d+", token):
        return re.search(r"(?<![0-9a-fA-Fx]){}(?![0-9a-fA-F])".format(re.escape(token)), text, re.I) is not None
    return token.lower() in text.lower()


def main():
    questions = json.loads((Path(__file__).resolve().parent / "questions.json").read_text())["questions"]
    answers = json.loads(Path(sys.argv[1]).read_text())
    passed = 0
    for q in questions:
        text = answers.get(q["id"], "")
        missing = [g for g in q["accept"] if not any(contains(text, t) for t in g)]
        ok = not missing
        passed += ok
        print("{:4} {:18} {}".format("ok" if ok else "FAIL", q["id"], "" if ok else "missing {}".format(missing)))
    print("{}/{} passed".format(passed, len(questions)))
    return 0 if passed == len(questions) else 1


if __name__ == "__main__":
    sys.exit(main())
