# 변수 정답

`expected/state_*.json`: 변수 식(`seed`, `pool[2].value`, `ring[3]` …)의 주소, 크기, 타입을 gdb가 ELF에서 읽은 값
(`whatis`, `sizeof`, `&`). SimGDB 엔진의 DWARF 읽기(`simgdb-elf::variables`)와 비교합니다.
값은 `tests/firmware`의 정답 시점에 gdb가 읽은 RAM과 비교합니다.

테스트: `cargo test -p simgdb-engine --test variables`

다시 만들기: `GDB=gdb-multiarch python3 tests/variables/make_expected.py`

## DWARF 버전별 배치 (`elf/layout_dwarf{2,4,5}.elf`)

`src/layout.c`(구조체 안의 배열과 구조체, union, 2차원 배열, 256개 배열, 함수 static)를 DWARF 2, 4, 5로 빌드한 ELF입니다.
DWARF 2/3은 구조체 멤버 위치를 식(`DW_OP_plus_uconst`)으로 적으므로 이 형식을 따로 시험합니다.
테스트: `cargo test -p simgdb-elf --test variables_layout`(정답은 같은 `make_expected.py`가 gdb로 만듦).

```bash
for v in 2 4 5; do arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -O1 -gdwarf-$v -ffreestanding -nostdlib -Wl,--build-id=none \
  -Wl,-Ttext=0x1000 -Wl,-Tdata=0x20000000 tests/variables/src/layout.c -o tests/variables/elf/layout_dwarf$v.elf; done
# big-endian (BE8): DWARF을 big-endian으로 읽는지
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -mbig-endian -O1 -gdwarf-4 -ffreestanding -nostdlib -Wl,--build-id=none \
  -Wl,-Ttext=0x1000 -Wl,-Tdata=0x20000000 tests/variables/src/layout.c -o tests/variables/elf/layout_dwarf4_be.elf
```
