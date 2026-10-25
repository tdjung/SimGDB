# 저장 형식 비교 (D12)

인덱스 저장소 후보 세 가지를 같은 데이터와 같은 질의로 비교했습니다.

- 측정일: 2026-10-25
- 프로그램: [`bench/storage-formats`](../../bench/storage-formats) (Rust, release 빌드)
- 환경: 개발 컨테이너, x86_64 2코어, 메모리 7 GB, Rust 1.97, arrow/parquet 60.0, SQLite 3 (rusqlite bundled)

## 데이터

결정적 합성 trace입니다. 실제 trace의 분포를 흉내 냈지만 실제 데이터는 아닙니다.

| 표 | 행 수 | 열 |
|---|---:|---|
| `instructions` | 10,000,000 | pc u32, time u64, flags u8, opcode u32 (index는 행 번호) |
| `reg_writes` | 9,501,322 | idx u64, reg u8, value u32 |
| `mem_accesses` | 2,996,507 | idx u64, addr u32, size u8, write u8, value u64 |
| `checkpoints` | 2,442 (4,096 명령어마다) | 레지스터 17개, reg_writes 재생 시작 행 |
| `pc_index` | 10,000,000 | (pc, idx) 정렬 |
| `write_index` | 1,165,575 | (4바이트 단위 주소, idx, 행) 정렬 |

## 후보

- **Arrow IPC (mmap)**: 압축 없는 Arrow 파일을 mmap하고 복사 없이 읽음. 정렬 열은 이진 탐색.
- **Parquet zstd**: 압축 파일을 열 때 메모리로 모두 풀어 읽음(Parquet은 mmap 직접 접근이 안 됨). 질의는 Arrow와 같은 코드.
- **SQLite**: 같은 표 + B-tree 인덱스 `(pc, idx)`, `reg_writes(idx)`, `write_index(granule, idx)`. 일괄 적재(transaction, journal off) 후 인덱스 생성.

## 결과 (1천만 명령어)

질의 열은 질의 하나당 **중앙값(ms)**입니다. 한 시점 질의는 임의 시점 2,000개, `executions`는 PC 200개, 훑기는 3회.
세 형식 모두 결과 checksum이 같습니다.

### page cache가 데워진 상태

| 형식 | 생성 (s) | 크기 (MB) | 열기 (ms) | 레지스터 @ i | 메모리 @ i | executions(pc) | 훑기: r0 == v | 훑기: 주소 범위 쓰기 | 훑기: pc 열 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Arrow IPC (mmap)** | 0.7 | 516 | 0.4 | 0.007 | 0.001 | 0.002 | 12 | 3 | 7 |
| Parquet zstd | 3.6 | 193 | 1,061 | 0.006 | 0.001 | 0.002 | 11 | 3 | 7 |
| SQLite | 16.9 | 830 | 0.1 | 0.569 | 0.018 | 0.515 | 781 | 140 | 315 |

### page cache를 비운 직후 (`COLD=1`)

| 형식 | 열기 (ms) | 레지스터 @ i | executions(pc) | 훑기: r0 == v | 첫 레지스터 질의 | 첫 pc 열 훑기 |
|---|---:|---:|---:|---:|---:|---:|
| **Arrow IPC (mmap)** | 5.1 | 0.022 | 0.165 | 20 | 0.43 | 28 |
| Parquet zstd | 1,174 | 0.008 | 0.002 | 12 | 0.006 | 30 |
| SQLite | 0.8 | 0.818 | 0.940 | 956 | 1.14 | 504 |

생성 시간은 메모리에 있는 표를 저장하는 시간만입니다. 실제 인덱싱 시간은 Tarmac 파싱이 대부분이라 엔진 구현 후 따로 측정합니다.

## 해석

- **Arrow IPC**: 열기가 즉시(mmap)이고 한 시점 질의가 수 µs, 1천만 행 훑기가 수~수십 ms입니다.
  1억 명령어로 늘려도 훑기는 수백 ms로 예상되어 분석형 목표(수 초)에 여유가 있습니다.
  단점은 크기(명령어당 약 52바이트, 1억 명령어면 약 5 GB)입니다. time 차분 저장, 행 번호 u32 등으로 줄일 여지가 있습니다.
- **Parquet**: 크기가 Arrow의 약 37%로 가장 작지만 열 때 1천만 명령어당 약 1초 동안 압축을 풀고 그만큼 메모리를 씁니다.
  대화형으로 바로 여는 용도에는 맞지 않고, **오래된 run 보관용**으로 적합합니다.
- **SQLite**: 한 시점 질의가 Arrow보다 수십~수백 배, 훑기가 수십 배 느리고, 생성이 약 24배 느리며 크기도 가장 큽니다.
  AI의 훑기 질의에는 맞지 않습니다. 작고 자주 바뀌는 표(메타데이터, bookmark, 호출 로그, 작업 상태)에만 씁니다.

## 결론

D12 확정: 큰 표는 **Arrow IPC(mmap)**, 작고 수정되는 표는 **SQLite**. Parquet은 보관용 선택지로 남깁니다.

## 다시 실행하기

```bash
cd bench/storage-formats
cargo run --release -- 10000000 /tmp/bench-out          # 데워진 상태
COLD=1 cargo run --release -- 10000000 /tmp/bench-out   # page cache를 비우고 (root 필요)
```
