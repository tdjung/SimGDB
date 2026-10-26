# SimGDB 0.1.0 폐쇄망 확인 절차

- 대상: Linux x86_64, glibc 2.17 이상(RHEL/CentOS 7 이후). 설치할 것은 없습니다.
- 빌드 기준: `tdjung/SimGDB` `feature/gdb-rsp` 브랜치 (PR #1–#3).
- 확인 결과(출력 그대로, 또는 `--mask`로 주소·값을 가린 것)를 전달해 주시면 실제 형식에 맞춰 고친 실행 파일을 다시 드립니다.

```
bin/simgdb                 실행 파일
tools/simgdb.gdb           gdb 도우미 명령 (simgdb-goto, simgdb-time, simgdb-info)
selftest/                  QEMU로 만든 테스트 trace, 펌웨어, gdb 시나리오와 정답
```

## 1. 실행 확인

```bash
bin/simgdb --version
```

기대 결과: `simgdb 0.1.0`. `GLIBC_2.xx not found`가 나오면 아래 명령의 결과를 전달해 주세요.

```bash
head -3 /etc/os-release; ldd --version | head -1
```

## 2. 자체 점검 (실제 데이터 전에, 폐쇄망의 gdb로)

Python 3.6 이상과 폐쇄망의 `arm-none-eabi-gdb`가 필요합니다.

```bash
python3 selftest/tests/gdb/gdb_tests.py check --core m4 --dialect m4rtl --simgdb bin/simgdb --gdb arm-none-eabi-gdb
```

기대 결과: 네 줄이 모두 `ok`입니다(`exceptions`, `psp`, `reverse`, `steps`). 괄호 안의 “differ only as unknown” 개수는
SimGDB가 모른다고 답한 줄 수라 0이 아니어도 정상입니다. `FAIL`이 나오면 출력 전체와 `arm-none-eabi-gdb --version` 첫 줄을 전달해 주세요.

## 3. 실제 trace 인덱싱과 진단 보고서

```bash
time bin/simgdb index <trace.log 또는 .gz> --elf <firmware.elf> -o real.idx --report --mask > report.json
du -sh real.idx
```

기대 결과:

- 화면에 `indexed N instructions from M lines in T s`. N이 실제 명령어 수와 맞는지 확인해 주세요.
- `report.json`에서 다음을 확인합니다.
  - `parse_errors.count`가 0입니다. 0이 아니면 `samples`에 해석하지 못한 줄이 있습니다.
  - `unrecognised`가 비어 있습니다. 비어 있지 않으면 거기 나온 레코드 종류를 아직 모르는 것입니다.
  - `capabilities`에서 `register_writes`, `memory_writes`, `exceptions`, `exception_stacking_writes`, `time_unit`을 확인합니다.
  - `unmodelled_registers`는 SimGDB가 아직 다루지 않는 레지스터 이름입니다(FPU 등).
- 전달 부탁: `report.json`, `time` 결과, `du -sh` 결과.

## 4. 값 대조 (Tarmac 원문과)

Tarmac에서 레지스터 쓰기(`R …`) 바로 다음에 오는 명령어 줄을 하나 고르고, 그 줄의 시간 값을 `T`라 합니다.

```bash
bin/simgdb query real.idx registers --time T > regs.json
```

기대 결과:

- 고른 `R` 레코드의 레지스터 값이 `regs.json`의 같은 레지스터 `value`(10진수)와 같습니다.
- 그 레지스터의 `evidence.line`이 그 `R` 레코드의 줄 번호입니다.
- trace가 아직 쓰지 않은 레지스터는 `"source": "unknown"`입니다.

메모리도 같은 방식으로 확인합니다. 저장 기록(`MW`/`MNW`/`ST` 등) 다음 시점에서:

```bash
bin/simgdb query real.idx memory --addr <주소> --len 4 --time T
```

`hex`에 저장한 값이 바이트 순서(작은 주소부터)대로 나옵니다.

## 5. GDB / VS Code

```bash
bin/simgdb serve real.idx --listen 127.0.0.1:3333
# 다른 터미널
arm-none-eabi-gdb firmware.elf -ex "target remote :3333" -ex "source tools/simgdb.gdb"
```

gdb에서 다음을 실행합니다.

```
simgdb-info
break main
continue
bt
info registers
next
next
reverse-next
reverse-stepi
simgdb-time
simgdb-goto 1000
set var <전역변수> = 1
```

기대 결과:

- `simgdb-info`가 trace 정보와 limitation 목록을 출력합니다.
- `continue`가 `Breakpoint 1, main () …`에서 멈춥니다.
- `bt`와 `info registers`가 나옵니다. trace가 아직 쓰지 않은 레지스터는 `<unavailable>`입니다.
- `next`, `reverse-next`, `reverse-stepi`가 소스 줄 사이를 오갑니다.
- `simgdb-time`이 `index …, time …, pc …`를 출력하고, `simgdb-goto 1000`이 그 시점의 frame을 보여 줍니다.
- `set var`는 거부됩니다(`Cannot access memory …`).

trace 끝을 지나 `continue`하면 `No more reverse-execution history.`로 멈추는 것이 정상입니다.

### call stack 대조

gdb에서 깊은 함수(가능하면 인터럽트 핸들러 안)에 breakpoint를 걸고 `continue`한 뒤 `simgdb-time`으로 index `I`를 얻고
`bt`를 적어 둡니다. 그다음:

```bash
bin/simgdb query real.idx locate --at I
```

기대 결과:

- `call_stack`의 각 항목 `source.function`이 `bt`의 함수와 같은 순서입니다(gdb는 `main`에서 멈추고, inline 함수는
  `source.inline`에 들어 있습니다). 예외 항목(`"kind": "exception"`) 다음 자리가 gdb의 `<signal handler called>`입니다.
- 줄 번호는 같거나, 함수 첫 명령어 등에서 한 줄 다를 수 있습니다(ARCHITECTURE §6의 line table 규칙 차이).
- 다르면 `locate` 출력과 `bt` 출력을 함께 알려 주세요. RTOS 문맥 전환이 있는 펌웨어라면 그 사실도 알려 주세요.

## 6. 범위 질의

실제 trace에서 다음을 실행하고 결과를 확인합니다(사용법: `docs/USAGE.md` §3).

```bash
bin/simgdb query real.idx exceptions --limit 3 > exc.json
bin/simgdb query real.idx calls --depth 3 > calls.json
bin/simgdb query real.idx executions <자주 실행되는 함수 이름> --limit 3
bin/simgdb query real.idx variables > vars.json
bin/simgdb query real.idx find 'pc == sym("<예외 핸들러 이름>")'
```

기대 결과:

- `exc.json`의 `by_exception`이 펌웨어가 쓰는 예외(SysTick, 인터럽트 번호 등)와 맞습니다. 첫 항목의 `entered_at`을
  gdb에서 `simgdb-goto <entered_at>` 하면 그 핸들러 첫 줄에서 멈춥니다.
- `calls.json`의 `functions`에 주요 함수가 호출 횟수와 함께 나옵니다.
- `executions`의 `entries.total`이 그 함수가 호출된 횟수입니다. 첫 항목으로 `simgdb-goto` 하면 그 함수 첫 줄입니다.
- `vars.json`에 전역변수가 주소·타입과 함께 나옵니다. 그중 하나를 `bin/simgdb query real.idx var <이름> --at I`로 읽은 값이
  gdb에서 같은 시점(`simgdb-goto I`)에 `print <이름>`한 값과 같습니다(모르는 바이트는 `??`).
- `find` 결과의 `found.index`가 첫 예외 항목의 `entered_at`과 같습니다.
- 다르거나 오류가 나면 명령과 출력을 알려 주세요. `report.json`의 `exceptions_inferred`도 함께 알려 주세요.

## 7. MCP 서버

AI 클라이언트 없이도 동작을 확인할 수 있습니다(`tools/mcp_call.py`, Python 3.6 이상).

```bash
SIMGDB=bin/simgdb python3 tools/mcp_call.py real.idx                 # 도구 목록
SIMGDB=bin/simgdb python3 tools/mcp_call.py real.idx trace_info
SIMGDB=bin/simgdb python3 tools/mcp_call.py real.idx exceptions '{"limit": 2}'
```

기대 결과: 도구 16개가 나열되고, `trace_info`의 `exceptions`가 §6의 `exceptions` 결과와 같습니다.
AI 클라이언트를 쓸 수 있으면 `bin/simgdb mcp real.idx`를 stdio MCP 서버로 등록하고 "주요 인터럽트가 몇 번 들어왔고
첫 진입 시점은?"처럼 물어본 뒤, 답의 시점을 gdb에서 `simgdb-goto <index>`로 확인합니다.
도구 호출 기록은 인덱스 옆 `annotations.sqlite`(`call_log` 표)에 남습니다.

## 8. 함께 알려 주시면 좋은 것 (Step 0)

- ES `tic`이 무엇을 세는지(코어 클럭인지 전역 tick인지), 클럭이 멈추면 카운터도 멈추는지, 시뮬레이션 중 주파수가 바뀌는지.
- `IF` 레코드의 의미(바로 앞 명령어의 중복인지).
- Cortex-M RTL trace에서 메모리 레코드(`MNR`/`MNW`, `BNR` 등)가 그것을 일으킨 명령어 줄 **뒤에** 나오는지,
  아니면 몇 명령어 늦게 나오는지(파이프라인 순서). SimGDB는 지금 “다음 명령어 줄 전까지의 레코드는 앞 명령어의 것”으로 봅니다.
- 펌웨어가 PSP를 쓰는지(RTOS 사용 여부).
