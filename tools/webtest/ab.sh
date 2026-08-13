#!/bin/bash
# wasmの交互A/B — ネイティブの交互A/Bと同じ形式・同じ流儀の定規。
#
# 2つの pkg ディレクトリ (wasm-pack の出力一式) を交互に headless.mjs で
# 走らせ、**時間の差だけ**を見る。出力はネイティブの定規と同じ
#   round1 main: 18.3s / feat: 18.1s
# の形 (ラベルはディレクトリ名)。単発の速さは熱ダレの運として捨て、
# 対の符号と平均で読む。
#
#   tools/webtest/ab.sh <pkgA(main)> <pkgB(開発)> [rounds=5]
#
# 例: 現行mainのビルドを控えてから候補ビルドと比べる
#   bash tools/build-web.sh && cp -r web/pkg /tmp/pkg_main
#   (候補をビルド)          && cp -r web/pkg /tmp/pkg_dev
#   tools/webtest/ab.sh /tmp/pkg_main /tmp/pkg_dev
#
# web/pkg は退避して終了時に必ず戻す。
set -u
cd "$(dirname "$0")/../.."
A=${1:?使い方: ab.sh <pkgA(main)> <pkgB(開発)> [rounds]}
B=${2:?使い方: ab.sh <pkgA(main)> <pkgB(開発)> [rounds]}
R=${3:-5}
LA=$(basename "$A"); LB=$(basename "$B")

bak=$(mktemp -d)
cp -r web/pkg/. "$bak"/
trap 'cp -r "$bak"/. web/pkg/; rm -rf "$bak"' EXIT

run() {
  cp -r "$1"/. web/pkg/
  node tools/webtest/headless.mjs 2>/dev/null | sed -n 's/.*time=\([0-9.]*\)s.*/\1/p'
}

wins_a=0; wins_b=0
sum_a=0; sum_b=0
for i in $(seq "$R"); do
  ta=$(run "$A"); tb=$(run "$B")
  [ -n "$ta" ] && [ -n "$tb" ] || { echo "round$i: 計測失敗 (banner不達?)"; exit 1; }
  echo "round$i $LA: ${ta}s / $LB: ${tb}s"
  w=$(awk -v a="$ta" -v b="$tb" 'BEGIN{print (a<b)?"A":(b<a)?"B":"-"}')
  case $w in A) wins_a=$((wins_a+1));; B) wins_b=$((wins_b+1));; esac
  sum_a=$(awk -v s="$sum_a" -v t="$ta" 'BEGIN{print s+t}')
  sum_b=$(awk -v s="$sum_b" -v t="$tb" 'BEGIN{print s+t}')
done
awk -v sa="$sum_a" -v sb="$sum_b" -v r="$R" -v wa="$wins_a" -v wb="$wins_b" \
    -v la="$LA" -v lb="$LB" 'BEGIN{
  ma=sa/r; mb=sb/r;
  printf "平均   %s: %.1fs / %s: %.1fs  (%s-%s %+.1f%%、符号 %d勝%d敗%d分)\n",
         la, ma, lb, mb, lb, la, (mb-ma)/ma*100, wa, wb, r-wa-wb;
}'
