/* State-reconstruction test firmware for SimGDB.
 *
 * Exercises what the engine must rebuild at any point: initialised data copied
 * by startup, byte/halfword/word/doubleword stores, LDM/STM and PUSH/POP,
 * recursion, function pointers, SVC and SysTick exceptions (with and without
 * nesting into each other), and a switch of thread mode onto the process stack
 * (PSP) halfway through, so exception frames land on both stacks.
 */
#include <stdint.h>

#define NOINLINE __attribute__((noinline))

struct record {
    uint8_t kind;
    uint16_t len;
    uint32_t value;
    uint64_t stamp;
    struct record *next;
};

/* .data: initial values come from the ELF load image via the startup copy. */
uint32_t seed = 0x12345678;
uint16_t primes[8] = {2, 3, 5, 7, 11, 13, 17, 19};
const char *banner = "simgdb";

/* .rodata */
static const uint32_t table[8] = {
    0x01020304, 0x11223344, 0xa5a5a5a5, 0xdeadbeef, 0, 1, 0x80000000, 0x7fffffff,
};

/* .bss */
volatile uint32_t ticks;
volatile uint32_t svc_calls;
volatile uint32_t ring[8];
struct record pool[6];
struct record *head;
uint8_t bytes[24];
uint64_t wide;
uint32_t psp_stack[256] __attribute__((aligned(8)));
volatile uint32_t result;

NOINLINE uint32_t mix(uint32_t a, uint32_t b)
{
    a ^= b + 0x9e3779b9u + (a << 6) + (a >> 2);
    return a;
}

NOINLINE uint32_t depth(uint32_t n, uint32_t acc)
{
    uint32_t local[3] = {n, acc, n ^ acc};
    if (n == 0)
        return acc;
    return depth(n - 1, mix(acc, local[2])) + local[0];
}

NOINLINE void build_list(int n)
{
    head = 0;
    for (int i = 0; i < n; i++) {
        struct record *r = &pool[i];
        r->kind = (uint8_t)(i * 3);
        r->len = (uint16_t)(primes[i] * 100);
        r->value = mix(seed, table[i]);
        r->stamp = ((uint64_t)r->value << 32) | ticks;
        r->next = head;
        head = r;
    }
}

NOINLINE uint32_t walk_list(void)
{
    uint32_t sum = 0;
    for (struct record *r = head; r; r = r->next)
        sum += r->kind + r->len + (uint32_t)r->stamp;
    return sum;
}

NOINLINE void fill_bytes(uint8_t start)
{
    for (unsigned i = 0; i < sizeof bytes; i++)
        bytes[i] = (uint8_t)(start + i * 7);
    ((uint16_t *)bytes)[3] = 0xbeef;
    wide = ((uint64_t)table[3] << 32) | table[0];
}

typedef uint32_t (*op_fn)(uint32_t, uint32_t);
NOINLINE uint32_t op_add(uint32_t a, uint32_t b) { return a + b; }
NOINLINE uint32_t op_xor(uint32_t a, uint32_t b) { return a ^ b; }
static op_fn const ops[2] = {op_add, op_xor};

NOINLINE uint32_t apply_ops(uint32_t x)
{
    for (int i = 0; i < 4; i++)
        x = ops[i & 1](x, table[i]);
    return x;
}

static inline void svc(void)
{
    __asm__ volatile("svc #1" ::: "memory");
}

void SVC_Handler(void)
{
    svc_calls++;
    ring[svc_calls & 7] = 0x5c000000u | svc_calls;
}

void SysTick_Handler(void)
{
    uint32_t t = ++ticks;
    ring[t & 7] = mix(t, seed);
}

static void semihost_exit(int code)
{
    uint32_t block[2] = {0x20026u, (uint32_t)code};
    register uint32_t r0 __asm__("r0") = 0x20;
    register uint32_t *r1 __asm__("r1") = block;
    __asm__ volatile("bkpt 0xab" : : "r"(r0), "r"(r1) : "memory");
    for (;;) {}
}

NOINLINE void use_process_stack(void)
{
    /* Copy the live frame is not needed: switch before any locals matter. */
    uint32_t top = (uint32_t)&psp_stack[256];
    __asm__ volatile(
        "msr psp, %0\n"
        "mrs r0, control\n"
        "orr r0, r0, #2\n"
        "msr control, r0\n"
        "isb\n" : : "r"(top) : "r0", "memory");
}

NOINLINE uint32_t work(uint32_t round)
{
    uint32_t acc = round;
    build_list(6);
    acc += walk_list();
    fill_bytes((uint8_t)round);
    acc += depth(5 + round, seed);
    acc = apply_ops(acc);
    svc();
    return acc;
}

int main(void)
{
    /* SysTick often enough that interrupts land inside every function. */
    *(volatile uint32_t *)0xE000E014 = 61;
    *(volatile uint32_t *)0xE000E018 = 0;
    *(volatile uint32_t *)0xE000E010 = 7;

    uint32_t acc = 0;
    for (uint32_t round = 0; round < 3; round++)
        acc = mix(acc, work(round));
    use_process_stack();
    for (uint32_t round = 3; round < 5; round++)
        acc = mix(acc, work(round));
    result = acc;
    *(volatile uint32_t *)0xE000E010 = 0;
    semihost_exit(0);
    return 0;
}
