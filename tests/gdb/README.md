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

정답 다시 만들기(QEMU 10.1 이상 필요):

```bash
QEMU=/path/to/qemu-system-arm GDB=arm-none-eabi-gdb python3 tests/gdb/gdb_tests.py expected --core m4
```

## 알려진 차이 (시나리오에서 피함)

- **인터럽트가 끼어든 명령어의 breakpoint**: 실제 대상(QEMU)은 PC가 그 명령어에 도착한 순간 한 번 멈추고,
  인터럽트 처리 후 실제로 실행할 때 또 멈춥니다. Tarmac에는 실행만 기록되므로 SimGDB는 실행 때 한 번만 멈춥니다.
  시나리오의 breakpoint는 이 실행에서 그런 일이 없는 주소에 둡니다.
