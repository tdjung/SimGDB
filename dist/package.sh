#!/bin/sh
# Build the closed-network package: a binary for glibc 2.17+ (RHEL/CentOS 7
# and later), the gdb helper, the self-test kit and CHECK.md.
# Needs cargo-zigbuild and zig (pip install ziglang; cargo install cargo-zigbuild).
set -e
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
target=x86_64-unknown-linux-gnu
name=simgdb-$version-linux-x86_64-glibc2.17
out=${OUT:-target/dist}
cargo zigbuild --release -p simgdb-cli --target $target.2.17
rm -rf "$out/$name" && mkdir -p "$out/$name/bin" "$out/$name/tools" \
  "$out/$name/selftest/tests/firmware/traces" "$out/$name/selftest/tests/gdb"
cp target/$target/release/simgdb "$out/$name/bin/" && strip "$out/$name/bin/simgdb"
cp tools/simgdb.gdb tools/index_all.sh tools/summarize_reports.sh tools/brief_summary.sh "$out/$name/tools/"
cp tests/firmware/state_*.elf "$out/$name/selftest/tests/firmware/"
cp tests/firmware/traces/state_*.fm.log.gz tests/firmware/traces/state_*.m4rtl.log.gz \
  tests/firmware/traces/state_*.es.log.gz "$out/$name/selftest/tests/firmware/traces/"
cp -r tests/gdb/scenarios tests/gdb/expected tests/gdb/gdb_tests.py tests/gdb/ide_session.py tests/gdb/stack_scan.py \
  tests/gdb/README.md \
  "$out/$name/selftest/tests/gdb/"
cp dist/CHECK.md dist/simgdb.toml.example docs/USAGE.md docs/LIMITATIONS.md "$out/$name/"
cp tests/ai_eval/mcp_call.py "$out/$name/tools/"
# Fail if the binary needs a newer glibc than 2.17.
newest=$(objdump -T "$out/$name/bin/simgdb" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)
[ "$newest" = GLIBC_2.17 ] || [ "$(printf '%s\nGLIBC_2.17\n' "$newest" | sort -V | tail -1)" = GLIBC_2.17 ] \
  || { echo "binary needs $newest" >&2; exit 1; }
tar czf "$out/$name.tar.gz" -C "$out" "$name"
echo "$out/$name.tar.gz ($newest)"
