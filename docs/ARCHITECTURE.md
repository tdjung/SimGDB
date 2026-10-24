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
  코드 패키지 이름은 개발 세션에서 정합니다.

## 2. 처음부터 고정할 설계 결정

1. **시점(Point)**: `(run_id, core_id, index, sim_time)`.
   - `index`는 그 core에서 retire된 명령어 순번, `sim_time`은 시뮬레이션 시간(단위 포함).
   - Step 1은 run 하나, core 하나여도 모든 API가 이 형식을 씁니다.
   - ES 형식(`tic`)처럼 시뮬레이션 시간이 없거나 단위가 다르면 변환 규칙을 run 메타데이터에 둡니다.
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

## 3. 데이터 모델 (run별)

| 테이블 | 열 (예) | 비고 |
|---|---|---|
| `instructions` | index, sim_time, pc, encoding, flags(ccfail, is, exception 진입 등) | core별 |
| `reg_writes` | index, reg, value | Tarmac `R` 레코드 |
| `mem_accesses` | index, kind(read/write), addr, size, value, master | Tarmac `LD`/`ST`/`MR`/`MW`…, Step 2는 bus 트랜잭션(master 포함) |
| `exceptions` | index, kind(entry/return), number | |
| `checkpoints` | index, 레지스터 전체, 메모리 변경분 | 간격 설정 가능 |
| 역색인 | pc → indexes, addr → write indexes | |
| `signals` (Step 2) | sim_time, signal_id, value | 필요한 신호만, SFR·bus 등 |
| `sources` (공유) | pc → file, line, function, inline frame | ELF 기준, line 0 규칙 적용 |
| 메타데이터 | ISA, core, ELF 경로·해시, 시간 단위, 공급자 버전, capability | |

ELF와 소스 귀속은 run 사이에 공유합니다(같은 ELF면 한 번만).

## 4. 엔진 API (개념)

```
dataset.info() -> capabilities, runs, cores, limitations
locate(point) -> pc, function, file:line, call stack (evidence)
state.registers(point) -> {reg: Value}
state.memory(addr, size, point) -> [Value per byte or word]
query.executions(target: line|function|pc, range, limit, cursor) -> [point]
query.last_write(addr|symbol, point) -> point + Value
query.writes_to(addr range, time range) -> [point + Value]
query.find_first(condition, range, direction) -> point
query.exceptions(range) -> [...]
query.call_tree(range, depth) -> tree
diff(point_a, point_b) -> changed registers / memory
annotations.add/list(point, text)
(Step 2) hw.sfr(name, point), hw.sfr_history(name, range), hw.bus(range, filter), hw.signal_changes(path, range)
(Step 3) runs.first_divergence(run_a, run_b), runs.where_executed(target, runs)
```

모든 시점 인자는 `index` 또는 `sim_time`으로 줄 수 있고, 엔진이 `Point`로 바꿉니다.

## 5. 앞단

### GDB RSP 서버

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

설계 원칙: 도구는 **적고 원자적이고 상태가 없음**(모든 도구가 시점을 인자로 받음), **구조화된 JSON**,
**양 제한 + 요약 우선**, 모든 값에 **출처와 증거**, `unknown`을 명시, **skill 문서** 제공.

| 도구 | 단계 |
|---|---|
| `trace_info` (기능·제약 포함) | 1 |
| `locate` | 1 |
| `find_executions` | 1 |
| `call_tree` | 1 |
| `registers`, `read_memory` | 1 |
| `read_variable` (C 식; 내부에서 gdb를 SimGDB RSP에 붙여 DWARF 해석) | 1 |
| `last_write`, `writes_to` | 1 |
| `exceptions` | 1 |
| `find_first` (조건식) | 1 |
| `diff_state` | 1 |
| `bookmark_add`, `bookmark_list` | 1 |
| `job_status`, `job_result` (비동기) | 2 (구조는 1) |
| `sfr`, `sfr_history`, `bus_transactions`, `signal_changes` | 2 |
| `first_divergence`, `where_executed` (여러 run) | 3 |
| `explain` (서버 측 분석 에이전트) | 3 |

### CLI / Python API

`index`(인덱스 생성), `serve`(GDB), `mcp`, `query`(엔진 질의를 JSON으로), `track`(SW 트랙 waveform).

### 뷰어 어댑터 (Step 2)

공통 인터페이스: `goto(time)`, `add_signals(paths)`, `on_time_selected(callback)`.
구현: Verdi(Tcl/NPI), SimVision(Tcl), Surfer(WCP), VaporView(VS Code 명령; API 공개 상태 확인 필요).

## 6. 소스 귀속 규칙 (변환기와 동일하게 유지)

- `addr2line -a -f -i -C`: 라인은 첫(가장 안쪽) frame, 함수는 마지막(가장 바깥쪽) frame.
- line 0(`file.c:?`, `??:0`): ① 앞 명령어가 이어서 실행되고 같은 inline 함수면 그 라인 ② 아니면 같은 흐름의 다음 명령어 라인
  ③ inline이면 호출 라인 ④ 미해결. 흐름 끊김: 무조건 `b`, `bx`, `pop {..pc}`, `ldm {..pc}`, `ldr/mov pc`, `tbb/tbh`.
- 데이터 영역 제외: GNU `.word`/ASCII dump, LLVM raw byte + `.word`.

## 7. 저장소 구조 (제안)

```
SimGDB/
├── simgdb/
│   ├── core/        # Point, Value, 조건식, 엔진, 질의
│   ├── store/       # 인덱스 저장소, checkpoint
│   ├── sources/     # 공급자: tarmac (1), waveform (2), riscv (3)
│   ├── isa/         # arm_m, riscv: 레지스터, target XML, 명령어 분류
│   ├── elf/         # 심볼, line table, 초기 메모리, 소스 귀속
│   ├── gdb/         # RSP 서버
│   ├── mcp/         # MCP 서버, skill 문서
│   ├── viewers/     # 뷰어 어댑터 (2)
│   └── cli/
├── tests/
│   ├── firmware/    # 테스트 펌웨어, QEMU trace, 정답
│   └── ai_eval/     # AI 평가 세트
└── docs/
```
