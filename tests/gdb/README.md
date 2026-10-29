# GDB 서버 스크립트 테스트

실제 `arm-none-eabi-gdb`로 SimGDB의 GDB 서버를 검증합니다. **같은 gdb 명령**을 두 대상에 실행해 출력을 비교합니다.

| 대상 | 내용 |
|---|---|
| 정답 (`expected/<core>/*.txt`, 커밋됨) | QEMU gdbstub. `tests/firmware/traces`를 만든 실행을 record/replay로 재생 |
| 검사 대상 | `simgdb serve`. 같은 실행의 Tarmac trace(세 방언)로 만든 인덱스 |

## 시나리오 (`scenarios/*.gdb`)

| 이름 | 확인하는 것 |
|---|---|
| `steps` | breakpoint, `next`, `step`, `finish`, `stepi`, 변수·배열·구조체·문자열 출력, `x` |
| `exceptions` | SysTick·SVC 핸들러 안에서 정지, 예외 frame을 거치는 backtrace |
| `psp` | thread 모드가 PSP로 바뀐 뒤 SP/MSP/PSP, PSP에 쌓인 예외 frame을 거치는 backtrace |
| `reverse` | `reverse-stepi`, `reverse-continue`. 앞으로 실행한 정답의 같은 지점 출력과 비교 |
| `watch` | `watch`(Z2): 멈춘 위치와 이전/새 값, `reverse-continue`로 쓰기 직전까지 되돌아간 뒤 `stepi`로 정답 상태에 도착 |
| `watch_kinds` | `rwatch`(Z3), `awatch`(Z4), 예외 핸들러가 쓰는 배열 원소의 `watch` |
| `fp` (`m4f`만) | S/D 레지스터와 FPSCR, float 인자·반환값(`finish`), 호출자 frame의 S16–S31(`vpush`의 CFI로 복원), FP 문맥 예외 frame을 지나는 backtrace |

시나리오 첫머리의 `@cores m4 m35p m4be`는 그 코어에만 실행한다는 뜻입니다. 앞의 네 시나리오는 `m4` 실행에 맞춘 breakpoint
위치(인터럽트가 끼어들지 않는 곳)를 쓰므로 `m4f`에는 `fp`만 실행합니다.

`@dump`는 레지스터, backtrace, 인자, 지역변수를 출력합니다. `@reverse` 명령은 SimGDB에서만 실행합니다
(QEMU는 역방향 실행을 지원하지 않음).

## 비교 규칙

- 출력이 같아야 합니다.
- 예외: SimGDB가 **모른다고 답한 곳**만 다를 수 있습니다(`<unavailable>`, `Cannot access memory`, `is not available`,
  알 수 없는 메모리에서 멈춘 backtrace). **값이 다르면 실패**입니다.
- 예외 stacking 저장이 trace에 없는 방언(`fm`, `es`)에서만, 핸들러 아래 frame에서 backtrace가 끝나는 것을 허용합니다.
  저장이 있는 `m4rtl`은 backtrace 전체가 같아야 합니다.
- `x=x@entry=V`와 `x=V`는 같은 값으로 봅니다(entry 값 증명에 필요한 레지스터를 모를 때 gdb가 짧게 출력).

## 실행

```bash
cargo build --release -p simgdb-cli
GDB=arm-none-eabi-gdb python3 tests/gdb/gdb_tests.py check --core m4 --dialect m4rtl --simgdb target/release/simgdb
```

`--core`는 `m4`, `m35p`, `m4be`(같은 m4 펌웨어의 big-endian/BE8 빌드), `m4f`(hard-float 빌드)입니다.

정답 다시 만들기(QEMU 10.1 이상 필요, `m4be`는 `tests/firmware/qemu/qemu-m-bigend.patch`를 적용한 QEMU):

```bash
QEMU=/path/to/qemu-system-arm GDB=arm-none-eabi-gdb python3 tests/gdb/gdb_tests.py expected --core m4
```

## 알려진 차이

- **FP 문맥 예외 frame을 지나는 backtrace**: gdb는 이 frame을 풀 때 `FPCCR`·`FPCAR`(메모리에 매핑된 레지스터)을 읽습니다.
  SimGDB는 이를 추론해 답하므로 `fp` 시나리오의 backtrace가 QEMU와 같습니다(시험 trace는 FPCCR을 기록하지 않음). 추론이 안 되는
  구간에서 gdb가 내는 `warning: Could not fetch required FPCCR content`는 모르는 값으로 보고 허용합니다(`docs/LIMITATIONS.md`).

## 알려진 차이 (시나리오에서 피함)

- **인터럽트가 끼어든 명령어의 breakpoint**: 실제 대상(QEMU)은 PC가 그 명령어에 도착한 순간 한 번 멈추고,
  인터럽트 처리 후 실제로 실행할 때 또 멈춥니다. Tarmac에는 실행만 기록되므로 SimGDB는 실행 때 한 번만 멈춥니다.
  시나리오의 breakpoint는 이 실행에서 그런 일이 없는 주소에 둡니다.

## IDE 방식 세션 (`ide_session.py`)

VS Code 디버그 확장(cortex-debug, cppdbg)이 gdb에 보내는 것과 같은 MI 명령 순서로 SimGDB에 붙어 봅니다:
extended-remote 연결, `monitor halt`, 함수·줄 breakpoint, continue/next/step/finish, 역방향 next, 스택, 변수 객체와 자식,
레지스터, 메모리, `monitor where`/`hits`/`find`/`reset`, 쓰기 거부. 값은 같은 인덱스의 `simgdb query`와 대조합니다.

```bash
python3 tests/gdb/ide_session.py --simgdb target/release/simgdb --gdb arm-none-eabi-gdb --core m4 --dialect m4rtl
```

2 gdb × 2 코어 × 3 방언 모두 통과합니다. 예외 frame 저장이 trace에 없는 방언(fm, es)에서는 핸들러의 스택이
`<signal handler called>`에서 멈추는 것이 정상입니다(쌓인 frame이 "모름").

## 모든 시점의 stack 조회 (`stack_scan.py`)

IDE가 멈출 때마다 보내는 `-stack-list-frames`를 trace의 모든 시점에서 실행합니다. 기본 모드는 PSP를 모르는 시점에서만,
`--no-psp-unwind`는 어디서도 실패하면 안 됩니다.

```bash
python3 tests/gdb/stack_scan.py --simgdb target/release/simgdb --gdb arm-none-eabi-gdb --core m4 --dialect m4rtl
```

결과(m4, m4rtl): 기본 4,748개 시점 중 1,524개 실패(모두 PSP를 처음 쓰기 전), `--no-psp-unwind` 0개.
