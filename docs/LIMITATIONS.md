# 제약사항

SimGDB는 **끝난 시뮬레이션의 기록**으로 동작합니다. 기록에 없는 것은 보여줄 수 없고,
보여주는 값마다 출처를 표시합니다. 이 문서는 데이터 출처별로 무엇이 보이는지 정리합니다.
Step 0의 실제 Tarmac 조사 결과에 따라 갱신합니다.

## 출처별 가시성

| 대상 | Tarmac만 (Step 1) | + waveform (Step 2) |
|---|---|---|
| 실행 흐름 (PC, 순서, 예외) | ✅ 정확 | ✅ (Tarmac 기준, waveform은 cycle 타이밍 보충) |
| 범용 레지스터, xPSR | ✅ Tarmac이 모든 쓰기를 기록하면 | ✅ |
| FPU 레지스터, 뱅크된 SP, 시스템 레지스터 | ⚠️ Tarmac이 기록하는 경우만 | ✅ 신호를 매핑하면 |
| 스택, 지역변수 | ✅ 대부분 (core가 쓴 값은 Tarmac에 있음) | ✅ |
| 전역변수 (core만 씀) | ✅ ELF 초기값 + Tarmac 쓰기 | ✅ |
| 전역변수 (DMA 등 다른 master가 씀) | ❌ 틀릴 수 있음 → 해당 시점 이후 출처 `inferred`/`unknown` | ✅ bus 트랜잭션을 덤프했다면 |
| 주변장치 SFR | ⚠️ core가 읽은 순간의 값만 | ✅ IP의 레지스터 신호가 덤프 범위에 있고 매핑이 있으면 |
| core가 접근하지 않은 메모리 | ELF 초기값(있으면) 또는 `unknown` | bus/메모리 덤프에 따라 |
| cycle 단위 타이밍, handshake, IRQ 지연 | ❌ | ✅ |

## 동작 제약

- **상태를 바꾸는 명령은 지원하지 않습니다.** `set var`, 레지스터·메모리 쓰기, 함수 호출(`print f()`),
  `jump`, `return`. 실행된 기록을 재생할 뿐 다시 실행하지 않습니다.
- **기록 밖으로는 갈 수 없습니다.** trace 시작 전과 끝 이후는 없습니다. continue가 끝까지 가면 trace 끝에서 멈춥니다.
- **line 0 명령어의 라인은 추정입니다.** 규칙은 [ARCHITECTURE.md](ARCHITECTURE.md) §6. 미해결은 line 0으로 남습니다.
- **최적화된 코드**: 변수가 레지스터에서 사라진 구간(`<optimized out>`), inline·tail call로 인한 backtrace 차이는 일반 GDB와 같습니다.
- **reset이나 trace 중간 시작**: 기록 시작 시점의 레지스터·메모리는 Tarmac이 처음 쓰기 전까지 `unknown`(메모리는 ELF 초기값 가능).
- **멀티코어**: Step 3 전까지는 core별로 따로 봅니다.
- **시간**: trace에 기록된 값(`ps`/`ns` 또는 ES `tic` 사이클)을 그대로 보여 줍니다. 사이클 trace의 절대 시간은
  시간 변환 표가 있을 때만 계산하고, 없으면 `unknown`입니다(주파수 변경·clock gating 때문에 고정 배율을 쓰지 않음).
  waveform과 시간을 맞추려면(Step 2) 변환 표가 필요합니다.

## Step 1 구현의 규칙 (첫 마일스톤)

- **레지스터**: trace에 쓰기 기록이 나온 뒤부터 알려짐. 그 전에는 `unknown`. `x` 자리가 있는 값은 일부만 알려짐(GDB에는 unavailable).
- **SP와 MSP/PSP**: trace가 어느 쪽을 쓰든(`r13`, `r13 (MSP)`, `SP_main`, `MSP` …) 모드(IPSR)와 CONTROL.SPSEL로
  나머지를 맞춥니다. 모드를 모르면 맞추지 않습니다.
- **메모리**: 그 바이트에 대한 마지막 기록(읽기 포함)의 값. 기록이 없으면 ELF load image(플래시 등 load 주소 기준),
  없으면 나중에 쓰기 없이 읽힌 값(`inferred`), 그것도 없으면 `unknown`. `.data`의 RAM 주소는 startup 복사가 trace에 나온 뒤부터 알려짐.
- **예외 진입**: 코어가 스택에 frame을 쌓는 저장이 trace에 없으면, 진입 전 SP 아래와 진입 후 frame 위치의 108바이트를
  `unknown`으로 표시합니다(옛 값을 보여 주지 않음). 저장이 trace에 있으면 그 값을 씁니다.
- **주변장치 영역**(`0x40000000–0x5FFFFFFF`, `0xA0000000` 이상): 항상 `unknown`. 마지막 접근은 증거로만 보여 줍니다.
- **`IF` 레코드**: 바로 앞 명령어와 PC가 같으면 중복으로 보고 제외합니다(실제 의미는 Step 0에서 확인).

## GDB에서 보이는 동작

- **역방향 실행**: `reverse-step`, `reverse-next`, `reverse-stepi`, `reverse-continue`, `reverse-finish`가 동작합니다.
  trace 처음과 끝에서는 “No more reverse-execution history”로 멈춥니다.
- **시점 이동**: `monitor goto <index>`, `monitor goto time <t>`. gdb는 대상이 바뀐 것을 모르므로
  `tools/simgdb.gdb`의 `simgdb-goto`/`simgdb-goto-time`을 쓰면 캐시를 비우고 새 frame을 보여 줍니다.
- **인터럽트가 끼어든 명령어의 breakpoint**: 실제 하드웨어는 PC가 도착한 순간(인터럽트 처리 전)과 실제 실행 때 두 번
  멈출 수 있지만, Tarmac에는 실행만 기록되므로 SimGDB는 실행 때 한 번 멈춥니다.
- **PSP를 쓰는 펌웨어**: trace가 PSP를 한 번이라도 쓰면 gdb의 M-profile 예외 unwinder를 씁니다. 이 unwinder는 예외 frame마다
  PSP를 읽으므로, PSP가 처음 쓰이기 전 핸들러 안의 backtrace는 `Register … is not available`에서 끊깁니다(PSP는 reset 후 UNKNOWN).
  PSP를 전혀 쓰지 않는 trace에서는 이 문제가 없습니다.
- **쓰기 명령**: `set var`, 레지스터·메모리 쓰기는 거부됩니다(“Cannot access memory” 등).

## 값 표시 규칙

- GDB: 모르는 레지스터는 unavailable로 응답(`<unavailable>`), 모르는 메모리는 읽기 오류.
- MCP/CLI: 모든 값에 `source`(`trace` / `waveform` / `elf_initial` / `inferred` / `unknown`)와 근거 시점.
