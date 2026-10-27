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
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Built from src/ so DWARF names the files `startup.c`, `state.c`.
# m4be: the m4 build, big-endian (BE8).
# m4f: the m4 build, hard-float, with fp.c: the reset handler calls fp_main
# (startup.c compiled with -Dmain=fp_main), which then calls main.
for t in m4 m35p m4be m4f; do
  case $t in
    m4) cpu=cortex-m4 ld=m4 opts=;;
    m35p) cpu=cortex-m35p ld=m35p opts=;;
    m4be) cpu=cortex-m4 ld=m4 opts=-mbig-endian;;
    m4f) cpu=cortex-m4 ld=m4 opts="-mfloat-abi=hard -mfpu=fpv4-sp-d16";;
  esac
  cflags="-mcpu=$cpu -mthumb $opts -O2 -g -ffreestanding -nostdlib -fno-builtin -Wall"
  if [ $t = m4f ]; then
    (cd src && arm-none-eabi-gcc $cflags -Dmain=fp_main -c startup.c -o "$work/startup_fp.o" &&
      arm-none-eabi-gcc $cflags -Wl,--build-id=none -T link_$ld.ld "$work/startup_fp.o" state.c fp.c -o ../state_$t.elf)
  else
    (cd src && arm-none-eabi-gcc $cflags -Wl,--build-id=none -T link_$ld.ld startup.c state.c -o ../state_$t.elf)
  fi
done
cc -shared -fPIC -O2 -Wall $(pkg-config --cflags glib-2.0) -I"$QEMU_SRC/include/qemu" \
  qemu/trace_plugin.c -o "$work/trace_plugin.so"
for t in m4 m35p m4be m4f; do
  core=${t%be}  # m4be runs on the m4 board, big-endian (from the ELF header); m4f records FP registers
  QEMU="$QEMU" PLUGIN="$work/trace_plugin.so" python3 qemu/make_traces.py state_$t.elf $core traces --points 40
done
