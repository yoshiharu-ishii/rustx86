#!/bin/bash
# PGO (プロファイル誘導最適化) でネイティブの実行ファイルを作る (台帳のA3)。
#
# 2段階ビルド: ①計測入りでビルドしてLinuxブートを1回流し、
# ②その実測プロファイルで最適化し直す。matchの並びや分岐予測が
# 実頻度に合わせて最適化され、交互A/Bで約10%速くなる (2026-08-10 実測、
# 6周中5勝・中央値13.8s→12.2s)。
#
# 普段のcargo buildには混ぜない — プロファイルという入力が増えると
# ビルドの再現性が下がるので、**速いネイティブ実行ファイルが欲しいとき
# だけ**これを使う。wasmには適用しない (rustcのPGOはネイティブ向け)。
#
# 前提: images/ にLinux一式 (fetch-images.sh linux + extract-vmlinux.sh)。
# 出力: target/pgo-use/release/examples/ 以下 (bootphase / regress など)
set -euo pipefail
cd "$(dirname "$0")/.."

PGO_DATA="${PGO_DATA:-target/pgo-data}"
PROFDATA=$(ls "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-profdata | head -1)
if [ -z "$PROFDATA" ]; then
    echo "llvm-profdata が無い。rustup component add llvm-tools を先に" >&2
    exit 1
fi

echo "==> ①計測入りビルド + プロファイル採取 (Linuxブート1回)"
rm -rf "$PGO_DATA" && mkdir -p "$PGO_DATA"
RUSTFLAGS="-Cprofile-generate=$PGO_DATA" \
    cargo build --release --example bootphase --target-dir target/pgo-gen
./target/pgo-gen/release/examples/bootphase > /dev/null

echo "==> ②プロファイルで最適化し直す"
"$PROFDATA" merge -o "$PGO_DATA/merged.profdata" "$PGO_DATA"/*.profraw
RUSTFLAGS="-Cprofile-use=$PGO_DATA/merged.profdata" \
    cargo build --release --examples --target-dir target/pgo-use

echo "==> できた: target/pgo-use/release/examples/"
