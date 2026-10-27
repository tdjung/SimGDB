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
- **watchpoint**: trace에 기록된 읽기·쓰기에서만 멈춥니다. 기록되지 않은 접근(예외 stacking 저장이 없는 trace, 읽기를 기록하지
  않는 trace의 읽기, DMA·다른 코어의 쓰기)은 보드와 달리 멈추지 않습니다.
- **시간**: trace에 기록된 값(`ps`/`ns` 또는 ES `tic` 사이클)을 그대로 보여 줍니다. 사이클 trace의 절대 시간은
  시간 변환 표가 있을 때만 계산하고, 없으면 `unknown`입니다(주파수 변경·clock gating 때문에 고정 배율을 쓰지 않음).
  waveform과 시간을 맞추려면(Step 2) 변환 표가 필요합니다.

## Step 1 구현의 규칙 (첫 마일스톤)

- **레지스터**: trace에 쓰기 기록이 나온 뒤부터 알려짐. 그 전에는 `unknown`. `x` 자리가 있는 값은 일부만 알려짐(GDB에는 unavailable).
  `-` 자리는 그 기록이 쓰지 않은 부분으로, 이전 값을 유지합니다(`R D2 --------37800000`처럼 일부만 바뀐 기록).
- **FPU 레지스터**: `S0`–`S31`과 `FPSCR`를 모델링합니다. trace의 `R S4 …`, `R D2 <16자리>`(`D2` = `S5:S4`),
  `R D2 --------37800000`(`-` 쪽 절반은 쓰지 않음)을 모두 받아 S 레지스터 단위로 저장하므로, 절반만 기록돼도 그 S 레지스터는 알려집니다.
  질의·MCP·`monitor`에서는 `s0`…`s31`, `fpscr`로 봅니다.
  - gdb에는 표준 `org.gnu.gdb.arm.vfp`(D0–D15, FPSCR)로 보내고, gdb가 S0–S31을 거기서 만듭니다. 컴파일러가 `vpush {d8-d9}`를
    D 레지스터로 기록하므로 gdb가 frame을 풀고 호출자의 S16–S31을 보여 주려면 D 레지스터가 필요하기 때문입니다.
    **대가**: 두 절반 중 하나라도 모르는 D 레지스터는 gdb에서 `<unavailable>`이고, 그 안의 S 레지스터 둘도 그렇습니다
    (`simgdb query … registers`는 아는 쪽을 보여 줌).
  - **FP 문맥이 쌓인 예외 frame(EXC_RETURN bit 4 = 0)을 지나는 gdb backtrace는 그 frame에서 멈춥니다.** gdb가 이 frame을 풀 때
    메모리에 매핑된 `FPCCR`(0xE000EF34)을 읽는데, PPB 주변장치 값은 trace로 알 수 없기 때문입니다
    (`warning: Could not fetch required FPCCR content`). SimGDB 자체의 call stack(`monitor where`, MCP `call_tree`)은 영향이 없습니다.
  - `FPCCR`, `FPCAR`, `FPDSCR` 기록과 Q 레지스터, Non-secure 뱅크는 아직 모델링하지 않습니다(`unmodelled_registers`).
- **Armv8-M `MSPLIM`/`PSPLIM`**: 기록되면 레지스터로 보입니다(검사는 하지 않음). Armv7-M에서는 늘 `unknown`입니다.
- **SP와 MSP/PSP**: trace가 어느 쪽을 쓰든(`r13`, `r13 (MSP)`, `r13 (PSP) 값`, `SP_main`, `r13_mn`, `MSP` …) 모드(IPSR)와
  CONTROL.SPSEL로 나머지를 맞춥니다. 모드를 모르면 어느 쪽이 바뀌었는지 알 수 없으므로, 낡았을 수 있는 쪽을 `unknown`으로 둡니다.
  알려지지 않은 PSP로 전환하면 SP도 `unknown`입니다.
- **TrustZone(Armv8-M)**: Secure 뱅크(`MSP_S` 등)만 모델링합니다. Non-secure 뱅크(`MSP_NS` 등)는 합치지 않고
  보고서의 `unmodelled_registers`에 나옵니다. Non-secure 상태로 실행하는 구간의 SP/MSP/PSP 정확도는 아직 보장하지 않습니다.
- **메모리**: 그 바이트에 대한 마지막 기록(읽기 포함)의 값. 기록이 없으면 ELF load image(플래시 등 load 주소 기준),
  없으면 나중에 쓰기 없이 읽힌 값(`inferred`), 그것도 없으면 `unknown`. `.data`의 RAM 주소는 startup 복사가 trace에 나온 뒤부터 알려짐.
- **예외 진입**: 코어가 스택에 frame을 쌓는 저장이 trace에 없으면 frame 영역을 `unknown`으로 표시합니다(옛 값을 보여 주지 않음).
  진입 전 SP 아래 216바이트(가장 큰 frame, 살아 있는 데이터 없음)와, 진입 후 EXC_RETURN이 가리키는 스택의 frame 크기만큼입니다
  (기본 32바이트, FP 문맥이면 0x68바이트, Armv8-M의 Secure FP 문맥은 trace로 알 수 없어 있다고 가정). 진입 전 SP를 알면 frame은
  거기서 끝나므로 그 위(중단된 코드의 스택)는 건드리지 않습니다. 저장이 trace에 있으면 그 값을 씁니다. SP나 EXC_RETURN을 몰라
  frame 위치를 정할 수 없으면 보고서의 `exception_frames_unplaced`에 셉니다.
- **멀티코어 trace**: 한 번에 한 코어를 인덱싱합니다. 다른 코어가 쓴 메모리 바이트는 그 시점부터 `unknown`입니다.
- **주소 범위**: ISA 주소 범위(M-profile 32비트)를 벗어난 접근은 해석 오류로 보고하고 쓰지 않습니다.
- **주변장치 영역**(`0x40000000–0x5FFFFFFF`, `0xA0000000` 이상): 항상 `unknown`. 마지막 접근은 증거로만 보여 줍니다.
- **call stack(`locate`)**: 스택 메모리가 아니라 trace 흐름으로 만듭니다. `bl`/`blx`가 실행되면 frame을 열고 그 복귀 주소에
  도착하면 닫습니다(longjmp처럼 여러 단계를 건너뛰어도 닫힘). 예외 기록은 예외 frame을 열고, 핸들러의 복귀 명령어(`bx lr`, `pop {..pc}`)
  다음 명령어나 tail-chain에서 닫습니다. 그래서 스택 내용을 몰라도 동작하지만, trace 시작 전에 들어간 호출은 보이지 않고
  (가장 바깥은 `top`), 복귀 주소로 돌아가지 않는 호출(`bl` 후 RTOS 문맥 전환 등)은 frame이 남습니다.
  예외가 끊은 PC는 코어가 쌓은 복귀 주소(trace에 있을 때)를, 없으면 예외가 돌아간 PC를 씁니다. 각 frame의 `pc_from`이 출처입니다.
  `returned`(돌아간 PC)는 RTOS 문맥 전환 뒤에는 다음 task의 PC이므로 끊긴 PC와 다를 수 있습니다.
- **SW 트랙의 `exception`**: 예외 기록에 번호가 없으면 `0xffffffff`(번호 모름)입니다.
- **tail-chaining**: 핸들러가 복귀하자마자 다른 예외로 이어질 때 trace에 예외 기록이 없으면, 복귀 직후에도
  핸들러 모드이고 예외 번호(IPSR)가 돌아갈 곳과 다른 것으로 알아내 예외 frame을 엽니다(보고서 `exceptions_inferred`).
  xPSR이 trace에 없으면 알아낼 수 없어 그 핸들러는 끊긴 코드의 일부로 보입니다.
- **변수**: 고정 주소 변수(전역, 파일 static, 함수 static)만 엔진이 읽습니다. 지역변수, 포인터 따라가기, bit-field는
  gdb에서 봅니다.
- **Cortex-M RTL 버스 사본(`BNW1___S`, `BSR4___D` …)**: 코어 레코드(`M…___D`)와 같은 접근을 버스 쪽에서 다시 적은 것으로 보고
  상태에는 쓰지 않습니다(사본은 한 명령어 뒤에 나오기도 함). 대신 인덱싱 때 코어 레코드와 하나씩 맞춰 보고서 `bus_copies`에
  짝 없는 사본 수를 남깁니다. 명령어 쪽(`…I`) 레코드는 무시합니다.
- **바이트 순서(endian)**: little과 big(Arm BE8) 모두 지원합니다. 기본은 ELF 헤더를 따르고, `--endian` 또는 설정
  `[memory] endian`으로 바꿉니다(ELF와 다르면 보고서 `endian_conflict`). Arm의 PPB(`0xE0000000`–`0xE00FFFFF`, SysTick·NVIC 등)는
  BE8 코어에서도 little-endian으로 다룹니다. Arm BE32(구형, M-profile 아님)는 지원하지 않습니다.
  - Trace가 메모리 데이터를 **숫자로** 적는 형식(`MR4`/`MW4` …)은 byte 순서에 따라 해석이 달라집니다.
    **주소 순서로** 적는 형식(Cortex-M RTL `MNR4___D`, `LD`/`ST` 그림)은 byte 순서와 관계없습니다.
  - BE 검증: 같은 시험 펌웨어를 BE8로 빌드해, M-profile big-endian을 넣은 QEMU(`tests/firmware/qemu/qemu-m-bigend.patch`)로
    실행한 trace와 gdbstub 정답으로 시험합니다. 실제 BE Cortex-M 하드웨어 trace로는 아직 확인하지 않았습니다.
- **조건 검색**: trace가 보여 주지 않는 값이 들어간 비교는 "모름"이며 참으로 치지 않습니다. 결과의 `first_undecided`로
  판단하지 못한 시점을 알려 줍니다.
- **소스 줄 진입(`executions file:line`)**: 같은 줄이 line table에서 다른 줄과 번갈아 나오면(최적화 코드) 진입이 여러 번으로 셉니다.
- **RISC-V**: ISA 정의 계층(RV32 레지스터 x0–x31·pc, ABI 이름, GDB target description, jal/jalr/분기·압축 명령어 분류)만
  있습니다. RISC-V trace를 읽는 공급자와 CSR(mcause, mepc 등), trap 처리는 Step 3입니다.
- **인덱스 크기 한도**: 메모리 기록 행이 2^32개를 넘는 trace는 아직 지원하지 않습니다(오류로 알림).
- **`IF` 레코드**: 바로 앞 명령어와 PC가 같으면 중복으로 보고 제외합니다(실제 의미는 Step 0에서 확인).

## GDB에서 보이는 동작

- **역방향 실행**: `reverse-step`, `reverse-next`, `reverse-stepi`, `reverse-continue`, `reverse-finish`가 동작합니다.
  trace 처음과 끝에서는 “No more reverse-execution history”로 멈춥니다.
- **시점 이동**: `monitor goto <index>`, `monitor goto time <t>`. gdb는 대상이 바뀐 것을 모르므로
  `tools/simgdb.gdb`의 `simgdb-goto`/`simgdb-goto-time`을 쓰면 캐시를 비우고 새 frame을 보여 줍니다.
- **인터럽트가 끼어든 명령어의 breakpoint**: 실제 하드웨어는 PC가 도착한 순간(인터럽트 처리 전)과 실제 실행 때 두 번
  멈출 수 있지만, Tarmac에는 실행만 기록되므로 SimGDB는 실행 때 한 번 멈춥니다.
- **PSP를 쓰는 펌웨어**: trace가 PSP를 한 번이라도 쓰면 gdb의 M-profile unwinder를 씁니다. 이 unwinder는 frame을 만들 때마다
  MSP와 PSP를 읽으므로, PSP가 처음 쓰이기 전 구간(PSP는 reset 후 UNKNOWN)에서는 backtrace와 `-stack-list-frames`가
  `Register 92 is not available`로 실패합니다. 시험 trace에서는 PSP를 쓰기 전 4,748개 시점 중 1,524개였습니다. gdb 명령줄에서는
  backtrace가 끊기는 정도지만, VS Code(cppdbg)는 이 실패를 "Failed to find thread 1 for break event"로 보고하고 세션을 끝냅니다.
  `simgdb serve --no-psp-unwind`로 띄우면 gdb가 MSP/PSP에 의존하지 않아 이 실패가 없어지는 대신, PSP 위의 thread를 끊고 들어온
  핸들러에서는 예외 frame 아래 backtrace가 틀립니다(gdb가 MSP 쪽에서 예외 frame을 읽음). 그 구간의 호출 관계는 `monitor where`가
  맞게 보여 줍니다. PSP를 전혀 쓰지 않는 trace에서는 두 문제 모두 없습니다.
- **쓰기 명령**: `set var`, 레지스터·메모리 쓰기는 거부됩니다(“Cannot access memory” 등).

## 값 표시 규칙

- GDB: 모르는 레지스터는 unavailable로 응답(`<unavailable>`), 모르는 메모리는 읽기 오류.
- MCP/CLI: 모든 값에 `source`(`trace` / `waveform` / `elf_initial` / `inferred` / `unknown`)와 근거 시점.
