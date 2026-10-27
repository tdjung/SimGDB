# 테스트 펌웨어와 정답

상태 복원(레지스터·메모리)을 **SimGDB와 독립적인 정답**으로 검증하기 위한 데이터입니다.
생성물은 커밋되어 있어 `cargo test`에는 QEMU나 Arm 툴체인이 필요 없습니다.

| 파일 | 내용 |
|---|---|
| `src/state.c`, `src/startup.c`, `src/link_*.ld` | 테스트 펌웨어. `.data` 초기화, 바이트/하프워드/더블워드 저장, 재귀, 함수 포인터, SVC·SysTick 예외, 도중에 PSP로 전환(예외 frame이 두 스택에 쌓임) |
| `state_m4.elf`, `state_m35p.elf` | Cortex-M4(`mps2-an386`), Cortex-M35P 빌드(QEMU는 `mps2-an505` Cortex-M33으로 실행, 같은 Armv8-M Mainline) |
| `state_m4be.elf` | 같은 Cortex-M4 빌드의 big-endian(BE8) 판. 아래 "Big-endian" 참고 |
| `src/fp.c`, `state_m4f.elf` | 같은 Cortex-M4 빌드의 hard-float(FPv4-SP) 판. 아래 "부동소수점" 참고 |
| `qemu/qemu-m-bigend.patch` | QEMU 10.2.4에 M-profile big-endian 데이터를 넣는 시험용 패치(`QEMU_ARM_M_BIGEND=1`일 때만 동작) |
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

`m4f`(아래 "부동소수점")에서는 FP 레지스터를 방언마다 다른 형식으로 씁니다: `fm`은 D 레지스터 전체(`R D7 404000003f800000`),
`m4rtl`은 쓰지 않은 절반을 `-`로 둔 D 레지스터(`R D7 --------3f800000`, Cortex-M4 RTL trace와 같은 형식), `es`는 S 레지스터(`R S14 3f800000`).

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

## 부동소수점 (`m4f`)

`state.c`를 hard-float(`-mfloat-abi=hard -mfpu=fpv4-sp-d16`)로 빌드하고 `src/fp.c`를 더합니다. `startup.c`를 `-Dmain=fp_main`으로
컴파일하므로 reset handler가 먼저 `fp_main`을 부릅니다: FPU를 켜고(CPACR), SysTick이 도는 동안 float 계산(S 레지스터, 호출을 건너
살아 있는 값은 `vpush {d8-d9}`로 저장되는 S16–S31, float 저장, `VMSR`로 FPSCR 변경)을 한 뒤 원래 `main`을 부릅니다.
FP 명령어를 한 번 실행하면 thread 모드에서 CONTROL.FPCA가 유지되므로 이후의 모든 예외가 FP 문맥 frame(0x68바이트)을
MSP와, 전환 뒤에는 PSP에 쌓습니다. 핸들러는 FP를 쓰지 않으므로 lazy stacking이 frame의 FP 부분을 실제로 쓰지는 않습니다.

플러그인은 `fp=on` 인자로 gdbstub의 D0–D15와 FPSCR을 함께 기록하고(S0–S31로 나눠 적음), 정답 gdb는 `info registers`의 raw 값으로
S 레지스터와 FPSCR을 읽습니다. 앞의 세 펌웨어(`m4`, `m35p`, `m4be`)와 그 trace는 바뀌지 않습니다.

빌드 도구: `m4`, `m35p`, `m4f`는 Ubuntu의 arm-none-eabi-gcc 13.2.1, `m4be`는 xPack arm-none-eabi-gcc 14.2.1로 만들었습니다.
다른 버전으로 다시 빌드하면 ELF가 달라지므로 trace와 정답도 함께 다시 만듭니다.

## Big-endian (`m4be`)

QEMU의 M-profile은 little-endian 데이터만 지원하므로, `qemu/qemu-m-bigend.patch`를 적용한 QEMU로 BE8 펌웨어를 실행합니다.
패치는 `QEMU_ARM_M_BIGEND=1`일 때만 동작하고(없으면 원래 QEMU와 같음), 다음을 바꿉니다.

- 코어의 데이터 접근을 big-endian으로(CPSR.E와 같은 방식), 예외 stacking·unstacking·벡터 표 읽기도 같은 순서로.
- PPB(SysTick, NVIC 등) 접근은 아키텍처대로 little-endian.
- BE ELF를 읽고, semihosting 인자를 big-endian으로 읽음.
- gdbstub이 레지스터를 big-endian 바이트로 보냄(BE 코어에 붙은 디버그 프로브처럼). 정답을 읽는 gdb는 BE ELF를 읽으므로
  추가 설정이 없습니다.

`make_traces.py`는 ELF 헤더로 byte 순서를 정하고, Tarmac 방언도 그 순서로 적습니다(`MW4`는 숫자, `MNW4___D`와 `LD`/`ST`는
주소 순서 바이트). 정답 레지스터가 플러그인 기록과 모든 정답 시점에서 같은지 확인하는 것은 LE와 같습니다.

```bash
cd /path/to/qemu && git apply /path/to/simgdb/tests/firmware/qemu/qemu-m-bigend.patch && ninja -C build qemu-system-arm
```
