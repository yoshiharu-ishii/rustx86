#!/bin/bash
# wasmの交互A/B — ネイティブと同じ流儀を wasm でやる常設の定規。
#
# 2つの pkg ディレクトリ (wasm-pack の出力一式) を交互に headless.mjs で
# 走らせ、**時間の差だけ**を見る。単発の速さは熱ダレの運として捨て、
# 対の符号 (各ラウンドでどちらが速かったか) と平均で読む。
#
#   tools/webtest/ab.sh <pkgA> <pkgB> [rounds=5]
#
# 例: 現行mainのビルドを /tmp/pkg_a に控えてから候補ビルドと比べる
#   bash tools/build-web.sh && cp -r web/pkg /tmp/pkg_a
#   (候補をビルド)          && cp -r web/pkg /tmp/pkg_b
#   tools/webtest/ab.sh /tmp/pkg_a /tmp/pkg_b
#
# web/pkg は退避して終了時に必ず戻す。
set -u
cd "$(dirname "$0")/../.."
A=${1:?使い方: ab.sh <pkgA> <pkgB> [rounds]}
B=${2:?使い方: ab.sh <pkgA> <pkgB> [rounds]}
R=${3:-5}

bak=$(mktemp -d)
cp -r web/pkg/. "$bak"/
trap 'cp -r "$bak"/. web/pkg/; rm -rf "$bak"' EXIT

run() {
  cp -r "$1"/. web/pkg/
  node tools/webtest/headless.mjs 2>/dev/null | sed -n 's/.*time=\([0-9.]*\)s.*/\1/p'
}

echo "A=$A"
echo "B=$B"
echo "round   A(s)    B(s)"
wins_a=0; wins_b=0
sum_a=0; sum_b=0
for i in $(seq "$R"); do
  ta=$(run "$A"); tb=$(run "$B")
  [ -n "$ta" ] && [ -n "$tb" ] || { echo "round $i: 計測失敗 (banner不達?)"; exit 1; }
  mark=$(awk -v a="$ta" -v b="$tb" 'BEGIN{print (a<b)?"A":(b<a)?"B":"-"}')
  case $mark in A) wins_a=$((wins_a+1));; B) wins_b=$((wins_b+1));; esac
  printf '%-7s %-7s %-7s %s\n' "$i" "$ta" "$tb" "$mark"
  sum_a=$(awk -v s="$sum_a" -v t="$ta" 'BEGIN{print s+t}')
  sum_b=$(awk -v s="$sum_b" -v t="$tb" 'BEGIN{print s+t}')
done
awk -v sa="$sum_a" -v sb="$sum_b" -v r="$R" -v wa="$wins_a" -v wb="$wins_b" 'BEGIN{
  ma=sa/r; mb=sb/r;
  printf "平均    %.1f    %.1f\n", ma, mb;
  printf "対の符号: A %d勝 / B %d勝 / 分け %d\n", wa, wb, r-wa-wb;
  d=(mb-ma)/ma*100;
  printf "B-A: %+.1f%% (ノイズ床±2%%の中なら裁定はワッシュ)\n", d;
}'
