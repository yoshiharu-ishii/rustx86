#!/usr/bin/env bash
# PGO (プロファイル誘導最適化) ビルド — ネイティブの汎用最適化。
#
#   bash tools/build-pgo.sh              # 訓練→焼き直し (成果物は target/pgo/)
#   target/pgo/release/examples/bootphase images/vmlinux-lts
#
# 何をするか:
#   1. -Cprofile-generate でビルドし、Linuxブート (bzImage+vmlinux) を走らせて
#      「どの分岐がどちらへ倒れるか」の実測プロファイルを採る
#   2. llvm-profdata でマージ
#   3. -Cprofile-use で焼き直す — LLVMが実測に合わせて分岐配置・インライン判断を
#      並べ替える。実測でおよそ +10% (79→87 MIPS 級)
#
# 訓練データがLinuxブートなので、**Linuxブートに最適化された**バイナリになる。
# 別ワークロード (DOSゲーム等) を速くしたければ訓練にそれを足す。
# 通常ビルド (cargo build --release) はこれまで通り — PGOは別target-dirに住む。
set -euo pipefail
cd "$(dirname "$0")/.."

PROFDIR="$(pwd)/target/pgo-profdata"
GENDIR="$(pwd)/target/pgo-gen"
USEDIR="$(pwd)/target/pgo"
PROFDATA=$(ls ~/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-profdata | head -1)
if [ -z "$PROFDATA" ]; then
  echo "llvm-profdata が無い。rustup component add llvm-tools を先に" >&2
  exit 1
fi

rm -rf "$PROFDIR"
mkdir -p "$PROFDIR"

echo "==> 1/3 計測ビルド (profile-generate)"
RUSTFLAGS="-Cprofile-generate=$PROFDIR" \
  cargo build --release -q --example bootphase --target-dir "$GENDIR"

echo "==> 2/3 訓練 (Linuxブート bzImage + vmlinux)"
"$GENDIR/release/examples/bootphase" >/dev/null
"$GENDIR/release/examples/bootphase" images/vmlinux-lts >/dev/null
"$PROFDATA" merge -o "$PROFDIR/merged.profdata" "$PROFDIR"

echo "==> 3/3 焼き直し (profile-use)"
RUSTFLAGS="-Cprofile-use=$PROFDIR/merged.profdata" \
  cargo build --release -q --example bootphase --example run --target-dir "$USEDIR"

echo "==> できた: $USEDIR/release/examples/{bootphase,run}"
