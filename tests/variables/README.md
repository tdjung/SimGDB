# 변수 정답

`expected/state_*.json`: 변수 식(`seed`, `pool[2].value`, `ring[3]` …)의 주소, 크기, 타입을 gdb가 ELF에서 읽은 값
(`whatis`, `sizeof`, `&`). SimGDB 엔진의 DWARF 읽기(`simgdb-elf::variables`)와 비교합니다.
값은 `tests/firmware`의 정답 시점에 gdb가 읽은 RAM과 비교합니다.

테스트: `cargo test -p simgdb-engine --test variables`

다시 만들기: `GDB=gdb-multiarch python3 tests/variables/make_expected.py`
