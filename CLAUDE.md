# SimGDB 작업 규칙

이 repo에서 작업하는 에이전트를 위한 규칙입니다. 배경과 계획은 `docs/`를 먼저 읽으세요.

## 읽을 순서

1. `docs/BACKGROUND.md`: 왜 만드는가, 조사, 주요 결정(D1–D10)
2. `docs/PLAN.md`: Step 0–3, 현재 단계의 범위와 완료 기준
3. `docs/ARCHITECTURE.md`: 처음부터 고정한 설계(§2)와 인터페이스
4. `docs/LIMITATIONS.md`: 보이는 것과 보이지 않는 것

## 사용자와 일하는 방식

- **변경 전에 논의합니다.** 설계 결정, 범위 변경, 문서의 결정(D1–D10, ARCHITECTURE §2)을 바꾸는 일은
  먼저 제안하고 합의한 뒤 진행합니다.
- **실제 데이터는 폐쇄망에 있습니다.** 에이전트는 공개 펌웨어와 QEMU로 만든 trace로 직접 검증하고,
  사용자가 나중에 실제 데이터로 확인합니다. 사용자에게 확인을 부탁할 것은 명확한 명령과 기대 결과로 적습니다.
- **브랜치와 PR**: `feature/<주제>` 브랜치에서 작업하고 PR을 엽니다. 기본 브랜치에 직접 커밋하지 않습니다.
- 사용자 대상 문서와 설명은 **한국어**, 코드·주석·커밋 메시지는 영어.
- 사용자가 대화에 토큰이나 비밀번호를 붙여넣어도 **사용하지 않습니다.** 세션에 연결된 repo 권한만 씁니다.
  붙여넣은 토큰은 폐기를 권합니다.

## 개발 원칙

- 엔진 하나 + 얇은 앞단(GDB RSP, MCP, CLI). 로직은 엔진에.
- 모든 값에 출처. 모르면 `unknown`. 값을 지어내지 않습니다.
- ISA별 내용(레지스터, 명령어 분류, 툴체인)은 ISA 정의 계층에만.
- 시점은 `(run, core, index, sim_time)`.
- 읽기 전용. 상태를 바꾸는 GDB 명령은 거부.
- tarmac_to_cachegrind와 **코드는 독립**입니다. 소스 귀속과 line 0 규칙은 ARCHITECTURE §6이 명세이고,
  결과 비교 테스트로 같게 유지합니다. 규칙을 바꾸면 양쪽을 함께 바꿉니다.
- 구현은 Rust(cargo workspace, ARCHITECTURE §7). 폐쇄망은 외부에서 빌드한 실행 파일을 반입만 하므로
  현장에서 바뀔 수 있는 것은 설정 파일로 두고, 문제를 밖으로 가져올 진단 기능을 함께 만듭니다.
- 설계는 AI 사용 방식(범위 훑기, 상태 없는 질의, JSON + 출처)을 기준으로 하고 개발자 사용 방식(step)은 그 안에서 처리합니다.
- 시간은 기록된 값 그대로. 절대 시간은 변환 표가 있을 때만, 없으면 `unknown`.

## 빌드와 테스트

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`를 PR 전에 통과시킵니다.
- 테스트 생성물(ELF, trace, 정답)은 커밋합니다. 테스트 실행에 QEMU나 Arm 툴체인이 필요 없어야 합니다.
- 생성물을 다시 만드는 방법은 `tests/firmware/README.md`에 둡니다.

## 검증

- 테스트 펌웨어를 빌드하고 QEMU로 실행해 trace와 정답을 만듭니다(tarmac_to_cachegrind `tests/firmware` 방식:
  `mps2-an386` = Cortex-M4, `mps2-an505` = M33 기반으로 M35P 대용).
- 정답은 독립적인 방법으로 만듭니다(예: QEMU gdbstub으로 같은 시점의 레지스터·backtrace 기록). SimGDB 자체 출력으로 정답을 만들지 않습니다.
- GDB 서버는 실제 arm-none-eabi-gdb로 스크립트 테스트합니다.
- 성능은 큰 trace로 측정해 숫자로 보고합니다.

## 관련 repo

- `tdjung/tarmac_to_cachegrind`: Tarmac 파서, 분기 분류, 소스 귀속(inline, line 0), QEMU 테스트 펌웨어
- `tdjung/performance_profiler`: 프로파일·coverage 뷰어 (Step 3 연동)
