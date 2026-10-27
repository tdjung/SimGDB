#!/bin/bash
# Index every trace of many tests and keep each diagnostic report.
#
#   tools/index_all.sh TESTS_DIR "LOG_NAME..." ELF
#
#   TESTS_DIR  folder holding the test folders (searched recursively)
#   LOG_NAME   file name(s) of the traces in each test folder, e.g. "core0.log core1.log"
#   ELF        ELF name inside each test folder, or an absolute path shared by all
#
# Environment: SIMGDB (default: bin/simgdb of this package, else simgdb on PATH),
#   JOBS (parallel indexings, default 4), KEEP (1: keep the indexes, default 0:
#   keep only the reports), CONFIG (site configuration file for --config).
#
# For each trace X.log it writes X.report.json and X.err next to it (and X.idx
# with KEEP=1). Then: tools/summarize_reports.sh TESTS_DIR
set -u
ROOT=${1:?usage: index_all.sh TESTS_DIR "LOG_NAME..." ELF}
LOGS=${2:?usage: index_all.sh TESTS_DIR "LOG_NAME..." ELF}
ELF=${3:?usage: index_all.sh TESTS_DIR "LOG_NAME..." ELF}
here=$(cd "$(dirname "$0")" && pwd)
if [ -z "${SIMGDB:-}" ]; then
  if [ -x "$here/../bin/simgdb" ]; then SIMGDB=$here/../bin/simgdb; else SIMGDB=simgdb; fi
fi
JOBS=${JOBS:-4}
KEEP=${KEEP:-0}
CONFIG=${CONFIG:-}

index_one() {
  f=$1
  out=${f%.*}
  case "$ELF" in
    /*) elf=$ELF ;;
    *) elf=$(dirname "$f")/$ELF ;;
  esac
  set -- index "$f" --elf "$elf" -o "$out.idx" --report --mask
  [ -n "$CONFIG" ] && set -- "$@" --config "$CONFIG"
  if "$SIMGDB" "$@" > "$out.report.json" 2> "$out.err"; then
    echo "ok     $f"
  else
    echo "FAILED $f (see $out.err)"
  fi
  [ "$KEEP" = 1 ] || rm -rf "${out:?}.idx"
}
export -f index_one
export SIMGDB ELF KEEP CONFIG

for n in $LOGS; do find "$ROOT" -name "$n" -type f; done | sort | xargs -P "$JOBS" -I{} bash -c 'index_one "$1"' _ {}
