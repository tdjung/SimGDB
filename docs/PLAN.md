# 계획

전체 범위(Step 3까지)를 기준으로 세운 계획입니다. 각 단계는 앞 단계의 구조를 **다시 만들지 않고
더하는** 방식으로 진행합니다. 그러기 위해 처음부터 고정할 설계는 [ARCHITECTURE.md](ARCHITECTURE.md)
§2에 있습니다.

## 한눈에 보기

| 단계 | 목표 | 핵심 산출물 |
|---|---|---|
| **Step 0** | 사실 확인과 범위 확정 (짧게, 병행) | 실제 Tarmac 레코드 조사 결과, Verdi HW/SW Debug 평가표, 환경 제약 목록 |
| **Step 1** | Tarmac 기반 디버거 (사람 + AI) | 인덱스 엔진, GDB RSP 서버, 최소 MCP 도구, CLI, SW 트랙 waveform, 제약사항 문서 |
| **Step 2** | waveform으로 정확도 보충 + HW 연동 | waveform 공급자, 메모리·SFR 복원, 뷰어 커서 동기화, 신호 매핑 자동 검증 |
| **Step 3** | 전체 기능 | 여러 run 분석, 멀티코어, SW+HW 조건, 프로파일러·coverage 연동, 서버 측 AI 분석, RISC-V 실사용 |

## Step 0. 사실 확인과 범위 확정

개발 전에 답이 필요한 질문들입니다. 대부분 사용자가 폐쇄망에서 확인합니다.

| # | 확인할 것 | 방법 | 결정되는 것 |
|---|---|---|---|
| 0-1 | 실제 Tarmac에 있는 레코드 종류 | 아래 명령으로 종류별 개수 | 메모리 쓰기 데이터(`ST`/`MW`/`BNW` 등), 모든 레지스터 쓰기(xPSR, 뱅크된 SP, FPU), 예외 stacking 쓰기 기록 여부 → Step 1에서 복원 가능한 범위 |
| 0-2 | Verdi HW/SW Debug 평가 | 실제 Tarmac 하나로 `hwsw_debug_convert` 실행 | M4/M35P `cpuType` 지원, 역방향 step, call stack·변수, 메모리 출처, SFR 보기, 대용량 성능 → Step 2에서 우리가 만들 범위 |
| 0-3 | Cadence Verisium ESW Debug | AE 문의 | 입력 형식, 지원 코어, 외부 디버거 연동 |
| 0-4 | 실행 환경 | 확인 | Python 버전(변환기는 3.6.8 지원), 폐쇄망에서 쓸 수 있는 AI 클라이언트와 MCP 허용 여부, VS Code 확장 오프라인 설치(vsix), arm-none-eabi-gdb 버전 |
| 0-5 | waveform 환경 | 확인 | 덤프 범위(메모리 배열·bus 포함 여부), VCD 변환 시간과 크기, Verdi NPI/VC Apps 사용 가능 여부 |
| 0-6 | 규모 | 확인 | 대표 Tarmac 크기(명령어 수), 한 회귀에서의 run 수, 코어 수 |

0-1 확인 명령:

```bash
zcat -f tarmac.log | awk '{t=($2=="ps"||$2=="tic"||$2=="ns")?$3:$1; sub(/[0-9_].*$/,"",t); print t}' \
  | sort | uniq -c | sort -rn | head -20
```

**완료 기준:** 위 표가 채워지고, Step 1의 "복원 가능한 값" 목록과 Step 2의 범위(상용 도구에 맡길 것 / 우리가 만들 것)가 정해짐.

## Step 1. Tarmac 기반 디버거

### 범위

- **Runway (수집·인덱싱)**
  - Tarmac 공급자: ES, IT/IS/IF, CCFAIL, IF 중복, 예외. 레지스터 쓰기(`R`), 메모리 접근(`LD`/`ST`/`MR`/`MW`/`BNR`…).
    변환기 파서를 출발점으로 확장.
  - 인덱스 저장소: 명령어, 레지스터 쓰기, 메모리 접근, 예외 테이블과 역색인(PC → 시점들, 주소 → 쓰기 시점들), 일정 간격 상태 checkpoint.
  - ELF: 심볼, DWARF line table, 초기 메모리 이미지(.data/.rodata/.text).
- **엔진**
  - 임의 시점의 레지스터·메모리 복원(값 + 출처).
  - 질의: 위치(locate), 실행 시점 목록(hits), 마지막 쓰기(last_write), 조건 검색(find_first), 예외 목록, 상태 비교(diff).
  - 소스 귀속: inline, line 0 규칙(변환기와 동일).
- **GDB RSP 서버**
  - Arm M-profile target description(r0–r12, sp, lr, pc, xpsr; FPU, msp/psp 등은 Tarmac에 있으면).
  - `g`/`p`/`m`, `s`/`c`, `Z0`/`z0`(Thumb kind 2/3/4), **역방향 `bs`/`bc`**(인덱스가 있으므로 Step 1에 포함 권장), 쓰기 명령은 거부.
  - `monitor time`, `monitor goto <time|index>`, `monitor hits <file:line>`, `monitor info source`(값 출처 요약).
  - VS Code에서 연결(cortex-debug 또는 cppdbg의 remote target) 검증.
- **MCP 서버 (최소 도구)**: [ARCHITECTURE.md](ARCHITECTURE.md) §5 표의 Step 1 도구. skill 문서(사용법과 분석 절차) 포함.
- **CLI**: `index`, `serve` (GDB), `mcp`, `query`, `track` (SW 트랙 waveform).
- **SW 트랙 waveform**: Tarmac → VCD(현재 함수, 파일:라인, PC, call depth, 예외 번호, 관심 라인 표시 신호). Verdi/SimVision에서 HW waveform과 함께 엶.
- **제약사항 문서**: [LIMITATIONS.md](LIMITATIONS.md)를 실제 동작에 맞게 유지.
- **ISA**: Arm(Cortex-M4, M35P) 구현. RISC-V는 ISA 정의 계층만 준비(레지스터 정의 파일 수준).

### 검증

- QEMU로 만든 테스트 펌웨어 trace(변환기 `tests/firmware` 방식)에서 **gdb 자체 실행 결과와 비교**:
  같은 펌웨어를 QEMU gdbstub으로 실행해 특정 시점의 레지스터·backtrace·변수를 기록하고, SimGDB가 같은 값을 주는지 확인.
- GDB 서버: arm-none-eabi-gdb로 스크립트 테스트(breakpoint, step, next, finish, reverse-step, backtrace, print).
- MCP: 도구 단위 테스트 + **AI 평가 세트**(질문과 기대하는 답 몇 개)로 실제 AI 클라이언트에서 확인.
- 성능: 수백만~수천만 명령어 trace에서 인덱싱 시간, 질의 응답 시간 측정.

### 완료 기준

- VS Code에서 실제 펌웨어 trace로 breakpoint, step/next/finish, 역방향 step, backtrace, 변수 보기가 동작.
- 모르는 값은 "모름"으로 표시되고, 값마다 출처를 확인할 수 있음.
- AI 클라이언트가 MCP 도구로 평가 세트 질문에 증거와 함께 답함.
- SW 트랙 waveform을 Verdi에서 HW waveform과 함께 볼 수 있음.
- 사용자가 폐쇄망 실제 데이터로 확인.

## Step 2. waveform 보충과 HW 연동

Step 0-2 평가 결과에 따라 범위를 조정합니다(상용 도구가 충분히 하는 부분은 만들지 않음).

### 범위

- **waveform 공급자**: VCD 우선(벤더 도구로 변환, 필요한 신호·구간만). 이후 FSDB 직접 읽기(NPI) 검토.
  공급자 인터페이스 뒤에 두어 형식을 바꿔 끼울 수 있게.
- **메모리 복원 보강**: 메모리 배열 대신 **bus 트랜잭션**(모든 master의 쓰기)으로 메모리 상태 복원 → DMA 반영.
- **SFR**: IP register map(IP-XACT, SystemRDL, 사내 형식)으로 **SFR 주소 → RTL 신호 경로** 매핑 자동 생성.
  GDB 메모리 읽기와 MCP `sfr` 도구에서 waveform 값을 출처 `waveform`으로 제공.
- **출처 충돌 규칙**: Tarmac과 waveform 값이 다를 때 둘 다 보존하고 표시.
- **시간 해상도**: 명령어 단위와 cycle 단위 시점 모두 지원. ES 형식(`tic`)은 시간 변환 규칙 필요.
- **신호 매핑 검증**: waveform에서 뽑은 retire PC·레지스터 쓰기를 같은 run의 Tarmac과 자동 비교(회귀 테스트).
- **뷰어 커서 동기화**: 뷰어 어댑터(Verdi Tcl/NPI, SimVision Tcl; 오픈소스 Surfer WCP·VaporView는 선택).
  GDB 정지 이벤트 → 뷰어 커서 이동, 뷰어 시간 선택 → `goto`. 이벤트 채널(JSON over TCP/WebSocket).
- **VS Code 보조 확장**: 디버그 세션 정지 시 뷰어 동기화, 상태 표시줄에 현재 시뮬레이션 시간.
- **MCP 도구 추가**: `sfr`, `sfr_history`, `bus_transactions`, `signal_changes`, 비동기 작업(긴 질의).

### 완료 기준

- VS Code와 AI에서 SFR과 DMA가 쓴 메모리가 정확히 보이고, 출처가 `waveform`으로 표시됨.
- GDB에서 멈춘 시점에 Verdi/SimVision 커서가 따라옴(그 반대도).
- 신호 매핑이 Tarmac과 자동 비교로 검증됨.

## Step 3. 전체 기능

### 범위

- **여러 run**: 회귀 테스트 수백 개를 한 데이터셋으로 인덱싱. "이 라인/분기를 실행한(안 한) 테스트",
  **통과 run과 실패 run의 첫 분기점(first divergence)** 찾기.
- **멀티코어**: core별 trace, 시뮬레이션 시간 기준 전체 순서, 공유 메모리 쓰기 추적.
- **SW+HW 조건**: `find_first(line == X && signal("irq") == 1)`, 조건부 breakpoint에 HW 신호.
- **watchpoint**: 질의 기반(`writes_to`)을 GDB `Z2`/`Z3`/`Z4`로도 제공.
- **연동**: tarmac_to_cachegrind의 coverage index·branch coverage, performance_profiler 화면과 연결
  (예: never 분기 클릭 → 그 분기를 탈 뻔한 run과 시점).
- **서버 측 AI 분석**: Undo `explain`과 같은 분석 에이전트, 회귀 실패 자동 분류, 보고서.
- **RISC-V 실사용**: 로그 형식(Spike commit log, 코어별 tracer, RVFI) 공급자, 레지스터·CSR 정의, 명령어 분류(압축 명령어 포함).
- **권한 단계**: 읽기 도구와 실행 도구(시뮬레이션 재실행 등) 분리, 실행 도구는 승인 절차.

### 완료 기준

- 필요한 항목을 Step 1·2 사용 피드백으로 선택해 단계적으로 완료. 각 항목은 AI 평가 세트와 사용자 실데이터 확인을 거침.

## 위험과 대응

| 위험 | 영향 | 대응 |
|---|---|---|
| 실제 Tarmac에 메모리 쓰기 데이터나 일부 레지스터가 없음 | 변수·backtrace 정확도 하락 | Step 0-1에서 확인. 없으면 "모름" 표시, Step 2 waveform으로 보충 |
| 대용량 trace(수 GB) | 인덱싱 시간, 디스크 | 스트리밍 파서, 열 기반 저장, checkpoint 간격 조정, 필요 구간만 인덱싱 |
| 폐쇄망 Python 버전이 낮음 (MCP Python SDK는 3.10+) | MCP 서버 구현 제약 | Step 0-4에서 확인. 엔진은 낮은 버전 호환, MCP 앞단만 별도 런타임 또는 직접 구현 |
| Cortex-M waveform 신호 찾기 | Step 2 지연 | Arm RTL의 Tarmac tracer 모듈이 쓰는 신호 참고, 설정 파일화, Tarmac 비교로 검증 |
| 상용 도구와 기능 중복 | 노력 낭비 | Step 0-2 평가로 역할 분담 |
| AI가 도구를 잘못 쓰거나 값을 지어냄 | 잘못된 분석 | 적고 원자적인 도구, 출처·증거 필수, skill 문서, 평가 세트, 호출 로그 |
| VCD 변환 크기 | Step 2 실용성 | 신호·구간 선택 변환, 이후 FSDB 직접 읽기 |

## 미정 사항 (결정 필요)

- 구현 언어: 엔진은 **Python**(변환기 재사용, 폐쇄망 배포 용이)이 1순위 후보. 성능이 부족한 부분만
  네이티브 확장. Step 0-4 결과로 확정.
- 인덱스 저장 형식: SQLite / 열 기반 파일(Parquet 등) / 자체 형식. Step 1 초기에 프로토타입으로 비교.
- GDB RSP 구현: 직접 구현(패킷 수가 적음) vs 라이브러리.
- 변환기 파서를 공유 패키지로 분리할지, SimGDB 쪽으로 복사해 시작할지.
