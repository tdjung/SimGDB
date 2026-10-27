# SimGDB 사용법

시뮬레이션이 남긴 Tarmac trace와 펌웨어 ELF로 인덱스를 만들고, gdb·VS Code나 명령줄 질의(JSON)로 봅니다.
보이지 않는 값과 그 이유는 [LIMITATIONS.md](LIMITATIONS.md)에 있습니다.

## 1. 인덱스 만들기

```bash
simgdb index trace.log --elf firmware.elf -o trace.idx --report > report.json
```

- trace는 gzip이어도 됩니다. ELF는 소스 줄, 함수, 변수, 초기 메모리(플래시)에 씁니다.
- `--report`의 JSON에서 먼저 볼 것:
  - `unrecognised`: 해석하지 못한 줄 종류와 샘플. 비어 있어야 정상입니다.
  - `capabilities`: 이 trace로 할 수 있는 것(메모리 값, 소스 줄 등).
  - `exception_frames_unplaced`, `exceptions_inferred`: 예외 처리에서 추정이 들어간 횟수.
  - `bus_copies`(Cortex-M RTL trace의 `B…___D/S` 레코드가 있을 때): 버스 쪽에서 본 같은 접근의 사본입니다. 인덱스는 코어 쪽
    `M…___D` 레코드를 쓰고, 사본은 코어 레코드와 맞춰 보기만 합니다(같은 접근이거나 그 일부: 버스가 정렬 안 된 접근을 나눔).
    `unmatched_writes`가 0이면 인덱스가 놓친 쓰기가 없다는 뜻입니다. 0이 아니면 `unmatched_write_samples`를 알려 주세요.
    `unmatched_reads`는 놓친 읽기로, 그 값을 모르게 할 뿐 틀린 값을 만들지는 않습니다. 샘플마다 사본의 내용과, 같은 바이트에
    가장 가까운 코어 기록(몇 명령어 앞뒤인지)이 적혀 있습니다.
  - `timings_ms`: 단계별 시간(trace 읽기, 버스 사본 점검, 정렬·쓰기, ELF 소스·변수).
  - `sources.problems`: ELF 디버그 정보 중 읽지 못한 부분. 인덱싱은 계속되고, 읽은 부분만 씁니다
    (못 읽은 곳의 함수 이름은 심볼 표에서 가져옴).
- 멀티코어 trace는 `--core N`으로 한 코어씩 만듭니다.
- **바이트 순서**: ELF 헤더를 따릅니다(big-endian/BE8 ELF면 인덱싱할 때 `data byte order: big (from elf)`가 나옴).
  ELF 없이 만들거나 ELF와 다르게 해야 하면 `--endian big|little`(또는 설정의 `[memory] endian`). `simgdb info`의 `endian`으로 확인합니다.
  gdb는 자신이 읽은 ELF로 byte 순서를 정하므로, gdb·VS Code에도 같은 ELF를 줍니다(`set endian`은 필요 없음).
- **현장 설정 파일**(`--config simgdb.toml`, 예: `dist/simgdb.toml.example`): 실행 파일을 다시 만들지 않고 바꿀 수 있는 것들입니다.
  - `[tarmac]` `record_aliases`(현장 레코드 이름 → 아는 이름), `ignore`(무시할 레코드 종류)
  - `[registers]` `aliases`(trace 레지스터 이름 → SimGDB 이름)
  - `[memory]` `volatile`(주변장치처럼 값을 믿을 수 없는 영역), `endian`(`"little"`/`"big"`, ELF 헤더 대신)
  - `[time]` cycle → 절대 시간 표(구간별 주기). 있으면 시점마다 `time`(출처 `time_map`)이 붙고 `track`도 이 표를 씁니다.
  보고서의 `unrecognised`, `unmodelled_registers`에 나온 이름은 대부분 여기서 해결됩니다. 설정은 인덱스에 함께 저장됩니다.
- 메모리: 인덱서는 trace 길이와 관계없이 수백 MB 안에서 동작합니다. 정렬 메모리는 `--sort-memory-mb`(기본 384)로 조절하고,
  그만큼 디스크에 임시 파일을 씁니다. 인덱스 크기는 대략 trace의 1.5배이고, 만드는 동안 메모리 색인 크기만큼의 여유가 더 필요합니다.

## 2. gdb / VS Code

```bash
simgdb serve trace.idx --listen 127.0.0.1:3333
arm-none-eabi-gdb firmware.elf -ex "target remote :3333" -ex "source tools/simgdb.gdb"
```

보드에 붙은 것처럼 `break`, `continue`, `step`, `next`, `finish`, `bt`, `print`, `info registers`를 씁니다.
기록을 거꾸로 가는 `reverse-step`, `reverse-next`, `reverse-continue`, `reverse-finish`도 됩니다.

| 명령 | 하는 일 |
|---|---|
| `simgdb-time` | 지금 시점: 명령어 index, trace 시간, PC |
| `simgdb-goto N` | 명령어 index N으로 이동 |
| `simgdb-goto-time T` | trace 시간 T 이후 첫 명령어로 이동 |
| `simgdb-info` | trace가 기록한 것과 SimGDB가 볼 수 없는 것 |
| `simgdb-where` | trace 흐름으로 만든 위치와 call stack(gdb가 스택 메모리를 몰라 `bt`가 끊길 때도 동작) |
| `simgdb-hits state.c:42` | 그 줄·함수·PC가 실행된 횟수와 시점 |
| `simgdb-reset` | trace 처음으로 |
| `monitor find <조건>` / `monitor find -back <조건>` | 조건이 참인 다음/이전 시점(§3의 조건식). `simgdb-goto`로 이동 |
| `monitor info source [변수\|주소]` | 지금 값들의 출처(trace, ELF, 모름)와 그 값을 만든 trace 줄 |
| `monitor bookmark <메모>`, `monitor bookmarks` | 시점 표시(AI 클라이언트와 공유) |

### VS Code

SimGDB는 이미 실행 중인 gdb 서버처럼 붙습니다(펌웨어를 내려받거나 리셋하지 않음). 먼저 `simgdb serve`를 실행해 둡니다.

cortex-debug:

```json
{
  "name": "SimGDB",
  "type": "cortex-debug",
  "request": "attach",
  "servertype": "external",
  "gdbTarget": "localhost:3333",
  "executable": "${workspaceFolder}/build/firmware.elf",
  "gdbPath": "arm-none-eabi-gdb",
  "overrideAttachCommands": ["-interpreter-exec console \"monitor halt\""]
}
```

Microsoft C/C++(cppdbg):

```json
{
  "name": "SimGDB",
  "type": "cppdbg",
  "request": "launch",
  "program": "${workspaceFolder}/build/firmware.elf",
  "cwd": "${workspaceFolder}",
  "MIMode": "gdb",
  "miDebuggerPath": "arm-none-eabi-gdb",
  "miDebuggerServerAddress": "localhost:3333",
  "stopAtEntry": true
}
```

- breakpoint, 계속, step over/into/out, call stack, 변수·watch, 레지스터, 메모리 보기가 됩니다.
- 거꾸로 가기 버튼은 확장에 따라 없을 수 있습니다. 디버그 콘솔에서 gdb 명령으로 합니다
  (cortex-debug: `reverse-next`, cppdbg: `-exec reverse-next`). `monitor …` 명령도 같은 방식입니다.
- `launch`로 펌웨어를 내려받는 설정(`-target-download`, `load`)은 쓰기라서 거부됩니다. 위처럼 attach 방식을 씁니다.
- **trace 끝(또는 거꾸로 갈 때 처음)에 닿으면** gdb는 `No more reverse-execution history.`로 멈춥니다. cppdbg는 이 정지
  이유를 몰라 "Exception has occurred. Unknown stopping event"로 보여 줍니다(세션은 살아 있음). VS Code로 볼 때는
  **`simgdb serve … --end-as-pause`**로 띄우면 계속(Continue), 거꾸로 계속, 그리고 그 둘로 처리되는 동작(줄 정보 없는 함수에서의
  step, Step Out)이 끝에 닿았을 때 일반 일시정지로 보입니다. 줄 단위 step(Step Over/Into)이 끝에 닿을 때만은 여전히
  "Unknown stopping event"입니다(gdb가 그 정지를 "아직 덜 갔음"으로 보고 끝없이 다시 step하므로 바꿀 수 없음).
  이 옵션에서 gdb 명령줄은 `continue`가 끝에 닿으면 `Program received signal SIGTRAP`으로 보여 줍니다.
- **Restart**: 서버는 다시 붙어도 위치를 유지하므로, Restart 때 trace 처음으로 가려면 launch.json에 다음을 넣습니다.
  `"postRemoteConnectCommands": [{ "text": "monitor reset" }, { "text": "maintenance flush register-cache" }]`
- 이 저장소에서는 VS Code 확장이 gdb에 보내는 것과 같은 MI 명령 순서(extended-remote 연결, `monitor halt`, breakpoint,
  continue/next/step/finish, 역방향 next, 스택, 변수 객체, 레지스터, 메모리, `monitor reset`)를 실제 gdb로 시험합니다
  (`tests/gdb/ide_session.py`). VS Code 자체로는 폐쇄망에서 확인해 주세요(`CHECK.md` §5).

### 실제 보드와 다른 점

- **값을 모르면 모른다고 보여 줍니다.** trace에 아직 나오지 않은 레지스터는 `<unavailable>`,
  기록이 없는 메모리는 읽기 오류입니다. 0 같은 값을 지어내지 않습니다.
- **쓰기는 거부됩니다.** `set var`, 레지스터·메모리 쓰기는 오류입니다(기록을 보는 도구이므로).
- **인터럽트에 선점된 명령어의 breakpoint는 한 번만 멈춥니다.**
  보드에서는 PC가 breakpoint 주소에 도착한 순간 멈출 수 있고, 그 명령어가 실행되기 전에 인터럽트가 들어오면
  핸들러에서 돌아온 뒤 같은 주소에서 한 번 더 멈춥니다. Tarmac에는 **실행된 명령어만** 기록되므로
  SimGDB는 실제로 실행될 때 한 번 멈춥니다. 그래서 `ignore`나 hit 횟수(`info breakpoints`의 "already hit")가
  보드보다 작을 수 있습니다. 멈춘 시점의 레지스터·메모리 값은 같습니다.
- **`next`/`finish` 도중 인터럽트**: 보드와 같이 핸들러를 건너뛰고 원래 함수에서 멈춥니다.
- **PSP를 쓰는 펌웨어(RTOS 등)**에서는 PSP가 처음 쓰이기 전 구간에서 gdb의 call stack 조회가 실패하고, VS Code(cppdbg)는
  "Failed to find thread 1 for break event"를 내며 끝납니다. 그 구간을 IDE로 볼 때는 `simgdb serve --no-psp-unwind`로 띄우세요.
  대신 PSP 위의 thread를 끊고 들어온 핸들러에서는 예외 frame 아래 call stack이 틀리게 보이므로, 그때는 `monitor where`를
  보세요(LIMITATIONS 참고).

## 3. 명령줄 질의

모든 질의는 JSON을 출력하고, 값마다 출처(`source`: `trace`, `elf_initial`, `inferred`, `unknown`)와
근거(`evidence`: 그 값을 만든 명령어 index와 trace 줄 번호)가 붙습니다.

### 시점

시점은 명령어 index입니다. **시점 i는 i번째 명령어를 실행하기 직전**의 상태(gdb가 그 PC에서 멈춘 상태)이고,
i번째 명령어가 쓴 값은 시점 i+1부터 보입니다. `--at INDEX` 대신 `--time T`(trace에 기록된 시간 값)를 쓸 수 있습니다.
범위는 `--from`, `--to`(양 끝 포함, 기본은 전체)입니다.

### 한 시점

| 질의 | 예 |
|---|---|
| 레지스터 | `simgdb query trace.idx registers --at 1000` |
| 메모리 | `simgdb query trace.idx memory --addr 0x20000000 --len 64 --at 1000` |
| 위치(PC, 소스 줄, call stack) | `simgdb query trace.idx locate --at 1000` |
| 변수 | `simgdb query trace.idx var 'pool[2].len' --at 1000` |
| 변수 목록 | `simgdb query trace.idx variables` |

- 변수는 고정 주소 변수(전역, 파일 static, 함수 static `func::name`)이고 `name`, `name[3]`, `name.member`를 씁니다.
  포인터를 따라가는 식(`head->next`)과 지역변수는 gdb에서 봅니다.
- `locate`의 call stack은 스택 메모리가 아니라 trace의 호출·복귀·예외 흐름으로 만듭니다. 항목마다 `pc_from`이
  그 PC의 출처입니다(`current`, `call`, `stacked`, `returned`).

### 범위 전체

| 질의 | 예 | 결과 |
|---|---|---|
| 실행 시점 | `simgdb query trace.idx executions state.c:111` | 실행 명령어 수와 진입 시점 목록 |
| 예외 목록 | `simgdb query trace.idx exceptions --from 0 --to 50000` | 종류별 개수, 진입·복귀 시점, 끊긴 PC |
| 호출 트리 | `simgdb query trace.idx calls --depth 4` | 호출 경로별 횟수·명령어 수, 함수별 합계 |
| 마지막 쓰기 | `simgdb query trace.idx last-write --var ticks --at 3000` | 각 바이트를 마지막으로 쓴 기록 |
| 쓰기 이력 | `simgdb query trace.idx writes --var ring --from 0 --to 5000` | 범위 안의 쓰기 기록 |
| 두 시점 비교 | `simgdb query trace.idx diff --a 1000 --b 2000` | 바뀐 레지스터, 바뀐 변수(원소·멤버, 스칼라는 이전/이후 값), 메모리 구간 |
| 레지스터 이력 | `simgdb query trace.idx reg-history r0 --from 0 --to 5000` | 그 레지스터의 쓰기 기록 |
| 조건 검색 | `simgdb query trace.idx find 'var("ticks") == 5'` | 조건이 처음 참인 시점 |

- **`executions`의 대상**: `0x1f4`(PC), `state.c:111`(줄), `mix`(함수).
  - 줄: `instructions`는 그 줄 명령어가 실행된 횟수(tarmac_to_cachegrind의 Ir과 같음), `entries`는 그 줄에
    **들어온** 시점입니다. 같은 줄 안에서 이어지는 실행과, 인터럽트에서 돌아와 줄을 이어 실행하는 것은 새 진입이 아닙니다.
  - 함수: `entries`는 함수 시작 주소의 실행(호출) 시점, `instructions`는 그 함수 자체 코드의 실행 횟수입니다
    (다른 함수에 inline된 사본은 포함하지 않음).
- 목록은 `--limit`(기본 100)만큼 나오고, 더 있으면 `next_cursor`를 `--cursor`로 넘겨 다음을 받습니다. `total`은 전체 개수입니다.
- **예외**: 복귀 기록 없이 이어지는 tail-chaining을 trace가 기록하지 않았으면 예외 번호(IPSR)로 추정하고,
  `evidence.note`에 그렇게 적습니다.
- **`calls`의 함수별 합계**: `inclusive_instructions`는 복귀한 호출마다 (복귀 시점 − 진입 시점)을 더한 값으로,
  그 사이에 실행된 callee와 인터럽트를 포함합니다(재귀 호출은 각각 셈). 복귀하지 않은 호출은 `unreturned_instructions`에,
  예외 핸들러는 `kind: "exception"`으로 따로 나옵니다.
- **쓰기 기록의 `inside`**: 질의한 범위 밖까지 덮는 저장(예: 변수 위로 겹친 넓은 스택 저장)이면 범위 안의 바이트만 따로 보여 줍니다.

### 조건식 (`find`)

```
var("ticks") == 5
func == "depth" && reg("r0") == 0
line == "state.c:111" && var("svc_calls") > 3
pc == sym("HardFault_Handler")
mem32(0x200000dc) != 0 || !(reg("sp") >= 0x2000ff00)
```

| 요소 | 뜻 |
|---|---|
| `pc`, `reg("r0")`, `reg("sp")` | PC, 레지스터 |
| `var("pool[3].len")`, `mem8/16/32/64(주소)` | 변수, 메모리(주소는 숫자나 `sym("이름")`) |
| `line == "file.c:123"`, `func == "name"`, `file == "file.c"` | 소스 위치(`==`, `!=`만) |
| `sym("name")` | 심볼 주소 |
| `==` `!=` `<` `<=` `>` `>=`, `&&` `\|\|` `!`, 괄호 | 비교와 논리 |

- `--backward`는 `--to`에서 거꾸로 찾아 **조건이 참인 마지막 시점**을 줍니다.
- trace가 보여 주지 않는 값이 들어간 비교는 "모름"이고, 모름은 참으로 치지 않습니다. 결과의 `first_undecided`가
  찾은 시점보다 앞서 있으면 그 사이에 판단하지 못한 시점이 있다는 뜻입니다(`undecided`는 그런 시점 수).
- 결과의 `inputs`는 찾은 시점의 레지스터·변수 값입니다. `previous`는 바로 앞 명령어와 그때 조건이 성립했는지(`holds`)이고,
  성립하지 않았다면 그 명령어가 조건을 참으로 만든 것입니다. `undecided_ranges`는 판단하지 못한 구간입니다(예: 시작 직후 초기화 전).
- 임의 코드는 실행하지 않습니다. 위 문법 밖의 식은 오류입니다.

## 4. SW 트랙 (waveform 뷰어에서 HW와 함께 보기)

```bash
simgdb track trace.idx -o sw.vcd --mark state.c:111 --mark HardFault_Handler
simgdb track es.idx -o sw.vcd --cycle-period 10ns      # 시간이 cycle인 trace
```

VCD의 `sw` scope에 신호가 생깁니다.

| 신호 | 뜻 |
|---|---|
| `pc` | PC |
| `function`, `file`, `line` | 소스 위치. function과 file은 번호이고 이름은 `sw.vcd.names.csv`에 있음 |
| `depth` | trace 흐름으로 센 호출 깊이 |
| `exception` | 처리 중인 예외 번호(0 = 없음, `0xffffffff` = 번호를 모르는 핸들러) |
| `index` | 명령어 index(`simgdb-goto`에 그대로 씀) |
| `mark_…` | `--mark`로 준 줄·함수·PC를 실행하는 동안 1 |

- 시간 축은 trace에 기록된 시간 그대로입니다(ps, ns 등). cycle로 기록된 trace는 `--cycle-period`로 주기를 주거나 인덱스의
  설정 파일에 `[time]` 표가 있어야 하고, 둘 다 없으면 거부합니다(조용히 바꾸지 않음). 주파수가 바뀌면 `[time]` 표에 구간을 나눠 적습니다.
  `--index-time`은 명령어 index를 시간으로 씁니다.
- 같은 시간에 명령어가 여럿이면 VCD에는 마지막 값만 보입니다(끝에 개수를 알려 줌).
- GTKWave에서는 `function` 신호에 `sw.vcd.gtkw-filter`를 Translate Filter File로 걸면 함수 이름으로 보입니다.
  Verdi(nWave)에서는 번호로 보이며, 이름은 `.names.csv`를 참고합니다.

## 5. AI 클라이언트 (MCP)

```bash
simgdb mcp trace.idx                       # stdio MCP 서버
claude mcp add simgdb -- simgdb mcp /path/trace.idx   # Claude Code 예
simgdb skill > .claude/skills/simgdb-debug/SKILL.md     # 분석 절차 skill 문서
```

도구는 위 질의와 같습니다: `trace_info`, `locate`, `registers`, `read_memory`, `read_variable`, `list_variables`,
`find_executions`, `call_tree`, `exceptions`, `last_write`, `writes_to`, `register_history`, `find_first`, `diff_state`,
`bookmark_add`, `bookmark_list`. 모든 도구가 시점·범위를 인자로 받고(상태 없음), 목록은 기본 20개씩입니다.
주소와 PC는 `0x…` 문자열, 시점은 10진 숫자입니다.

- **bookmark**와 **도구 호출 기록**은 인덱스 옆 `annotations.sqlite`에 남습니다(`--annotations 경로`로 바꿀 수 있음).
  인덱스 디렉터리에 쓸 수 없으면 둘 다 꺼지고 나머지는 동작합니다.
- 지역변수와 C 식(`p->next`)은 아직 MCP에 없습니다. gdb(`simgdb serve`)에서 봅니다.
