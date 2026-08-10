#!/usr/bin/env bash
# 起動済みLinuxのスナップショットを作り、web/ へ配る。
#
#   tools/make-linux-snapshot.sh
#
# ネイティブで一度シェルまで起動して丸ごと保存し (images/linux-booted.snap)、
# gzipしてブラウザ用に置く (web/linux-booted.snap.gz)。
# linux-machine.js はこのファイルがあれば「秒で起動」し、無ければ
# 従来どおりカーネルからフル起動する。イメージ同様、配布物なのでコミットしない。
set -euo pipefail
cd "$(dirname "$0")/.."

cargo run --release --example snapboot -- save
# 復元して対話できることまで確かめてから配る (壊れた控えを配らない)
cargo run --release --example snapboot -- load
gzip -9 -c images/linux-booted.snap > web/linux-booted.snap.gz
ls -la images/linux-booted.snap web/linux-booted.snap.gz
