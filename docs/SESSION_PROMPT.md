# 개발 세션 시작 프롬프트

새 세션을 시작할 때 아래 내용을 그대로 붙여넣으세요. 세션에는 `tdjung/SimGDB`와,
참고용으로 `tdjung/tarmac_to_cachegrind`(필수), `tdjung/performance_profiler`(Step 3에 필요)를 연결합니다.

---

## Step 1 시작용

```text
SimGDB 개발을 시작하자. repo는 tdjung/SimGDB이고, 참고 repo로 tdjung/tarmac_to_cachegrind를 연결해 줘.

먼저 SimGDB의 CLAUDE.md, docs/BACKGROUND.md, docs/PLAN.md, docs/ARCHITECTURE.md, docs/LIMITATIONS.md를
모두 읽고 배경, 결정(D1–D10), 처음부터 고정한 설계(ARCHITECTURE §2)를 이해해 줘.
tarmac_to_cachegrind에서는 tarmac_to_cachegrind.py(Tarmac 파싱, 분기 분류, resolve_pcs,
attribute_line_zero)와 tests/firmware(QEMU 기반 테스트 펌웨어와 trace)를 확인해 줘.

이번 세션의 목표는 Step 1의 첫 마일스톤이야:
1. 미정 사항(구현 언어, 인덱스 저장 형식, GDB RSP 구현 방식, 변환기 파서 공유 방식)에 대해
   선택지와 추천을 정리해서 나와 먼저 논의하자. 코드는 합의 후에 작성해.
2. 합의되면 저장소 골격, Point/Value 모델, Tarmac 공급자, 인덱스 저장소, 임의 시점의 레지스터·메모리 복원까지 구현하고,
   QEMU로 만든 trace와 독립적인 정답(QEMU gdbstub 등)으로 검증해 줘.
3. 그다음 GDB RSP 서버(최소 기능: target XML, g/p/m, s/c, Z0, bs/bc, monitor time/goto)를
   arm-none-eabi-gdb로 테스트해 줘.

작업 규칙은 CLAUDE.md를 따르고, main에 작업 단위별 커밋으로 push해 줘.
실제 데이터는 폐쇄망에 있어서 내가 나중에 확인할 거야. 나에게 확인을 부탁할 것은 명령과 기대 결과로 정리해 줘.
```

## Step 0 결과를 전달할 때

Step 0 확인 결과(Tarmac 레코드 종류, Verdi 평가, 환경)를 얻으면 다음처럼 전달합니다.

```text
SimGDB Step 0 결과야. docs/PLAN.md의 Step 0 표에 반영하고, LIMITATIONS.md와 Step 1·2 범위에
영향이 있는지 검토해서 변경안을 제안해 줘. 변경은 합의 후에 커밋해.

[0-1 Tarmac 레코드 종류별 개수]
...
[0-2 Verdi HW/SW Debug 평가]
...
[0-4 환경: Python 버전, AI 클라이언트/MCP 허용, VS Code 확장 설치, gdb 버전]
...
```

## 이후 단계를 시작할 때

```text
SimGDB Step <N>을 시작하자. CLAUDE.md와 docs/를 읽고, PLAN.md의 Step <N> 범위와 완료 기준,
지금까지의 구현 상태를 비교해서 작업 순서를 제안해 줘. 합의 후에 진행하자.
```
