/*
 * QEMU TCG plugin: write a canonical execution log for SimGDB test data.
 *
 * This is a test-data generator, not part of SimGDB. make_traces.py turns the
 * canonical log into Tarmac dialects. One line per event:
 *
 *   I <pc> <opcode> <bytes> <disassembly>   instruction about to execute
 *   S <name> <value>                        register snapshot (all tracked
 *                                           registers, before each instruction)
 *   M R|W <addr> <size> <value>             data access by the last instruction
 *   X <irq|exc> <from_pc> <to_pc>           exception entry, after QEMU pushed
 *                                           the frame and vectored
 *   P <name> <value>                        register state right after entry
 *   F <addr> <hex bytes>                    the stacked frame (32 bytes on the
 *                                           stack EXC_RETURN selects)
 *
 * Snapshots are full (not diffs) so the generator, not this plugin, decides
 * which register writes a trace shows. With the argument fp=on the snapshots
 * also hold the floating-point registers: s0-s31 (from the gdbstub's d0-d15,
 * low word = S<2n>) and fpscr. Requires QEMU >= 10.1 (register and
 * memory-value plugin APIs, discontinuity callbacks).
 *
 * Build: cc -shared -fPIC -O2 $(pkg-config --cflags glib-2.0)
 *           -I<qemu>/include/qemu trace_plugin.c -o trace_plugin.so
 */
#include <glib.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

static const char *wanted[] = {
    "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10",
    "r11", "r12", "sp", "lr", "xpsr", "msp", "psp", "primask", "basepri",
    "faultmask", "control",
    /* fp=on only (FIRST_FP on) */
    "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10",
    "d11", "d12", "d13", "d14", "d15", "fpscr",
};
#define NWANTED (sizeof(wanted) / sizeof(wanted[0]))
#define FIRST_FP 22
static size_t nwanted = FIRST_FP;

static struct qemu_plugin_register *handles[NWANTED];
static GByteArray *buf;
static FILE *out;
static int big_endian;

typedef struct {
    uint64_t pc;
    char *text;
} Insn;

static uint64_t read_reg(int k)
{
    uint64_t v = 0;
    if (!handles[k]) {
        return 0;
    }
    g_byte_array_set_size(buf, 0);
    int n = qemu_plugin_read_register(handles[k], buf);
    if (n == 4 || n == 8) {
        /* The gdbstub's register byte order: little-endian, or big-endian
         * under QEMU_ARM_M_BIGEND (tests/firmware/qemu/qemu-m-bigend.patch). */
        for (int i = 0; i < n; i++) {
            v |= (uint64_t)buf->data[big_endian ? n - 1 - i : i] << (8 * i);
        }
    }
    return v;
}

static void dump_regs(char tag)
{
    for (size_t k = 0; k < nwanted; k++) {
        if (!handles[k]) {
            continue;
        }
        uint64_t v = read_reg(k);
        if (wanted[k][0] == 'd') {
            int n = atoi(wanted[k] + 1);
            fprintf(out, "%c s%d %08" PRIx32 "\n", tag, 2 * n, (uint32_t)v);
            fprintf(out, "%c s%d %08" PRIx32 "\n", tag, 2 * n + 1, (uint32_t)(v >> 32));
        } else {
            fprintf(out, "%c %s %08" PRIx32 "\n", tag, wanted[k], (uint32_t)v);
        }
    }
}

static void vcpu_init(qemu_plugin_id_t id, unsigned int vcpu_index)
{
    GArray *regs = qemu_plugin_get_registers();
    for (guint i = 0; i < regs->len; i++) {
        qemu_plugin_reg_descriptor *d = &g_array_index(regs, qemu_plugin_reg_descriptor, i);
        for (size_t k = 0; k < nwanted; k++) {
            if (!handles[k] && strcmp(d->name, wanted[k]) == 0) {
                handles[k] = d->handle;
            }
        }
    }
    g_array_free(regs, TRUE);
}

static void insn_exec(unsigned int vcpu_index, void *udata)
{
    Insn *insn = udata;
    dump_regs('S');
    fprintf(out, "%s\n", insn->text);
}

static void mem_access(unsigned int vcpu_index, qemu_plugin_meminfo_t info,
                       uint64_t vaddr, void *udata)
{
    qemu_plugin_mem_value v = qemu_plugin_mem_get_value(info);
    unsigned size = 1u << qemu_plugin_mem_size_shift(info);
    uint64_t value;
    switch (v.type) {
    case QEMU_PLUGIN_MEM_VALUE_U8: value = v.data.u8; break;
    case QEMU_PLUGIN_MEM_VALUE_U16: value = v.data.u16; break;
    case QEMU_PLUGIN_MEM_VALUE_U32: value = v.data.u32; break;
    default: value = v.data.u64; break;
    }
    fprintf(out, "M %c %08" PRIx64 " %u %0*" PRIx64 "\n",
            qemu_plugin_mem_is_store(info) ? 'W' : 'R', vaddr, size,
            (int)(size * 2), value);
}

static void discon(qemu_plugin_id_t id, unsigned int vcpu_index,
                   enum qemu_plugin_discon_type type, uint64_t from_pc,
                   uint64_t to_pc)
{
    fprintf(out, "X %s %08" PRIx64 " %08" PRIx64 "\n",
            type == QEMU_PLUGIN_DISCON_INTERRUPT ? "irq" : "exc", from_pc, to_pc);
    dump_regs('P');
    /* The frame is on the stack EXC_RETURN names: PSP if LR bit 2 is set. */
    uint32_t lr = 0, msp = 0, psp = 0;
    for (size_t k = 0; k < FIRST_FP; k++) {
        if (strcmp(wanted[k], "lr") == 0) {
            lr = read_reg(k);
        } else if (strcmp(wanted[k], "msp") == 0) {
            msp = read_reg(k);
        } else if (strcmp(wanted[k], "psp") == 0) {
            psp = read_reg(k);
        }
    }
    uint32_t sp = (lr & 4) ? psp : msp;
    GByteArray *frame = g_byte_array_new();
    if (qemu_plugin_read_memory_vaddr(sp, frame, 32)) {
        fprintf(out, "F %08" PRIx32 " ", sp);
        for (guint i = 0; i < frame->len; i++) {
            fprintf(out, "%02x", frame->data[i]);
        }
        fprintf(out, "\n");
    }
    g_byte_array_free(frame, TRUE);
}

static void tb_trans(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    size_t n = qemu_plugin_tb_n_insns(tb);
    for (size_t i = 0; i < n; i++) {
        struct qemu_plugin_insn *qi = qemu_plugin_tb_get_insn(tb, i);
        Insn *insn = g_new0(Insn, 1);
        uint8_t bytes[4] = {0};
        size_t len = qemu_plugin_insn_size(qi);
        qemu_plugin_insn_data(qi, bytes, sizeof(bytes));
        uint32_t op;
        if (len == 2) {
            op = bytes[0] | bytes[1] << 8;
        } else {
            /* Thumb-2: two halfwords, first halfword printed first */
            op = (uint32_t)(bytes[0] | bytes[1] << 8) << 16 | (bytes[2] | bytes[3] << 8);
        }
        char *disas = qemu_plugin_insn_disas(qi);
        insn->pc = qemu_plugin_insn_vaddr(qi);
        insn->text = g_strdup_printf("I %08" PRIx64 " %0*" PRIx32 " %zu %s", insn->pc,
                                     (int)(len * 2), op, len, disas);
        g_free(disas);
        qemu_plugin_register_vcpu_insn_exec_cb(qi, insn_exec, QEMU_PLUGIN_CB_R_REGS, insn);
        qemu_plugin_register_vcpu_mem_cb(qi, mem_access, QEMU_PLUGIN_CB_NO_REGS,
                                         QEMU_PLUGIN_MEM_RW, NULL);
    }
}

static void plugin_exit(qemu_plugin_id_t id, void *p)
{
    fflush(out);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info,
                                           int argc, char **argv)
{
    const char *path = NULL;
    big_endian = getenv("QEMU_ARM_M_BIGEND") != NULL;
    for (int i = 0; i < argc; i++) {
        if (g_str_has_prefix(argv[i], "out=")) {
            path = argv[i] + 4;
        } else if (strcmp(argv[i], "fp=on") == 0) {
            nwanted = NWANTED;
        }
    }
    out = path ? fopen(path, "w") : stdout;
    if (!out) {
        return -1;
    }
    setvbuf(out, NULL, _IOFBF, 1 << 20);
    buf = g_byte_array_new();
    qemu_plugin_register_vcpu_init_cb(id, vcpu_init);
    qemu_plugin_register_vcpu_tb_trans_cb(id, tb_trans);
    qemu_plugin_register_vcpu_discon_cb(id, QEMU_PLUGIN_DISCON_INTERRUPT | QEMU_PLUGIN_DISCON_EXCEPTION,
                                        discon);
    qemu_plugin_register_atexit_cb(id, plugin_exit, NULL);
    return 0;
}
