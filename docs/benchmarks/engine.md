# 인덱싱·질의 성능 (Step 1 첫 마일스톤)

- 측정일: 2026-10-25, 개발 컨테이너 x86_64 2코어, 메모리 7 GB, release 빌드
- 입력: `tests/firmware/traces/state_m4.m4rtl.log.gz`(Cortex-M4 RTL 방언, 4,748 명령어)를 2,106번 이어 붙이고
  cycle 값을 이어지게 바꾼 trace. **약 1천만 명령어**, 2,423만 줄, 1.29 GB(비압축), 예외 109,512개.
  내용이 반복되므로 PC 분포는 실제보다 좁습니다(한 PC가 최대 약 25만 번 실행).

## 인덱싱

| 항목 | 값 |
|---|---:|
| 시간 | 23–28 s (약 90만–105만 줄/s, 단일 스레드; 리뷰 수정 후 28 s, call 표·소스 표 추가 후 23 s) |
| 최대 메모리 | 2.7 GB |
| 인덱스 크기 | 1.8 GB (trace의 1.4배; call 표 추가 후 1.9 GB, `calls` 25 MB + `instructions.frame` 40 MB) |

파일별 크기: `mem_index` 840 MB(리뷰 수정 후 911 MB), `reg_writes` 324 MB, `instructions` 307 MB, `mem` 192 MB, `pc_index` 162 MB.
`mem_index`의 절반 이상은 예외 진입 때 stacking 영역을 “모름”으로 표시하는 바이트 항목입니다(예외마다 진입 전 SP 아래 216바이트 + frame 크기).

## 질의 지연 (임의 시점 1,000개)

이어 붙인 trace는 복사본마다 reset 기록 없이 `Reset_Handler`부터 다시 시작하므로, 끝나지 않은 `main` 호출이 쌓여
call stack이 최대 2,104단계까지 깊어집니다(실제 trace에서는 생기지 않는 최악의 경우). `locate` 지연은 이 깊이를 포함한 값입니다.
call 표를 처음 넣었을 때 이렇게 쌓인 frame을 명령어마다 훑어 인덱싱이 63 s로 느려졌고, 열린 복귀 주소 표로 훑기를 건너뛰게 고쳐 23 s로 돌아왔습니다.

| 질의 | page cache 비운 직후: 중앙값 / p99 | 데워진 상태: 중앙값 / p99 |
|---|---:|---:|
| 열기 | 18.6 ms | 1.6 ms |
| 전체 레지스터 | 0.395 / 2.34 ms | 0.027 / 0.087 ms |
| SP 주변 메모리 64 B | 0.164 / 12.9 ms | 0.090 / 0.735 ms |
| `.bss` 메모리 256 B | 0.216 / 1.37 ms | 0.182 / 0.338 ms |
| `locate`(소스 줄 + call stack), 2026-10-26 | – | 0.422 / 1.43 ms |
| 다음 실행 지점(breakpoint, continue) | 0.002 / 1.68 ms | 0.001 / 0.044 ms |
| 이전 실행 지점, PC 4개(reverse-continue) | 0.003 / 0.144 ms | 0.003 / 0.010 ms |
| 한 PC의 모든 실행(248,508개) | 0.35 ms | 0.35 ms |

대화형 목표(100 ms 이하)를 모두 만족합니다.

## 개선할 점 (규모가 커질 때)

- **메모리 사용**: 인덱서가 표를 메모리에 모은 뒤 씁니다. 1억 명령어면 약 27 GB가 필요하므로,
  구간별로 쓰고 정렬은 외부 병합으로 바꿔야 합니다.
- **`mem_index` 크기**: 예외 진입의 “모름” 범위를 바이트마다 넣지 않고 구간 표로 따로 두면 크게 줄어듭니다.
- **병렬화**: 파싱은 단일 스레드입니다. trace를 구간으로 나눠 병렬로 파싱할 수 있습니다.

## 다시 실행하기

```bash
# 큰 trace 만들기 (예: tests/firmware/traces의 trace를 반복)
simgdb index big.log --elf state_m4.elf -o idx_big
cargo run --release -p simgdb-engine --example bench_queries -- idx_big
```
