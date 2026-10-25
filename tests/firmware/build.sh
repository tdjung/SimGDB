#!/bin/sh
# Rebuild the test firmware, the QEMU tracing plugin, and the traces + truth.
# Outputs are committed so `cargo test` needs none of these tools.
#
# Needs: arm-none-eabi-gcc/objdump, QEMU >= 10.1 built with --enable-plugins
# (register/memory-value plugin APIs and discontinuity callbacks), gdb with
# Python and Arm support (gdb-multiarch or arm-none-eabi-gdb), glib headers.
#   QEMU=/path/to/qemu-system-arm QEMU_SRC=/path/to/qemu-source ./build.sh
set -e
cd "$(dirname "$0")"
: "${QEMU:=qemu-system-arm}"
: "${QEMU_SRC:?set QEMU_SRC to the QEMU source tree (for include/qemu/qemu-plugin.h)}"
for t in m4 m35p; do
  case $t in m4) cpu=cortex-m4;; m35p) cpu=cortex-m35p;; esac
  arm-none-eabi-gcc -mcpu=$cpu -mthumb -O2 -g -ffreestanding -nostdlib -fno-builtin -Wall \
    -Wl,--build-id=none -T src/link_$t.ld src/startup.c src/state.c -o state_$t.elf
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cc -shared -fPIC -O2 -Wall $(pkg-config --cflags glib-2.0) -I"$QEMU_SRC/include/qemu" \
  qemu/trace_plugin.c -o "$work/trace_plugin.so"
for t in m4 m35p; do
  QEMU="$QEMU" PLUGIN="$work/trace_plugin.so" python3 qemu/make_traces.py state_$t.elf $t traces --points 40
done
