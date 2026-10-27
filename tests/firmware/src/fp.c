/* Floating-point part of the hard-float test build (state_m4f).
 *
 * startup.c is compiled with -Dmain=fp_main for this build, so the reset
 * handler comes here first: enable the FPU, do floating-point work with
 * SysTick running, then run the ordinary firmware (state.c main). The work
 * keeps values in S registers across calls (callee-saved S16-S31, saved with
 * VPUSH), stores floats to memory, and changes FPSCR with VMSR. Once an FP
 * instruction has run, CONTROL.FPCA stays set in thread mode, so every later
 * exception stacks an extended (FP) frame: on MSP here, and on PSP after
 * main switches stacks. The handlers use no FP, so lazy stacking never
 * writes the FP part of those frames.
 */
#include <stdint.h>

#define NOINLINE __attribute__((noinline))

extern int main(void);

/* Keeps a call a BL with a return: gdb shows a frame for a function that
 * tail-called (from DWARF call-site data), SimGDB's call stack only frames
 * that executed a call, and this firmware tests FP state, not that rule. */
static inline int no_tail_call(int v)
{
    __asm__ volatile("" : "+r"(v));
    return v;
}

float fvalues[8];
volatile float fresult;
volatile uint32_t fpscr_seen;

static const float weights[4] = {0.5f, -1.25f, 3.0f, 0.0625f};

/* noipa: callers must assume the ABI, so live values go to S16-S31. */
__attribute__((noipa)) float fscale(float x, float k)
{
    return x * k + 0.5f;
}

NOINLINE float fp_work(uint32_t round)
{
    float a = (float)round + 0.25f;
    float b = 1.0f / (float)(round + 3);
    float keep = a * b;
    for (int i = 0; i < 8; i++) {
        fvalues[i] = fscale(a, b) + keep;
        a = fvalues[i] * weights[i & 3];
        b = b + weights[(i + 1) & 3] / 7.0f;
        keep = keep * 1.0625f - a;
    }
    return keep + a;
}

static uint32_t get_fpscr(void)
{
    uint32_t v;
    __asm__ volatile("vmrs %0, fpscr" : "=r"(v));
    return v;
}

static void set_fpscr(uint32_t v)
{
    __asm__ volatile("vmsr fpscr, %0" : : "r"(v) : "memory");
}

NOINLINE int fp_run(void)
{
    *(volatile uint32_t *)0xE000E014 = 61;
    *(volatile uint32_t *)0xE000E018 = 0;
    *(volatile uint32_t *)0xE000E010 = 7;

    float acc = 0.0f;
    for (uint32_t round = 0; round < 3; round++) {
        /* Rounding mode (FPSCR.RMode) differs per round. */
        set_fpscr((get_fpscr() & ~(3u << 22)) | (round << 22));
        acc += fp_work(round);
    }
    set_fpscr(get_fpscr() & ~(3u << 22));
    fpscr_seen = get_fpscr();
    fresult = acc;
    return no_tail_call(main());
}

int fp_main(void)
{
    /* CPACR: full access to CP10/CP11 before the first FP instruction, which
     * may be in a function prologue (VPUSH): hence no FP code here. */
    *(volatile uint32_t *)0xE000ED88 |= 0xFu << 20;
    __asm__ volatile("dsb\n\tisb" ::: "memory");
    return no_tail_call(fp_run());
}
