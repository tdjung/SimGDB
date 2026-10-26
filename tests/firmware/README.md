# 테스트 펌웨어와 정답

상태 복원(레지스터·메모리)을 **SimGDB와 독립적인 정답**으로 검증하기 위한 데이터입니다.
생성물은 커밋되어 있어 `cargo test`에는 QEMU나 Arm 툴체인이 필요 없습니다.

| 파일 | 내용 |
|---|---|
| `src/state.c`, `src/startup.c`, `src/link_*.ld` | 테스트 펌웨어. `.data` 초기화, 바이트/하프워드/더블워드 저장, 재귀, 함수 포인터, SVC·SysTick 예외, 도중에 PSP로 전환(예외 frame이 두 스택에 쌓임) |
| `state_m4.elf`, `state_m35p.elf` | Cortex-M4(`mps2-an386`), Cortex-M35P 빌드(QEMU는 `mps2-an505` Cortex-M33으로 실행, 같은 Armv8-M Mainline) |
| `qemu/trace_plugin.c` | QEMU TCG 플러그인: 명령어마다 레지스터 상태, 메모리 접근 값, 예외 진입과 쌓인 frame을 기록 |
| `qemu/make_traces.py` | 플러그인 로그를 Tarmac 세 방언으로 만들고, gdbstub으로 정답을 기록 |
| `traces/state_*.{fm,m4rtl,es}.log.gz` | Tarmac trace (방언은 아래) |
| `traces/state_*.truth.json.gz` | 정답: 시점별 레지스터, RAM(.data/.bss/스택), backtrace |
| `traces/state_*.canonical.log.gz` | QEMU 플러그인의 원본 기록(명령어마다 레지스터 전체, 메모리 접근 값, 예외 frame). Tarmac으로 바꾸기 전의 실행 자체이며 질의 정답(`tests/queries`)의 입력 |

## Tarmac 방언

실제 trace 형식은 Step 0에서 확인 중이므로, 알려진 형식 세 가지로 만듭니다.

| 이름 | 형식 | 시간 | 특징 |
|---|---|---|---|
| `fm` | Fast Models 계열 `IT`/`IS`, `R r13`, `MR4`/`MW4`, `E ... CoreEvent_IRQ` | ps, 2000번째 명령어부터 주기 절반(주파수 변경) | 예외 stacking 저장 기록 **없음** |
| `m4rtl` | Cortex-M4 RTL 계열 `IT (pc:seq)`, `MNW4___D`(바이트 메모리 순서), `R r13 v (MSP)`, `R psr`, 가끔 `IF` 중복 | cyc | 예외 stacking 저장 기록 **있음** |
| `es` | `ES (pc:opcode)`, `CCFAIL`, `LD`/`ST` 16바이트 그림, `ES EXC [n]`, 대문자 레지스터 이름 | tic | 예외 stacking 저장 기록 **없음** |

## 정답이 독립적인 이유

1. QEMU를 **기록(record) 모드**로 한 번 실행하며 플러그인으로 trace를 만듭니다.
2. 같은 기록을 **재생(replay)** 하면서 QEMU gdbstub에 gdb를 붙여, 각 정답 시점(“PC X의 k번째 도달”)에서
   레지스터·RAM·backtrace를 gdb가 직접 읽습니다. SimGDB 출력은 쓰지 않습니다.
3. 재생을 쓰는 이유: 일반 실행을 다시 하면 gdb가 breakpoint를 넘어갈 때 타이머를 한 명령어 멈춰 인터럽트 시점이
   어긋납니다. 재생은 인터럽트를 기록된 명령어 위치에서 다시 일으키므로 두 실행이 같습니다.
4. 생성기는 모든 정답 시점에서 gdb가 읽은 레지스터가 플러그인 기록과 같은지 확인하고, 다르면 실패합니다.

검증 테스트: `cargo test -p simgdb-engine --test qemu_truth -- --nocapture`.
SimGDB가 **안다고 답한 값은 모두 gdb 값과 같아야** 하며, 모르는 값(`unknown`)은 개수만 셉니다.

## 다시 만들기

```bash
QEMU=/path/to/qemu-system-arm QEMU_SRC=/path/to/qemu ./build.sh
```

필요한 것: arm-none-eabi-gcc/objdump, QEMU 10.1 이상(`--enable-plugins`), Python과 Arm을 지원하는 gdb
(`gdb-multiarch` 또는 `GDB=arm-none-eabi-gdb`), glib 헤더. 개발 환경에서는 QEMU 10.2.4를 소스에서 빌드해 썼습니다.
