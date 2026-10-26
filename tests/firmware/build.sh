#!/bin/sh
# Rebuild the test firmware, the QEMU tracing plugin, and the traces + truth.
# Outputs are committed so `cargo test` needs none of these tools.
#
# Needs: arm-none-eabi-gcc/objdump, QEMU >= 10.1 built with --enable-plugins
# (register/memory-value plugin APIs and discontinuity callbacks) and with
# qemu/qemu-m-bigend.patch applied (for m4be; inert without QEMU_ARM_M_BIGEND),
# gdb with Python and Arm support (gdb-multiarch or arm-none-eabi-gdb), glib headers.
#   QEMU=/path/to/qemu-system-arm QEMU_SRC=/path/to/qemu-source ./build.sh
set -e
cd "$(dirname "$0")"
: "${QEMU:=qemu-system-arm}"
: "${QEMU_SRC:?set QEMU_SRC to the QEMU source tree (for include/qemu/qemu-plugin.h)}"
# Built from src/ so DWARF names the files `startup.c`, `state.c`.
# m4be: the m4 build, big-endian (BE8).
for t in m4 m35p m4be; do
  case $t in m4) cpu=cortex-m4 ld=m4 be=;; m35p) cpu=cortex-m35p ld=m35p be=;; m4be) cpu=cortex-m4 ld=m4 be=-mbig-endian;; esac
  (cd src && arm-none-eabi-gcc -mcpu=$cpu -mthumb $be -O2 -g -ffreestanding -nostdlib -fno-builtin -Wall \
    -Wl,--build-id=none -T link_$ld.ld startup.c state.c -o ../state_$t.elf)
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cc -shared -fPIC -O2 -Wall $(pkg-config --cflags glib-2.0) -I"$QEMU_SRC/include/qemu" \
  qemu/trace_plugin.c -o "$work/trace_plugin.so"
for t in m4 m35p m4be; do
  core=${t%be}  # the machine: m4be runs on the m4 board, big-endian (from the ELF header)
  QEMU="$QEMU" PLUGIN="$work/trace_plugin.so" python3 qemu/make_traces.py state_$t.elf $core traces --points 40
done
