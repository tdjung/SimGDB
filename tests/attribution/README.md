# 소스 귀속 비교 (D14)

SimGDB의 소스 귀속(PC → 파일, 함수, 라인, ARCHITECTURE §6)이 tarmac_to_cachegrind와 같은지 확인합니다.
코드는 독립이고 결과를 비교합니다.

- `elf/`: 비교에 쓰는 ELF.
  - `state_*`: 이 저장소의 테스트 펌웨어.
  - `inline_m4`, `line0_m4`(clang, line 0이 많음), `branchy_*`: tarmac_to_cachegrind `tests/firmware`.
- `expected/<elf>.gnu.json`, `expected/<elf>.llvm.json`: 변환기 자체 함수(`instruction_pcs`, `resolve_pcs`, `Listing`,
  `attribute_line_zero`)로 모든 명령어를 귀속한 결과. GNU `arm-none-eabi-addr2line`과 `llvm-addr2line` 각각으로 만듭니다.
- 테스트: `cargo test -p simgdb-elf --test attribution`.

## 두 도구의 차이와 SimGDB의 기준

변환기 결과는 어떤 addr2line을 쓰느냐에 따라 조금 다릅니다.

| 경우 | GNU addr2line 2.42 | llvm-addr2line | SimGDB |
|---|---|---|---|
| DWARF에 함수가 없는 코드(어셈블리, 함수 사이 padding) | 가장 가까운 앞쪽 함수 심볼 이름 | `???` | GNU와 같음 |
| DWARF 5 range list로 기술된 inline 호출(clang) | inline frame을 못 봄 → line 0 | inline frame을 봄 → 호출 라인 | LLVM과 같음 |

테스트 기준은 두 가지입니다. 함수는 GNU 결과와 같아야 합니다. 파일과 라인은 GNU 결과와 같거나, GNU가 line 0인 곳에서는 LLVM 결과와 같아야 합니다.
현재 1,158개 명령어가 모두 이 기준을 만족합니다.

## 다시 만들기

```bash
python3 tests/attribution/make_expected.py --converter /path/to/tarmac_to_cachegrind.py tests/attribution/elf/*.elf
```

변환기는 line 0 규칙이 들어간 `feature/branch-coverage` 브랜치(ed9fa51)로 만들었습니다.
