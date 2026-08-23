#!/bin/bash
# PGO (プロファイル誘導最適化) でネイティブの実行ファイルを作る (台帳のA3)。
#
# 2段階ビルド: ①計測入りでビルドして **3OS起動回帰 (regress)** を1回流し、
# ②その実測プロファイルで最適化し直す。matchの並びや分岐予測が
# 実頻度に合わせて最適化され、交互A/Bで約-25% (2026-08-11 実測、ADR-0009)。
#
# 2026-08-11 に「運用判断を開発に持ち込まない」で寝かせ、2026-08-23 に
# ADR-0009 の復帰条件 (2) で解放した。運用判断を殺す規則はこれ:
#   **訓練セットは回帰スイート (regress) と同一に固定する。** 回帰に足す = 訓練にも
#   載る、の一本化。「このワークロードが遅いから再訓練」は無い — 回帰に足すだけ
#
# 普段のcargo buildには混ぜない — プロファイルという入力が増えると
# ビルドの再現性が下がるので、**速いネイティブ実行ファイルが欲しいとき
# だけ**これを使う。wasmには適用しない (rustcのPGOはネイティブ向け)。
# 交互A/Bの定規 (bootphase) は従来どおり通常ビルドで測る — PGOは定規ではなく
# 「検証ループを速く回すための靴」である。
#
# 前提: images/ に3OS一式 (fetch-images.sh all)。
# 出力: target/pgo-use/release/examples/ 以下 (boot / bootphase / regress など)
set -euo pipefail
cd "$(dirname "$0")/../.."

# 絶対パスで — cargo は依存クレートを各パッケージのディレクトリから
# コンパイルするので、相対パスの -Cprofile-use は「file does not exist」で落ちる
PGO_DATA="${PGO_DATA:-$PWD/target/pgo-data}"
PROFDATA=$(ls "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-profdata | head -1)
if [ -z "$PROFDATA" ]; then
    echo "llvm-profdata が無い。rustup component add llvm-tools を先に" >&2
    exit 1
fi

echo "==> ①計測入りビルド + プロファイル採取 (3OS起動回帰を1回)"
rm -rf "$PGO_DATA" && mkdir -p "$PGO_DATA"
RUSTFLAGS="-Cprofile-generate=$PGO_DATA" \
    cargo build --release --example regress --target-dir target/pgo-gen
./target/pgo-gen/release/examples/regress > /dev/null

echo "==> ②プロファイルで最適化し直す"
"$PROFDATA" merge -o "$PGO_DATA/merged.profdata" "$PGO_DATA"/*.profraw
RUSTFLAGS="-Cprofile-use=$PGO_DATA/merged.profdata" \
    cargo build --release --examples --target-dir target/pgo-use

echo "==> できた: target/pgo-use/release/examples/"
