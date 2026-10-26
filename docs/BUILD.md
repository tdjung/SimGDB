# 빌드 방법

Rust를 몰라도 됩니다. Rust 빌드 도구 `cargo`가 의존 라이브러리 다운로드, 컴파일, 링크를 모두 합니다.
인터넷이 되는 Linux에서 빌드하고, 폐쇄망에는 만들어진 실행 파일(또는 패키지)만 반입합니다.

## 1. 준비 (처음 한 번)

필요한 것: C 컴파일러(`gcc` 또는 `cc`, 링크와 내장 SQLite 빌드에 씀), `git`, `curl`.

```bash
# Ubuntu/Debian: sudo apt install build-essential git curl
# RHEL/Rocky:    sudo dnf groupinstall "Development Tools"

# Rust 설치 (관리자 권한 불필요, ~/.cargo와 ~/.rustup에 설치)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
cargo --version        # 1.88 이상이면 됩니다
```

이미 설치되어 있으면 `rustup update`로 최신화합니다.

## 2. 빌드

```bash
git clone https://github.com/tdjung/SimGDB.git
cd SimGDB
cargo build --release -p simgdb-cli
./target/release/simgdb --version
```

- 처음 빌드는 의존 라이브러리를 받고 컴파일하느라 몇 분 걸립니다. 이후에는 바뀐 부분만 다시 빌드합니다.
- 결과물: `target/release/simgdb` 실행 파일 하나. 이 파일만 복사해 쓰면 됩니다.
- 이 실행 파일은 **빌드한 머신의 glibc 버전 이상**에서만 실행됩니다. 폐쇄망 OS가 더 오래됐다면 아래 3번을 쓰세요.

## 3. 폐쇄망 반입용 패키지 (오래된 glibc 대응)

glibc 2.17 이상(RHEL/CentOS 7 이후)에서 실행되는 실행 파일과 자체 점검 도구를 한 파일로 묶습니다.

```bash
pip install ziglang               # 다른 glibc 버전을 대상으로 링크하는 데 씀
cargo install cargo-zigbuild      # 한 번만
./dist/package.sh                 # → target/dist/simgdb-<버전>-linux-x86_64-glibc2.17.tar.gz
```

압축을 푼 뒤의 확인 절차는 패키지 안 `CHECK.md`(= `dist/CHECK.md`)에 있습니다.

## 4. 테스트 (선택)

```bash
cargo test                        # 전체 테스트. QEMU나 Arm 툴체인 없이 돌아감
cargo test -p simgdb-engine --test qemu_truth -- --nocapture   # QEMU 정답과 비교한 표 출력
```

gdb로 GDB 서버를 확인하는 스크립트 테스트는 `tests/gdb/README.md`를 보세요(`arm-none-eabi-gdb`와 Python 3.6 이상 필요).

## 자주 겪는 문제

| 증상 | 원인과 해결 |
|---|---|
| `linker 'cc' not found` | C 컴파일러가 없음 → 1번의 `build-essential` / `Development Tools` 설치 |
| `failed to download` / `failed to fetch` (사내 프록시) | `export HTTPS_PROXY=http://프록시:포트` 후 다시 실행 |
| `package requires rustc 1.88` | `rustup update` |
| 폐쇄망에서 `GLIBC_2.xx not found` | 3번 패키지(glibc 2.17 대상)를 사용 |

## 폐쇄망 안에서 직접 빌드해야 한다면

기본 방식은 반입만 하는 것이지만, 꼭 필요하면 가능합니다.
- 외부에서 `cargo vendor`로 의존 라이브러리 소스를 받아 저장소와 함께 반입합니다.
- Rust는 오프라인 설치 파일(standalone installer)로 설치합니다.

필요해지면 절차를 따로 정리하겠습니다.
