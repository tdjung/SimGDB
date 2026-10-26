---
name: simgdb-debug
description: Debug firmware from a recorded simulation run (Tarmac trace + ELF) with the SimGDB MCP tools - find when and why something happened, with evidence.
---

# SimGDB로 시뮬레이션 기록 디버깅하기

SimGDB MCP 도구는 **이미 끝난 시뮬레이션 한 번**의 기록(Tarmac trace)과 펌웨어 ELF를 봅니다.
실행을 바꾸거나 다시 돌릴 수 없고, 기록에 있는 것만 답합니다.

## 기본 개념

- **시점(point)** = 명령어 index. 시점 i는 i번째 명령어를 **실행하기 직전** 상태(디버거가 그 PC에서 멈춘 상태)이고,
  i번째 명령어가 쓴 값은 시점 i+1부터 보입니다. 마지막 명령어 뒤가 끝 시점입니다.
- 모든 도구에 시점을 직접 줍니다: `at`(index) 또는 `time`(trace의 시간 값). 도구는 상태를 기억하지 않습니다.
- 값마다 `source`가 붙습니다: `trace`(기록에서 봄), `elf_initial`(ELF 초기값, 아직 아무도 안 씀), `inferred`(나중 읽기로 추정),
  `unknown`(기록에 없음). **`unknown`은 0이 아닙니다.** 모르면 모른다고 답하세요.
- `evidence`는 그 값을 만든 명령어 index와 trace 줄 번호입니다. 답할 때 근거로 인용하세요.
- 목록은 잘려서 옵니다. `total`을 먼저 보고, 더 필요하면 `cursor`에 `next_cursor`를 넘깁니다.

## 시작

1. `trace_info`: 명령어 수, 시간 단위, 기록된 것(capabilities), 제약(limitations), 예외 종류별 횟수.
   제약에 있는 것(예: 예외 진입 저장이 trace에 없음)은 답의 신뢰도에 반영하세요.

## 질문 유형별 절차

**"언제 X가 일어났나 / 처음 X가 된 시점"**
- 조건으로 표현되면 `find_first`(예: `var("err") != 0`, `pc == sym("HardFault_Handler")`). 마지막 시점은 `direction: "backward"`.
- 결과의 `previous.holds`가 false면 바로 앞 명령어(`previous.index`, `previous.function`)가 조건을 참으로 만든 것입니다.
- `undecided_ranges`가 찾은 시점보다 앞에 있으면 그 구간은 값을 몰라 판단하지 못했습니다. 시작 직후(초기화 전)뿐이면 보통 문제없고,
  그렇지 않으면 답에 밝히세요.
- 함수 호출·줄 실행이면 `find_executions`(target: 함수 이름, `file.c:123`, PC).

**"왜 이 값이 되었나"**
1. `read_variable`(또는 `read_memory`)로 그 시점 값 확인.
2. `last_write`로 마지막으로 쓴 명령어(index, PC, trace 줄) 확인.
3. 그 index를 `locate`해 어느 함수·줄·call stack에서 썼는지 확인.
4. 필요하면 쓰기 직전 시점의 `registers`로 계산 입력을 보고, 거슬러 반복합니다. 쓰기 이력 전체는 `writes_to`,
   레지스터가 어떻게 바뀌어 왔는지는 `register_history`.
5. `writes_to`/`last_write`의 기록에 `inside`가 있으면 그 저장이 변수 밖까지 덮은 것(넓은 스택 저장 등)이니 의도된 대입인지 확인하세요.

**예외·인터럽트·HardFault**
1. `exceptions`(한 종류만: `kind`): 종류별 횟수와 각 진입(`entered_at` = 핸들러 첫 명령어)·복귀 시점,
   `taken_after`(바로 앞 명령어: SVC·fault면 그것을 일으킨 명령어), 끊긴 PC(`interrupted_pc`).
2. 진입 시점을 `locate`하면 핸들러와 call stack, 끊긴 코드가 나옵니다.
3. HardFault면 진입 시점의 `registers`(LR = EXC_RETURN, SP)와 스택에 쌓인 frame(`read_memory` SP부터 32바이트:
   r0, r1, r2, r3, r12, lr, 복귀 주소, xPSR)을 보고, 끊긴 PC를 `locate`합니다.

**성능·흐름 개요**
- `call_tree`: 호출 경로별 횟수와 포함 명령어 수, 함수별 합계. 범위를 좁혀(`from`/`to`) 특정 구간만 봅니다.
- `find_executions`의 `instructions`는 그 줄·함수 코드가 실행된 명령어 수입니다.

**두 시점 사이에 무엇이 바뀌었나**
- `diff_state`: 바뀐 레지스터, 바뀐 변수(원소·멤버, 스칼라는 이전/이후 값), 메모리 구간. 변수만 보려면 `variables_only: true`.
  `became_known`은 값이 바뀐 것이 아니라 그때부터 알게 된 것입니다.

## 답하는 방법

- 시점(index)과 근거(evidence의 trace 줄)를 함께 적어 사람이 gdb(`simgdb-goto <index>`)나 trace에서 확인할 수 있게 합니다.
- `unknown`, `undecided`, `exceptions_inferred`처럼 추정이나 모름이 끼어든 곳은 숨기지 말고 말합니다.
- 중요한 시점은 `bookmark_add`로 메모를 남기면 gdb·VS Code 사용자와 공유됩니다.

## 할 수 없는 것

- 지역변수, 포인터 따라가기(`p->next`), C 식 계산: 디버거(gdb + `simgdb serve`)에서 봅니다.
- DMA 등 코어가 아닌 master가 쓴 메모리, 주변장치 레지스터 값: trace에 없으면 `unknown`입니다.
- 기록되지 않은 시간(trace 시작 전, 끝 후).
