# 아키텍처

Step 3까지 고려한 구조입니다. 구현은 단계별로 하지만 §2의 결정은 **Step 1에서 고정**합니다.
나중에 바꾸려면 엔진을 다시 만들어야 하기 때문입니다.

## 1. 전체 구조

```
                         ┌───────────── 앞단 (얇은 어댑터) ─────────────┐
                         │  GDB RSP 서버 ── VS Code, gdb (사람)          │
   ┌──────────────┐      │  MCP 서버 ────── AI 에이전트                  │
   │  질의 엔진    │◄─────┤  CLI / Python API ── 스크립트, CI, 일괄 분석   │
   │ (Hangar)     │      │  뷰어 어댑터 ── Verdi, SimVision, Surfer …    │
   └──────┬───────┘      └───────────────────────────────────────────────┘
          │
   ┌──────┴───────┐     ┌──────────────┐
   │ 인덱스 저장소 │◄────┤ 수집·인덱싱   │◄── 공급자: Tarmac (Step 1)
   │ (run별 테이블)│     │ (Runway)     │◄── 공급자: waveform (Step 2)
   └──────────────┘     └──────────────┘◄── 공급자: RISC-V 로그 (Step 3)
          ▲
   ┌──────┴───────┐
   │ ISA 정의      │  Arm M-profile, RISC-V: 레지스터, target XML, 명령어 분류, 툴체인
   │ ELF/DWARF    │  심볼, line table, 초기 메모리, inline/line 0 귀속
   └──────────────┘
```

- **사람과 AI가 같은 엔진**을 씁니다. 같은 질문에 같은 답이 나옵니다.
- 앞단은 엔진 API를 프로토콜로 바꾸는 일만 합니다. 로직은 엔진에 둡니다.
- 이름은 로드맵 논의에서 쓴 비유입니다(Runway = 수집, Hangar = 엔진·디버깅, Tower = AI·여러 run, Radar = HW).
  코드에서는 기능 이름의 crate를 씁니다(§7).

## 2. 처음부터 고정할 설계 결정

1. **시점(Point)**: `(run_id, core_id, index, sim_time)`.
   - `index`는 그 core의 명령어 레코드 순번(0부터). **시점 `index = i`는 i번째 명령어를 실행하기 직전**의
     상태입니다(GDB가 `pc_i`에서 멈춘 상태). i번째 명령어의 레지스터·메모리 기록은 시점 i+1부터 보입니다.
     명령어가 N개면 마지막 명령어 실행 후 상태는 `index = N`(끝 시점)입니다.
   - index에 넣는 레코드: 실행된 명령어(`IT`, `ES`)와 **조건 실패 명령어**(`IS`, `ES … CCFAIL`, flag로 표시).
     `IF` 중복은 넣지 않습니다. 예외는 별도 테이블입니다.
   - `sim_time`은 **기록된 값 그대로** `{value, unit}`입니다. unit은 `ps`/`ns` 같은 시간 또는 `cycle`(ES `tic`).
     조용히 변환하지 않습니다(D16).
   - 절대 시간 `time`은 run 메타데이터의 **시간 변환 표**로 계산하는 파생 값이며 출처를 가집니다.
     표는 구간 목록 `(cycle 시작, 시각 시작, 주기)`이고 배율 하나가 아닙니다(주파수 변경, clock gating).
     표의 출처: 테스트벤치의 주파수 변경 기록, 사용자가 준 표, Step 2의 waveform–trace 자동 비교(D9).
     표가 없으면 `time`은 `unknown`이고 cycle 기준 기능은 그대로 동작합니다.
   - Step 1은 run 하나, core 하나여도 모든 API가 이 형식을 씁니다.
2. **인덱스형 저장소**: 재생(replay)이 아니라 테이블 + 역색인 + checkpoint.
   - AI 질의(`find_executions`, `last_write`)는 범위 전체를 훑으므로 재생 방식으로는 느립니다.
   - 사람의 step은 checkpoint + 짧은 재생으로 충분합니다.
3. **값과 출처**: 모든 값은 `{value, source, evidence, conflicts}`.
   - `source`: `trace` / `waveform` / `elf_initial` / `inferred` / `unknown`.
   - `evidence`: 그 값을 만든 레코드(시점, tarmac 라인 번호 등).
   - `conflicts`: 출처끼리 다를 때 다른 값들(Step 2).
4. **조건식 언어(작은 DSL)**: `pc == sym("HardFault_Handler")`, `reg("r0") == 0`, `line == "a.c:938"`,
   이후 `signal("u_irq.pend") == 1`. AI에게 임의 코드를 실행시키지 않고 검증 가능한 질의만 허용합니다.
5. **긴 질의는 비동기**: 작업 ID → 진행 → 결과 조회. 결과는 개수 제한과 페이지 넘김(cursor).
6. **기능 탐색(capability)**: 데이터셋마다 가능한 기능과 제약을 엔진이 보고합니다
   (waveform 유무, 코어 수, Tarmac에 메모리 쓰기 데이터가 있는지 등). MCP 도구 목록과 GDB `monitor`도 이를 따릅니다.
7. **주석 저장소**: bookmark와 메모를 데이터셋 옆 파일에 저장. 사람(VS Code)과 AI가 공유.
8. **호출 로그**: MCP 도구 호출과 결과 요약을 남깁니다(평가와 skill 개선용).
9. **ISA 정의 계층**: 레지스터 목록·크기·GDB target XML, 명령어 분류기(분기, 흐름 끊김, 조건부, IT 블록/압축 명령어),
   툴체인 이름. 엔진과 앞단은 ISA를 직접 알지 않습니다.
10. **읽기 전용**: GDB의 쓰기 명령(`G`, `P`, `M`, `X`)은 오류로 응답. Step 3에서 실행 도구가 생기면 별도 권한으로 분리.
11. **AI 사용 방식 기준 설계(D15)**: 개발자와 AI는 접근 방식이 다릅니다.

    | 측면 | 개발자 | AI |
    |---|---|---|
    | 접근 | 한 시점 주변, step | 범위 전체 훑기, 집계, 여러 run |
    | 상태 | 세션(현재 위치, breakpoint) | 상태 없음, 모든 호출에 시점 명시 |
    | 출력 | 소스 화면, GDB 텍스트 | JSON, 출처·증거, 개수 제한, 요약 먼저 |
    | 변수 | GDB가 DWARF 해석 | 훑기(“x가 5가 된 첫 시점”)에는 엔진이 변수 → 주소·타입을 알아야 함 |
    | call stack | GDB가 스택 메모리로 backtrace | trace 흐름(호출·복귀)으로 만든 호출 표, 메모리를 몰라도 됨 |

    - 저장소는 **훑기 우선**(열 기반)으로 만들고, 한 시점 조회는 checkpoint로 처리합니다.
    - **파생 표를 인덱싱 때 만듭니다**: 호출·복귀 표, PC별 실행 횟수, 주소별 쓰기 이력(§3).
    - 엔진 API는 **두 층**: 한 시점의 상태를 주는 API와 조건에 맞는 시점 목록·개수를 주는 API(§4). 같은 저장소를 씁니다.
    - call stack은 GDB backtrace와 trace 흐름 호출 표 두 방법으로 만들 수 있고, 서로 검증에 씁니다.
12. **엔진 자체 DWARF 해석**: `gimli`로 엔진이 직접 읽습니다. Step 1은 심볼, line table, inline, 전역변수의
    주소·타입. 지역변수(location list)는 이후. 한 시점의 C 식 해석(`read_variable`)은 GDB를 써도 됩니다.
13. **성능 목표 두 개**: 대화형(step, 레지스터, 메모리, breakpoint 정지) **100 ms 이하**,
    분석형(명령어 1억 개 trace 전체 훑기) **수 초**. 여러 run은 비동기 작업(§2.5).
    대용량 trace로 측정해 숫자로 보고합니다.
14. **반입 전용 배포**: 폐쇄망에서는 코드를 고치지 않고 외부에서 빌드한 실행 파일만 반입합니다.
    - 실행 파일은 폐쇄망 glibc에 맞추거나 정적 링크합니다.
    - 바뀔 수 있는 것은 **설정 파일**로 둡니다: Tarmac 표기 변형, 레지스터 이름 별칭, 시간 변환 표, 메모리 영역.
    - **진단 기능**을 처음부터 넣습니다: `index --report`가 레코드 종류별 개수, 해석하지 못한 줄의 종류와 샘플
      (주소·값 가림 옵션), 복원 가능한 값의 범위를 요약합니다. 이 요약을 밖으로 가져와 고친 실행 파일을 다시 반입합니다.
    - Step 2의 Verdi NPI 읽기처럼 벤더 라이브러리가 필요한 부분만 **폐쇄망에서 빌드하는 별도 C/C++ 보조 프로그램**으로 분리합니다.

## 3. 데이터 모델 (run별)

| 테이블 | 열 (예) | 비고 | 저장 |
|---|---|---|---|
| `instructions` | index, sim_time, pc, encoding, flags(ccfail, exception 진입 등), 원본 줄 번호 | core별 | 열 파일 |
| `reg_writes` | index, reg, value | Tarmac `R` 레코드 | 열 파일 |
| `mem_accesses` | index, kind(read/write), addr, size, value, master | Tarmac `LD`/`ST`/`MR`/`MW`…, Step 2는 bus 트랜잭션(master 포함) | 열 파일 |
| `exceptions` | index, kind(entry/return), number | | 열 파일 |
| `checkpoints` | index, 레지스터 전체(값 + 알려짐 여부) | 간격 설정 가능. 메모리는 쓰기 이력으로 복원 | 열 파일 |
| 역색인 | pc → indexes, addr → write indexes | 정렬 배열 + 이진 탐색 | 열 파일 |
| **파생: `calls`** | 종류(call/exception), 호출 index, 진입 index, 복귀 index, 호출 PC, 복귀 주소, 예외 번호, 부모, 깊이 | trace 흐름(호출·복귀·예외)으로 만듦(스택 메모리 불필요). `instructions.frame`이 명령어마다 안쪽 frame을 가리킴. `locate`의 call stack, AI의 `call_tree` | 열 파일 |
| **파생: `pc_hits`** | pc, 실행 횟수, 첫/마지막 index | 요약 먼저 응답 | 열 파일 |
| `signals` (Step 2) | sim_time, signal_id, value | 필요한 신호만, SFR·bus 등 | 열 파일 |
| `sources` (공유) | pc → file, line, function, line 0 배치 방식, inline frame(`frames`), 이름 표(`strings`) | ELF 기준, line 0 규칙 적용 | 열 파일 |
| `variables` (공유) | 고정 주소 변수 이름 → 주소, 크기, 타입(타입은 트리) | 엔진 DWARF(§2.12) | JSON(`variables.json`, 작고 통째로 읽음) |
| `symbols` (공유) | ELF 심볼 이름, 주소, 크기, 함수 여부 | `sym("…")`, 함수 진입 주소 | JSON(`symbols.json`) |
| 메타데이터 | ISA, core, ELF 경로·해시, 시간 단위·변환 표, 공급자 버전, capability, 진단 요약 | | SQLite |
| 주석·호출 로그·작업 | bookmark, 메모, MCP 호출 로그, 비동기 작업 상태 | 사람과 AI가 동시에 씀 | SQLite |

ELF와 소스 귀속은 run 사이에 공유합니다(같은 ELF면 한 번만).
인덱서는 스트리밍입니다: 큰 표는 batch(2^20행)마다 파일로 쓰고, 정렬이 필요한 색인은 외부 정렬(run 파일 + 병합)로 만들며,
호출 표는 최근 행만 메모리에 두고 닫힐 때 파일의 복귀 시점을 고칩니다. 메모리는 trace 길이와 무관하게 수백 MB입니다.
열 파일은 Arrow IPC(압축 없음, mmap)이며 형식 버전을 데이터셋 manifest에 둡니다.

## 4. 엔진 API (개념)

```
dataset.info() -> capabilities, runs, cores, limitations
locate(point) -> pc, function, file:line, call stack (evidence)
state.registers(point) -> {reg: Value}
state.memory(addr, size, point) -> [Value per byte or word]
query.executions(target: line|function|pc, range, limit, cursor) -> 실행 수 + 진입 시점
query.last_write(addr|variable, len, point) -> 바이트별 마지막 쓰기 기록
query.writes_to(addr|variable, len, range, limit, cursor) -> [쓰기 기록]
query.find_first(condition, range, direction) -> point + 입력 값 + 판단 못 한 시점
query.exceptions(range, limit, cursor) -> 종류별 개수 + [진입, 복귀, 끊긴 PC]
query.call_tree(range, depth) -> 경로별 트리 + 함수별 합계
diff(point_a, point_b, limit, cursor) -> changed registers / memory runs
read_variable(path, point) -> 값 + 바이트별 출처
annotations.add/list(point, text)
(Step 2) hw.sfr(name, point), hw.sfr_history(name, range), hw.bus(range, filter), hw.signal_changes(path, range)
(Step 3) runs.first_divergence(run_a, run_b), runs.where_executed(target, runs)
```

모든 시점 인자는 `index` 또는 `sim_time`으로 줄 수 있고, 엔진이 `Point`로 바꿉니다.
의미와 예는 [USAGE.md](USAGE.md) §3. 조건식은 값이 바뀔 수 있는 시점(대상 코드의 실행, 레지스터 쓰기,
읽는 바이트의 메모리 기록)만 평가하고, 모르는 값은 3값 논리(참/거짓/모름)로 다룹니다.
같은 `sim_time`에 명령어가 여럿이면 그중 첫 명령어의 시점입니다.

API는 두 층입니다(§2.11). `locate`, `state.*`, `diff`는 한 시점 층이고, `query.*`는 조건에 맞는
시점 목록·개수·요약을 주는 집합 층입니다. 집합 층은 개수 제한과 cursor를 기본으로 합니다.

## 5. 앞단

### GDB RSP 서버

`gdbstub` crate로 구현합니다(D13). 패킷·checksum·기능 협상은 crate가, 아래 처리는 SimGDB가 담당합니다.

| 패킷 | 처리 |
|---|---|
| `qSupported`, `qXfer:features:read` | ISA 정의의 target XML (Arm: `org.gnu.gdb.arm.m-profile` 등) |
| `?`, `g`, `p`, `m` | 현재 시점의 엔진 상태. 모르는 값은 `xx`(unavailable) |
| `s`, `c`, `vCont` | 인덱스로 다음 시점 계산 |
| `bs`, `bc` | 역방향 (GDB reverse execution) |
| `Z0`/`z0` (Step 1), `Z2`–`Z4` (Step 3) | breakpoint는 PC 역색인, watchpoint는 쓰기 질의 |
| `G`, `P`, `M`, `X` | 거부 (읽기 전용) |
| `qRcmd` (`monitor …`) | `time`, `goto`, `hits`, `info source`, `bookmark` |

정지할 때마다 이벤트 채널로 `{point}`를 보내 뷰어 어댑터와 VS Code 보조 확장이 따라오게 합니다(Step 2).

### MCP 서버 (AI)

공식 MCP Rust SDK(`rmcp`)로 같은 실행 파일에 넣습니다.

설계 원칙: 도구는 **적고 원자적이고 상태가 없음**(모든 도구가 시점을 인자로 받음), **구조화된 JSON**,
**양 제한 + 요약 우선**, 모든 값에 **출처와 증거**, `unknown`을 명시, **skill 문서** 제공.

| 도구 | 단계 |
|---|---|
| `trace_info` (기능·제약 포함) | 1 |
| `locate` | 1 |
| `find_executions` | 1 |
| `call_tree` | 1 |
| `registers`, `read_memory` | 1 |
| `read_variable`, `list_variables` (고정 주소 변수, 엔진 DWARF §2.12. C 식·지역변수는 gdb를 SimGDB RSP에 붙이는 방식으로 이후) | 1 |
| `last_write`, `writes_to`, `register_history` | 1 |
| `exceptions` | 1 |
| `find_first` (조건식) | 1 |
| `diff_state` | 1 |
| `bookmark_add`, `bookmark_list` | 1 |
| `job_status`, `job_result` (비동기) | 2 (구조는 1) |
| `sfr`, `sfr_history`, `bus_transactions`, `signal_changes` | 2 |
| `first_divergence`, `where_executed` (여러 run) | 3 |
| `explain` (서버 측 분석 에이전트) | 3 |

구현(`simgdb-mcp`): stdio 전송, 도구 16개, 주소·PC는 16진 문자열, 기본 목록 20개(최대 1000). 도구 오류는 MCP tool error(`isError`)와
고칠 수 있는 메시지로 돌려줍니다. 호출마다 도구, 인자, 결과 요약, 시간을 `annotations.sqlite`의 `call_log`에 남기고(§2.8),
bookmark도 같은 파일에 둡니다(§2.7). skill 문서는 `crates/simgdb-mcp/skill/SKILL.md`(`simgdb skill`로 출력).

### CLI / Python API

`index`(인덱스 생성), `serve`(GDB), `mcp`, `query`(엔진 질의를 JSON으로), `track`(SW 트랙 waveform).

### 뷰어 어댑터 (Step 2)

공통 인터페이스: `goto(time)`, `add_signals(paths)`, `on_time_selected(callback)`.
구현: Verdi(Tcl/NPI), SimVision(Tcl), Surfer(WCP), VaporView(VS Code 명령; API 공개 상태 확인 필요).

## 6. 소스 귀속 규칙 (변환기와 동일하게 유지)

코드는 변환기와 독립이고, 이 절의 규칙이 명세입니다(D14). 같은 ELF에서 두 도구의
PC → (파일, 라인, 함수) 결과를 비교하는 테스트로 일치를 확인합니다. 규칙을 바꾸면 양쪽을 함께 바꿉니다.

- `addr2line -a -f -i -C`: 라인은 첫(가장 안쪽) frame, 함수는 마지막(가장 바깥쪽) frame.
- line 0(`file.c:?`, `??:0`): ① 앞 명령어가 이어서 실행되고 같은 inline 함수면 그 라인 ② 아니면 같은 흐름의 다음 명령어 라인
  ③ inline이면 호출 라인 ④ 미해결. 흐름 끊김: 무조건 `b`, `bx`, `pop {..pc}`, `ldm {..pc}`, `ldr/mov pc`, `tbb/tbh`.
- 데이터 영역 제외: GNU `.word`/ASCII dump, LLVM raw byte + `.word`.
- SimGDB 구현: DWARF는 `addr2line` crate(gimli), 명령어 목록은 ELF 코드를 ISA 분류기로 직접 읽어 만듭니다
  (`$t`/`$d` mapping symbol로 코드·데이터 구분, 다른 심볼에서 이웃 끊음, IT 블록 안의 분기는 조건부).
- DWARF에 함수가 없는 코드는 가장 가까운 앞쪽 함수 심볼 이름을 씁니다(GNU addr2line과 같음).
- DWARF 5 range list로 기술된 inline 호출도 frame으로 봅니다(GNU addr2line 2.42는 못 보고, llvm-addr2line은 봄).
- 한 주소에 line table 행이 여럿이면 마지막 행을 씁니다(addr2line과 같음). gdb는 마지막 `is_stmt` 행을 쓰므로
  함수 첫 명령어 등에서 gdb와 한 줄 다를 수 있습니다(예: gdb는 `{` 줄, SimGDB는 첫 문장 줄).
- 비교 기준과 결과: `tests/attribution/README.md`.

## 7. 저장소 구조

Cargo workspace입니다. crate 경계가 의존 방향을 강제합니다(앞단 → 엔진 → 저장소·ISA·ELF).

```
SimGDB/
├── Cargo.toml          # workspace
├── crates/
│   ├── simgdb-model/   # Point, SimTime, Value(source/evidence), 오류 타입
│   ├── simgdb-isa/     # arm_m, riscv: 레지스터, target XML, 명령어 분류
│   ├── simgdb-elf/     # 심볼, line table, 초기 메모리, 소스 귀속, 전역변수
│   ├── simgdb-tarmac/  # 공급자: Tarmac 파서 (riscv(3), waveform(2)은 별도 crate)
│   ├── simgdb-store/   # 열 파일 + SQLite, 인덱서, checkpoint, 파생 표
│   ├── simgdb-engine/  # 상태 복원, 질의, 조건식
│   ├── simgdb-gdb/     # RSP 서버 (gdbstub)
│   ├── simgdb-mcp/     # MCP 서버, skill 문서 (rmcp)
│   └── simgdb-cli/     # 실행 파일 `simgdb`: index, serve, mcp, query, track
├── tests/
│   ├── firmware/       # 테스트 펌웨어, QEMU trace 생성기, 정답 (생성물은 커밋)
│   ├── gdb/            # gdb 스크립트 테스트
│   └── ai_eval/        # AI 평가 세트
└── docs/
```

## 8. 검증

| 층 | 검증하는 것 | 방법 |
|---|---|---|
| 파서 fixture | 레코드 형식 해석 | 손으로 쓴 줄, tarmac-trace-utilities의 실제 수집 줄, 가능하면 가린 실제 Tarmac 일부 |
| QEMU 종단 간 | 복원·질의·RSP 정확도 | QEMU 플러그인으로 Tarmac 생성, 별도 QEMU 실행의 gdbstub으로 정답 기록. 정답 시점은 “PC X의 k번째 실행”. 같은 gdb 스크립트를 QEMU와 SimGDB에 돌려 비교. 생성물은 커밋해 테스트에 QEMU가 필요 없음 |
| 교차 확인 | 독립 구현끼리 일치 | tarmac-trace-utilities(테스트 전용), tarmac_to_cachegrind(소스 귀속) |
| 폐쇄망 실제 데이터 | 실제 형식과 규모 | 사용자가 명령과 기대 결과로 확인 |

QEMU trace는 우리가 만든 Tarmac 표기이므로 **실제 Tarmac 형식 해석은 검증하지 못합니다**. 그 부분은 fixture와 사용자 확인이 담당합니다.
