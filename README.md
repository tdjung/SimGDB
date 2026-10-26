# SimGDB

**Post-simulation debugger for Arm and RISC-V cores.**
시뮬레이션이 끝난 뒤 남은 실행 기록(Tarmac 등 trace 로그, waveform)으로 펌웨어를
GDB, VS Code, AI 에이전트(MCP)에서 디버깅합니다.

> 상태: **Step 1 구현 완료, 폐쇄망 실제 데이터 확인 대기** (Rust). 진행 상태는 [docs/PLAN.md](docs/PLAN.md)의 Step 1 "진행 상태"에 있습니다.

## 무엇을 하나

- 시뮬레이션 trace를 인덱싱해 **임의 시점의 PC, 소스 라인, 레지스터, 메모리**를 복원합니다.
- **GDB 원격 프로토콜(RSP) 서버**로 제공해 VS Code나 gdb에서 breakpoint, step,
  backtrace, 변수 보기, 시간 이동(역실행)을 합니다.
- **MCP 서버**로 같은 엔진을 AI 에이전트에 질의형 도구로 제공합니다
  ("이 변수를 마지막으로 쓴 시점", "이 라인이 실행된 모든 시점" 등).
- 이후 단계에서 **waveform**(Synopsys/Cadence 덤프 → VCD 등)으로 trace에 없는 값
  (DMA가 쓴 메모리, IP의 SFR)을 보충하고, waveform 뷰어와 시간을 동기화하며,
  **여러 run(회귀 테스트)과 멀티코어**를 가로지르는 분석을 합니다.

## 빠른 시작

```bash
# 인덱스 만들기 (Tarmac은 gzip이어도 됨). --report는 레코드 종류, 해석 못 한 줄, 가능한 기능을 JSON으로 출력
simgdb index trace.log --elf firmware.elf -o trace.idx --report

# 질의 (JSON)
simgdb info trace.idx
simgdb query trace.idx registers --at 1000
simgdb query trace.idx locate --at 1000      # PC, 소스 줄, call stack
simgdb query trace.idx executions state.c:111 # 그 줄이 실행된 횟수와 시점
simgdb query trace.idx find 'var("ticks") == 5'
simgdb query trace.idx memory --addr 0x20000000 --len 64 --time 13350000

# GDB 서버
simgdb serve trace.idx --listen 127.0.0.1:3333
arm-none-eabi-gdb firmware.elf -ex "target remote :3333" -ex "source tools/simgdb.gdb"
```

질의 전체와 gdb 사용 시 실제 보드와 다른 점은 [docs/USAGE.md](docs/USAGE.md)에 있습니다.

## 문서

| 문서 | 내용 |
|---|---|
| [docs/USAGE.md](docs/USAGE.md) | 사용법: 인덱스, gdb/VS Code(실제 보드와 다른 점), 질의와 조건식 |
| [docs/BACKGROUND.md](docs/BACKGROUND.md) | 왜 만드는가, 기존 도구·상용 도구·AI 디버깅 동향 조사, 주요 결정과 이유 |
| [docs/PLAN.md](docs/PLAN.md) | Step 0~3 전체 계획, 단계별 범위·산출물·완료 기준·위험 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 구성요소, 데이터 모델, 인터페이스, 처음부터 고정할 설계 결정 |
| [docs/LIMITATIONS.md](docs/LIMITATIONS.md) | 데이터 출처별로 보이는 것과 보이지 않는 것 (제약사항) |
| [docs/BUILD.md](docs/BUILD.md) | 빌드 방법 (Rust 설치, 빌드, 폐쇄망 패키지) |
| [docs/SESSION_PROMPT.md](docs/SESSION_PROMPT.md) | 개발 세션 시작용 프롬프트 |
| [CLAUDE.md](CLAUDE.md) | 이 repo에서 작업하는 에이전트를 위한 작업 규칙 |

## 관련 프로젝트

- [tarmac_to_cachegrind](https://github.com/tdjung/tarmac_to_cachegrind): Tarmac → Cachegrind 변환기.
  Tarmac 파싱, 분기 분류, inline/line 0 소스 귀속, branch coverage가 검증되어 있어
  SimGDB의 Tarmac 공급자가 재사용합니다.
- [performance_profiler](https://github.com/tdjung/performance_profiler): Electron + Next.js
  프로파일·coverage 뷰어. Step 3에서 분석 결과와 연동합니다.
