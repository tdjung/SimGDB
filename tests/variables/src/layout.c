/* Variable layouts for SimGDB's DWARF reader, built with several DWARF
 * versions (see ../README.md). Not run: only the debug information matters. */
#include <stdint.h>

struct inner { uint8_t tag; uint32_t value; };
struct outer {
    int a;
    short b;
    long long c;
    char d[3];
    struct inner in[2];
    int e;
};
union u { uint32_t word; uint8_t bytes[4]; };

struct outer gs = {1, 2, 3, {4, 5, 6}, {{7, 8}, {9, 10}}, 11};
union u gu = {0x12345678};
uint8_t big[256];
uint16_t grid[3][5];
const char *names[4] = {"a", "b", "c", "d"};
volatile uint32_t counter;

int use(void) { static int calls; return ++calls + gs.e + gu.word + big[255] + grid[2][4] + (names[3] != 0) + counter; }
void _start(void) { use(); for (;;) {} }
