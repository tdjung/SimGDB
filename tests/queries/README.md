# 질의 정답 (A3)

범위 질의(`executions`, `exceptions`, `call_tree`, `last_write`, `writes_to`, `diff`, `find_first`)를
SimGDB와 독립적으로 구한 답과 비교합니다. 테스트: `cargo test -p simgdb-engine --test queries`
(세 Tarmac 방언 × 세 코어 `m4`, `m35p`, `m4be`(big-endian BE8)가 모두 같은 답을 내야 함).

## 정답을 만드는 방법

`make_expected.py`가 SimGDB를 쓰지 않고 다음에서 계산합니다.

| 정답 | 입력과 정의 |
|---|---|
| 줄별 실행 수 | tarmac_to_cachegrind `--pc-counts`(fm trace)의 PC별 Ir을 줄로 합침 |
| 줄 진입 시점 | canonical log를 처음부터 훑음: 그 줄의 명령어이고, 같은 문맥의 앞 명령어(예외에서 돌아온 직후면 예외 직전 명령어)가 그 줄이 아님 |
| 예외 | canonical log의 진입 기록과 시점마다의 예외 번호(IPSR): 진입 기록이 아닌 IPSR 변화는 복귀이거나(바깥 수준으로) tail-chaining(다른 수준으로). 핸들러가 EXC_RETURN으로 복귀하는 명령어(`bx lr`, `pop {pc}`) 바로 뒤의 진입 기록은 tail-chaining(앞 예외는 거기서 복귀) |
| 호출 | objdump의 `bl`/`blx`가 실행되고(LR 확인), 복귀 주소에 같은 SP로 돌아온 첫 시점. 복귀 명령어 직후 예외가 들어와 쌓인 복귀 주소가 그 주소이고 진입 전 SP가 같으면 그 진입 시점. 스택을 바꾼 함수(`use_process_stack`)는 CONTROL이 바뀐 채 돌아온 첫 시점 |
| 마지막 쓰기, 쓰기 이력 | canonical log의 메모리 쓰기 기록(전역변수 주소). 값은 코어의 byte 순서로 바이트에 나눔. 쓰기 이력은 바이트 단위로 비교(16바이트 줄을 넘는 접근을 `ST` 그림 두 개로 적는 방언이 있음) |
| 상태 비교 | canonical log의 레지스터와, 접근 기록으로 쌓은 메모리(두 시점 모두 아는 바이트만) |
| 조건 검색 | 모든 시점에서 조건을 Python으로 직접 평가(줄·함수는 변환기 귀속, 모르는 값은 모름) |

canonical log는 QEMU 플러그인의 원본 기록이고(`tests/firmware/traces/*.canonical.log.gz`), Tarmac 방언은 이것을
바꿔 쓴 것입니다. 이 기록에는 tail-chaining 진입 기록이 없어서 Tarmac에도 없습니다. SimGDB는 이것을 예외 번호로
추정하고(LIMITATIONS), 정답도 IPSR로 독립적으로 찾습니다.

## 다시 만들기

```bash
X=/path/to/arm-none-eabi/bin
NM=$X/arm-none-eabi-nm OBJDUMP=$X/arm-none-eabi-objdump ADDR2LINE=$X/arm-none-eabi-addr2line \
  python3 tests/queries/make_expected.py --converter /path/to/tarmac_to_cachegrind.py
```

변환기는 `feature/branch-coverage`(ed9fa51)로 만들었습니다.
