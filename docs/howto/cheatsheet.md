# チートシート — よく打つコマンド

増えてきたコマンドの早見表。**まず `tools/fetch-images.sh` でイメージを揃える**
(GPL配布物はリポジトリに置かない方針 — 入手経路はこのスクリプト一つ)。
詳しい背景はリンク先へ。ここは「手が覚える前に引く表」。

## イメージを揃える (最初に一度)

```bash
tools/fetch-images.sh            # 全部 (ELKS / FreeDOS / Linux)
tools/fetch-images.sh linux      # Linux (vmlinux + initramfs) だけ
tools/fetch-images.sh test386    # CPU互換テストROM (nasmで焼く)
tools/extract-vmlinux.sh         # bzImage から vmlinux を取り出す
tools/make-mini-initramfs.sh     # busyboxの最小initramfsを組む
```

## 走らせる

| やりたいこと | コマンド |
|---|---|
| Linuxを対話起動 (シリアルをターミナルへ) | `cargo run --release --example run -- images/vmlinux` |
| ディスクからOS起動 | `cargo run --release --example boot -- images/fd1440.img` |
| 起動して1コマンド打って結果を見る | `cargo run --release --example boot -- images/fd1440.img 50000000 root "uname -a"` |
| gdb風デバッガで追う | `cargo run --release --example dbg -- images/fd14games.img` |
| スナップショットから即起動 | `cargo run --release --example snapboot` |
| ブラウザ版 (別ポートで) | `python3 web/serve.py` → `http://localhost:8000/?kernel=vmlinux` |

## 測る (速度・命令数)

| 測るもの | コマンド |
|---|---|
| 命令毎秒 (機械まるごと) | `cargo run --release --example bench -- asm/bench.bin` |
| 命令毎秒 (CPUだけ、ラッパー無し) | `cargo run --release --example bench_raw -- asm/bench.bin` |
| 起動のどの区間に命令を使うか | `cargo run --release --example bootphase` |
| どのカーネル関数で燃えているか (ipサンプル) | `cargo run --release --example bootprof -- images/vmlinux > /tmp/p.txt` |
| ↑を関数名に解決 | `uv run --with pyelftools python3 tools/bootprof-resolve.py /tmp/p.txt images/System.map-lts` |
| v86と同一カーネルでMIPS比較 | `node tools/webtest/v86-bench.mjs` |

速度は**交互A/B**で判定する (単発の速さは熱ダレの運)。詳細は
[perf.md](../reference/perf.md)。

## 互換を検証する (門番)

互換ピラミッドの各層。安い順。設計は [ci.md](../reference/ci.md)、
方針は [ADR-0010](../adr/0010-test386-full-compat.md)。

| 層 | コマンド | 何を見るか |
|---|---|---|
| **L0** 命令単位 | `cargo test -p rustx86-cosim` | Unicornと毎命令照合 (初回はUnicornビルドで数分) |
| **L1** CPU総合 | `cargo run --release --example test386` | test386.asm完走 + EE出力照合。落ちた所は下記TRACEで |
| **L2** カーネル実走 | `cargo run -p rustx86-cosim --release --example kernel_lockstep` | 実カーネルをUnicornと1命令ずつ突き合わせ |
| **L3** OS起動 | `REGRESS_MIN_MIPS=10 cargo run --release --example regress` | 3OSがプロンプト到達 + MIPS下限 + スクショ |
| wasm経由の到達 | `node tools/webtest/headless.mjs` | ブラウザ実体 (wasm) でシェル到達 + snake盤面。exit 0が合格 |
| JIT決定性 | `node tools/webtest/jit-check.mjs` | interp と jit がビット同一か |
| JIT決定性の二分探索 | `node tools/webtest/jit-lockstep.mjs` | 食い違い箇所をスナップショット並列で挟み撃ち |

test386が途中で止まったら**POSTの足跡**が落ちたテスト番号を指す。
直近命令のトレースはこれ:

```bash
TEST386_TRACE=1 cargo run --release --example test386
```

番号→内容は test386 の README、命令→番地は `test386.lst`
(`tools/fetch-images.sh test386` が作業ディレクトリに残す) で引く。

## コード検査 (CIと同じもの、手元で)

```bash
cargo test                                  # ユニットテスト
cargo build -p rustx86-wasm --target wasm32-unknown-unknown --release  # wasmが壊れてないか
cargo fmt --all --check                     # 整形 (直すのは cargo fmt --all)
cargo clippy --workspace --exclude rustx86-cosim --all-targets -- -D warnings
```

CIはこれらを段構えで回す ([ci.md](../reference/ci.md))。PRのChecksタブは
合否掲示板になっている (関門ごとに1行、クリックで本文)。

## ブラウザ版を焼く・配る

```bash
bash tools/build-web.sh          # wasmを焼いて web/ へ (wasm-pack + wasm-opt)
python3 web/serve.py             # 手元配信 (Cache-Control: no-store 付き)
python3 web/serve.py 8001        # 検証用は別ポートで (ユーザーの8000と衝突させない)
```

## デバッガの中でよく打つ (example dbg)

`dbg` の対話コマンドは [dbg.rs](../../core/examples/dbg.rs) の冒頭ヘルプが正。
ブレーク・メモリ監視・I/O監視・トレース記録が打てる。
