# 배경

이 문서는 SimGDB를 만들기로 한 이유, 조사한 내용, 그리고 그 결과로 내린 결정을 정리합니다.
2026년 10월 논의 내용을 바탕으로 했습니다. 상용 도구에 대한 내용은 **공개 자료 기준**이며
실제 평가(Step 0)로 확인해야 합니다.

## 1. 출발점

### 1.1 현재 하고 있는 일

- SoC RTL 시뮬레이션에서 CPU 코어(Arm Cortex-M4, Cortex-M35P, 이후 RISC-V)가 펌웨어를 실행합니다.
- 시뮬레이션은 **실행 기록**을 남깁니다.
  - Arm: **Tarmac** 로그 (ES 형식 `tic`, IT/IS/IF 형식 `ps`). 명령어 실행, 레지스터 쓰기(`R`),
    메모리 접근(`LD`/`ST`, `MR`/`MW`, `BNR`/`MNR` 계열), 예외(`ES EXC`, `E`)가 들어 있습니다.
  - waveform: Synopsys(FSDB), Cadence(SHM). 두 회사 도구 모두 VCD로 변환할 수 있습니다.
- 실제 데이터는 **폐쇄망** 안에 있습니다. 개발은 외부에서 공개 펌웨어와 QEMU로 만든 trace로
  검증하고, 사용자가 폐쇄망에서 실제 데이터로 다시 확인합니다.

### 1.2 이미 만든 도구

| 도구 | 하는 일 | SimGDB와의 관계 |
|---|---|---|
| [tarmac_to_cachegrind](https://github.com/tdjung/tarmac_to_cachegrind) | Tarmac → Cachegrind 변환. 대량 일괄 변환, merge, coverage index | 파싱·분류·소스 귀속 규칙과 검증 방법을 참고. 코드는 독립, 소스 귀속 결과는 비교 테스트로 일치(D14) |
| [performance_profiler](https://github.com/tdjung/performance_profiler) | Cachegrind 프로파일·coverage 뷰어 (Electron + Next.js) | Step 3에서 분석 결과 연동 |

변환기에서 검증해 둔 것(그대로 가져갈 자산):

- **Tarmac 파싱**: ES/IT/IS/IF, CCFAIL, IF 중복, 예외 레코드, gzip/stdin.
- **명령어 분류 (Thumb/Thumb-2, v7E-M·v8-M)**: 조건 분기(`b<cc>`, `cbz/cbnz`, 조건부
  `bl/bx/blx/pop/ldm/ldr/mov pc`), IT 블록 처리, GNU/LLVM objdump 차이, 데이터 영역
  (`.word`, ASCII dump, LLVM raw byte) 제외.
- **소스 귀속**: `addr2line -a -f -i -C`로 라인은 가장 안쪽 inline frame, 함수는 가장 바깥쪽 frame.
- **line 0 처리**: DWARF line 0 명령어를 previous(같은 inline 함수, 흐름이 이어질 때) →
  next → inline 호출 라인 → 미해결 순서로 배치.
- **검증 방법**: QEMU(`mps2-an386` = Cortex-M4, `mps2-an505` = M33 기반으로 M35P 대용)로
  테스트 펌웨어를 실행해 trace를 만들고, **CPU 플래그로 계산한 정답**과 비교.

### 1.3 문제

- Tarmac은 실행의 거의 모든 것을 담고 있지만 **텍스트 로그**라서, "이 시점에 변수 값이 무엇이었나",
  "이 함수는 어디서 불렸나"를 사람이 손으로 찾아야 합니다.
- 소스 레벨 디버거처럼 **breakpoint, step, backtrace, 변수 보기**를 시뮬레이션 결과에 쓰고 싶습니다.
- HW 문제와 SW 문제를 가르려면 SW 위치와 **waveform 시간**을 오가야 합니다.
- 요즘 디버깅과 분석을 **AI**로 많이 하므로, AI가 쉽게 쓸 수 있는 인터페이스가 처음부터 필요합니다.

## 2. 조사

### 2.1 오픈소스: waveform 기반 GDB

| 항목 | GDBWave (tomverbeure) | dang / JPDB (1024bees) |
|---|---|---|
| 방식 | 시뮬레이션이 끝난 FST를 재생하며 GDB RSP에 응답 | 같음 |
| 언어 | C++ | Rust (+ Python 매핑 파일) |
| 입력 신호 | 설정 파일에 신호 이름: retire PC/valid, 레지스터 쓰기, 메모리 쓰기 | Python 함수로 매핑: retire PC, x0–x31 값 |
| 레지스터 복원 | 쓰기를 처음부터 누적 | 매 시점 값을 직접 읽음 |
| 메모리 | 초기 binary + 쓰기 누적, `m` 지원 | 언급 없음 |
| 프로그램 | binary (ELF 아님) | ELF |
| 시간 이동 | 처음부터 재시작만 | 언급 없음 |
| 뷰어 연동 | 없음 | Surfer (WCP), 전용 클라이언트·TUI |
| 한계 | RISC-V, single-issue in-order, step마다 처음부터 재생 | RV32G만, superscalar 미지원 |

결론:

- GDB 서버 쪽은 단순합니다. ISA에 묶인 부분은 레지스터 구성과 target description XML 정도입니다.
- 어려운 것은 **waveform에서 retire PC, 레지스터, 메모리 신호를 찾는 일**입니다.
  Arm Cortex-M RTL은 레지스터 파일이 내부 신호이고 이름이 평탄화되거나 난독화되어 있습니다.
  RISC-V는 RVFI 같은 표준 retire 인터페이스를 가진 코어가 많아 상대적으로 쉽습니다.
- Tarmac은 그 신호들을 **이미 텍스트로 뽑아 둔 것**입니다.

### 2.2 오픈소스: Arm tarmac-trace-utilities (Apache-2.0)

- tarmac-browser(임의 시점 레지스터·메모리, 마지막으로 쓴 지점으로 이동), tarmac → VCD 변환,
  호출 트리, 프로파일, flame graph.
- **인덱스**가 trace의 모든 시점 상태를 기록합니다. trace에 나오지 않은 메모리는 `??`.
- GDB 서버 기능은 소개 글에 없습니다. 인덱싱 방식은 참고하거나 재사용할 가치가 있습니다.
- 확인 결과(2026-10): C++14, 약 2만 2천 줄, `libtarmac` 라이브러리로 분리, 2026년 9월까지 활동.
  파서가 여러 실제 Tarmac 표기(Fast Models `IT`/`R`/`MR4`/`MW1`, ES와 `LD`/`ST` 16바이트 형식 등)를 처리하고,
  실제 수집한 trace 줄로 된 파서 테스트(`tests/parsertest.txt`)가 있습니다. 인덱스는 디스크 AVL 트리로
  한 시점 조회용이라 훑기 질의에는 맞지 않습니다.
- 활용: 파서 규칙과 테스트 줄을 참고(Apache-2.0, 출처 표기)하고, 테스트에서 **독립 구현끼리 비교하는
  교차 확인 도구**로 씁니다.

### 2.3 상용 도구 (라이선스 보유: Synopsys, Cadence)

| 도구 | 공개 자료로 확인된 것 | 확인 필요 |
|---|---|---|
| **Synopsys Verdi HW/SW Debug** | **입력이 Tarmac + ELF** (`hwsw_debug_convert cpuType=… exeFile=… -i tarmac.log -o hwsw.fsdb`). Eclipse 기반 SW 디버거: 소스·어셈블리, **breakpoint, single step, 선택 시점의 CPU 레지스터·메모리**. nWave에서 시간을 고르면 Eclipse가 동기화. SW 프로파일·라인 coverage | Cortex-M4/M35P용 `cpuType` 패턴(`$VERDI_HOME/share/hwsw_debug/patterns`), 역방향 step, call stack·지역변수, 메모리 출처(DMA 반영 여부), SFR 보기, 대용량 성능 |
| **Cadence Indago Embedded SW Debug** (현재 Verisium Debug ESW) | HW/SW trace 사후 분석, lock-step 동기화. 플랫폼 기능으로 역방향 디버깅, call stack, 변수, memory viewer | 입력 형식, 지원 코어, breakpoint, SFR |
| Siemens Codelink (참고, 미보유) | 기록 후 재생, 소스 뷰, 역방향 step, 변수 hover, call stack, 멀티코어, Arm 지원 | — |

결론:

- 상용 도구도 **"Tarmac 기반 SW 디버거 + waveform 커서 동기화"** 구조입니다.
  waveform에서 메모리나 IP 상태를 읽어 정확하게 보여주는 기능은 공개 자료에서 확인되지 않았습니다.
- **GDB/VS Code 같은 표준 인터페이스가 없고**, 라이선스 수량이 제한되어 SW 개발자 전원이
  쓰기 어렵고, 커스텀은 벤더 API(VC Apps/NPI 등) 범위로 제한됩니다.
- 그래도 이미 라이선스가 있으므로 **Step 0에서 직접 평가**해 역할을 나눕니다.

### 2.4 AI 디버깅 동향

| 분류 | 사례 | 교훈 |
|---|---|---|
| GDB를 MCP로 감싸기 | gdb-mcp, llmdb (GDB/MI → JSON) | 화면 텍스트가 아닌 **구조화된 결과** |
| 임베디드 디버거 | Lauterbach TRACE32 MCP(공식 초기 버전), embedded-debugger-mcp (probe-rs/OpenOCD) | 범용 LLM은 전용 명령을 자주 지어냄 → **검증된 skill 단위**로 노출 |
| 기록 재생 + AI | **Undo (UDB) MCP, `explain`** | 적고 원자적이고 독립적인 도구, **역방향 이동과 "마지막으로 바뀐 시점" 질의**, bookmark, **증거와 함께 답**, 기록 전체가 아니라 필요한 것만 가져가기 |
| 연구 | Microsoft debug-gym | 디버거 도구가 성공률을 높이지만 AI는 순차적 step 탐색에 약함 |

기존 GDB가 AI에게 어려운 이유: 세션 상태(현재 frame, breakpoint)를 추적해야 하고, 비동기
정지 이벤트, 사람용 텍스트 출력, step마다 왕복이 필요하며, 실제 타깃은 비결정적이고 상태를 바꿀 수 있습니다.

**우리 상황의 장점:** trace는 끝난 실행의 **완전하고 결정적인 기록**입니다. 읽기 전용이라 안전하고,
결과를 검증할 수 있고, step 대신 **질의**로 답할 수 있습니다.

## 3. 주요 결정

| # | 결정 | 이유 |
|---|---|---|
| D1 | **trace를 직접 읽는 자체 GDB 서버**를 만든다 (FST를 거치지 않음) | Tarmac에 필요한 정보가 이미 있음. 신호 합성 단계가 없어 단순하고 정확. 변환기 자산 재사용. Arm 직접 지원 |
| D2 | 기존 오픈소스는 **부품과 설계를 차용**한다 | GDB 서버 구조(GDBWave/dang), RSP 라이브러리, 인덱싱 방식(tarmac-trace-utilities), 뷰어 연동(WCP) |
| D3 | **질의 엔진 하나 + 앞단 여러 개** (GDB RSP, MCP, CLI) | 사람과 AI가 같은 답을 받음. 앞단 추가가 쉬움 |
| D4 | **인덱스형 저장소** (재생형 아님) | AI 질의는 범위 전체를 훑음(`find_executions`, `last_write`). 재생 방식으로는 느림 |
| D5 | 모든 값에 **출처**(tarmac / waveform / ELF 초기값 / 추정 / 모름) | "모름"을 명시해야 사용자와 AI가 잘못된 값을 믿지 않음 |
| D6 | **ISA 정의 계층** (Arm, RISC-V) | 레지스터, target XML, 명령어 분류, 툴체인을 코드에 박지 않음 |
| D7 | **시점 = (run, core, 명령어 번호, 시뮬레이션 시간)** | 여러 run, 멀티코어, cycle 단위 waveform을 나중에 붙일 수 있음 |
| D8 | waveform은 **보충 공급자**로 붙인다 | 실행 흐름·레지스터는 trace가 정확하고 가벼움. waveform은 trace에 없는 값(메모리, SFR, 다른 master)만 보충 |
| D9 | waveform 신호 매핑은 **설정 파일 + trace와 자동 비교로 검증** | 코어·릴리스·SoC마다 매핑이 달라짐. trace가 정답지 역할 |
| D10 | 상용 도구와 **역할 분담 가능성**을 열어둔다 | Verdi HW/SW Debug 평가 결과에 따라 Step 2 범위 결정 |

### 3.1 개발 시작 시 추가 결정 (2026-10-25)

PLAN의 미정 사항을 논의해 정한 내용입니다.

| # | 결정 | 이유 |
|---|---|---|
| D11 | **Rust**로 구현하고 엔진·GDB RSP·MCP·CLI를 **실행 파일 하나**로 배포한다 | Step 3까지 보면 인덱싱과 범위 훑기(여러 run 포함)가 엔진 대부분이라 네이티브 속도가 필요. 틀린 값을 조용히 내는 메모리 버그를 컴파일 단계에서 막음. 폐쇄망은 코드를 고치지 않고 **외부에서 빌드한 실행 파일을 반입**만 하므로 Python 버전·pip 제약이 사라짐. 부품이 성숙함(`gdbstub`, `gimli`, `arrow`, 공식 MCP SDK `rmcp`) |
| D12 | 저장소는 **열 기반 파일(Arrow IPC, mmap) + SQLite** | 큰 테이블(명령어, 레지스터 쓰기, 메모리 접근, 역색인, checkpoint)은 mmap 열 파일로 훑기와 index 직접 접근. 작고 수정되는 것(메타데이터, bookmark, 호출 로그, 작업 상태)은 SQLite. 표준 형식이라 Step 3에서 DuckDB 등을 저장 형식 변경 없이 붙일 수 있음. 1천만 명령어 비교에서 Arrow IPC는 SQLite보다 한 시점 질의 수십~수백 배, 훑기 수십 배 빠름([benchmarks/storage.md](benchmarks/storage.md)) |
| D13 | GDB RSP는 **`gdbstub` crate**로 구현 | 패킷·checksum·`qSupported`·역방향 실행 확장을 제공. 우리가 작성하는 부분(레지스터, 메모리, 실행, breakpoint)은 직접 구현과 같음 |
| D14 | tarmac_to_cachegrind와 **코드는 독립**, 소스 귀속 **결과는 같게** 유지 | 필요한 파서가 다르고 시간이 지나며 더 갈라짐. 대신 같은 ELF에서 PC → (파일, 라인, 함수)를 비교하는 테스트로 결과 일치를 보장(프로파일러 연동 시 혼란 방지) |
| D15 | **AI 사용 방식을 기준으로 설계**하고 개발자 사용 방식은 그 안에서 처리 | 개발자는 한 시점 주변(step), AI는 범위 전체 훑기·집계·여러 run. 훑기 우선 저장소, 인덱싱 때 만드는 파생 표, 엔진 자체 DWARF 해석, 성능 목표 두 개(ARCHITECTURE §2.11–§2.13) |
| D16 | 시간은 **기록된 값 그대로** 저장하고, 절대 시간은 **구간별 변환 표**로 계산. 표가 없으면 `unknown` | ES의 `tic`은 사이클이고 주파수 변경·clock gating이 있으면 고정 배율이 틀림. 값의 출처 규칙(D5)을 시간에도 적용 |
| D17 | breakpoint는 **실행된 명령어에서만** 멈춘다(인터럽트에 선점되어 실행되지 않은 "도착"에서는 멈추지 않음) | 시점 번호가 Tarmac의 실행 명령어 순번과 1:1로 맞아 대조·질의가 단순. 실제 보드보다 멈춤이 한 번 적을 수 있는 차이는 사용자 문서에 설명(2026-10-26 합의) |

## 4. 출처

- [GDBWave – Post-Simulation Waveform-Based RISC-V GDB Debugging Server](https://tomverbeure.github.io/2022/02/20/GDBWave-Post-Simulation-RISCV-SW-Debugging.html), [tomverbeure/gdbwave](https://github.com/tomverbeure/gdbwave)
- [1024bees/dang](https://github.com/1024bees/dang), [Show HN: JPDB](https://hn.svelte.dev/item/45431526)
- [Arm tarmac trace utilities (blog)](https://developer.arm.com/community/arm-community-blogs/b/tools-software-ides-blog/posts/tarmac-trace-utilities), [ARM-software/tarmac-trace-utilities](https://github.com/ARM-software/tarmac-trace-utilities)
- [Synopsys Verdi HW/SW Debug](https://www.synopsys.com/verification/debug/verdi-hw-sw-debug.html), [Verdi HW/SW co-debug 사용 정리](https://www.cnblogs.com/Alfred-HOO/articles/17433219.html), [RISC-Verdi](https://github.com/SchaffHub/RISC-Verdi)
- [Indago Debug Platform whitepaper](https://assets.markallengroup.com/article-images/83937/indago_debug_platform_wp.pdf), [Cadence Verisium Debug](https://www.cadence.com/en_US/home/tools/system-design-and-verification/ai-driven-verification/verisium-debug.html)
- [Two Methods For Debugging SW Workloads On Arm-Based SoCs](https://semiengineering.com/two-methods-for-debugging-sw-workloads-on-arm-based-socs/), [Debugging Embedded Software on Veloce](https://semiwiki.com/eda/301842-debugging-embedded-software-on-veloce/)
- [VaporView API discussion](https://github.com/Lramseyer/vaporview/discussions/16), [Surfer VS Code API issue](https://gitlab.com/surfer-project/surfer-vscode/-/issues/17)
- [Undo × MCP](https://undo.io/resources/time-travel-ai-code-assistant/), [Undo agentic debugging poster](https://undo.io/wp-content/uploads/2025/10/cppcon-explain-poster-2025-09-01-final.pdf), [Undo AI](https://docs.undo.io/UndoAI.html)
- [TRACE32 MCP](https://github.com/lauterbach-mirror/t32mcp), [llmdb](https://github.com/byteshiftlabs/llmdb), [gdb-mcp](https://github.com/richinseattle/gdb-mcp), [embedded-debugger-mcp](https://github.com/adancurusul/embedded-debugger-mcp)
- [Microsoft debug-gym](https://www.microsoft.com/en-us/research/blog/debug-gym-an-environment-for-ai-coding-tools-to-learn-how-to-debug-code-like-programmers)
- [GDB Reverse Execution](https://sourceware.org/gdb/current/onlinedocs/gdb.html/Reverse-Execution.html)
