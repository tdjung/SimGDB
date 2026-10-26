# AI 평가 세트

MCP 도구로 AI 클라이언트가 질문에 근거와 함께 답하는지 확인합니다(PLAN Step 1 완료 기준).

- `questions.json`: 질문 10개와 정답. 정답은 SimGDB 없이 만듭니다(`make_questions.py`: `tests/queries`의 정답과 QEMU 원본 기록).
  대상은 `tests/firmware/traces/state_m4.m4rtl.log.gz`입니다.
- `grade.py answers.json`: 답(질문 id → 답 텍스트)을 채점합니다. 정답의 숫자·이름이 들어 있어야 통과입니다.
- `mcp_call.py`: MCP를 직접 붙이지 못하는 환경에서 도구를 손으로(또는 에이전트가) 부르는 작은 클라이언트입니다.

## 실행 (Claude Code 예)

```bash
simgdb index tests/firmware/traces/state_m4.m4rtl.log.gz --elf tests/firmware/state_m4.elf -o /tmp/eval.idx
claude mcp add simgdb -- simgdb mcp /tmp/eval.idx
mkdir -p .claude/skills/simgdb-debug && simgdb skill > .claude/skills/simgdb-debug/SKILL.md
```

AI에게 `questions.json`의 질문을 주고(정답 필드는 빼고), 답을 `answers.json`으로 모아 `python3 tests/ai_eval/grade.py answers.json`.
도구 호출과 결과 요약은 인덱스 옆 `annotations.sqlite`의 `call_log` 표에 남으므로, 어떤 도구를 어떻게 썼는지 볼 수 있습니다.

## 결과

에이전트(도구 설명만 보고 `mcp_call.py`로 호출, 소스·정답 접근 금지)로 두 번 확인했습니다(2026-10-26).

| 회차 | 정답 | 도구 호출 | 주요 불편 → 반영 |
|---|---|---|---|
| 1 | 10/10 | 40 | 10진/16진 혼재 → 주소·PC는 16진 문자열. diff가 바이트 구간만 → 변수별 요약과 원소·멤버 경로. 쓴 명령어의 함수를 따로 찾아야 함 → 쓰기 기록에 함수. 예외 종류 필터 없음 → `kind`. locate가 장황 → 기본 요약형 |
| 2 | 10/10 | 25 | 조건을 참으로 만든 명령어 → `previous`. 판단 못 한 구간의 의미 → `undecided_ranges`. 변수 밖까지 덮는 저장 → `inside`. 레지스터 변화 추적 → `register_history`. 메모리 출처가 바이트마다 반복 → 구간으로 묶음 |

## 다시 만들기

```bash
NM=arm-none-eabi-nm OBJDUMP=arm-none-eabi-objdump python3 tests/ai_eval/make_questions.py
```
