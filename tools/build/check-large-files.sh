#!/bin/sh
# 持ち込み物の門番: 「大きいファイル」と「著作物になりそうな拡張子」を入れさせない。
#
#   tools/build/check-large-files.sh                    # HEAD のツリー
#   tools/build/check-large-files.sh origin/main..HEAD  # + その範囲のコミットが持ち込む blob
#   tools/build/check-large-files.sh --staged           # staged (pre-commit から)
#   LIMIT=500000 tools/build/check-large-files.sh       # 容量の閾値 (バイト、既定 1,000,000)
#
# なぜあるか (2026-08-22): FreeDOS + DOOM のディスクイメージ (4.5MB、GPL/シェアウェア) が
# public のこのリポジトリに commit され、履歴の書き換え (filter-repo + 保護の外し戻し) で
# 消す羽目になった。.gitignore は「置かない約束」でしかなく、名指しの穴から漏れた。
# これは「置けない仕組み」— CI の必須チェックと pre-commit の両方から同じ判定を呼ぶ。
# push ruleset (サーバ側の大きさ制限) は Organization 配下限定で、個人の public には付かない。
#
# 2 つの網 (拾うものが違うので両方):
#   1. 容量: 名前に関係なく 1MB 超 (像の類いは全部ここに掛かる)
#   2. 拡張子: 小さくても形から怪しいもの (.COM/.EXE/.ROM/.WAD/音/書庫/…)。
#      ただし**隣に同じ名前のソース** (.asm/.c/.rs/…) があれば自作物とみなして通す
#      (asm/hello.bin ↔ asm/hello.asm、tools/guest/air/AIR.COM ↔ air.asm)。出自が
#      リポジトリの中にある binary だけが住める
# 像 (ディスクイメージ・initramfs・ISO) は rustx86-images と fetch-images.sh が受け持つ。
set -eu
LIMIT=${LIMIT:-1000000}
MODE=${1:-}
# 著作物が入りやすい拡張子 (小文字で比べる)。テキスト・画像 (png/svg) は対象外
EXT_RE='\.(img|ima|iso|bin|rom|com|exe|wad|dll|sys|drv|ovl|zip|gz|tgz|xz|bz2|7z|rar|lha|lzh|cab|arj|mp3|wav|ogg|flac|mid|mod|xm|s3m|pdf|dat|snap|rx86snap|a|so|o|ko|elf|efi|dmg|vhd|vmdk|qcow2|flp|dsk|d64|t64|nes|sfc|smc|gba|gb|gbc|z80|tap|tzx|adf|hdf|ipf|ttf|otf|fon|pcf|pyc|pyo|class|jar)$'
SRC_RE='\.(asm|s|c|cc|cpp|rs|go|py|ly|nasm)$'

# 候補を「<size> <path>」で集める (case を $( ) の中に書くと sh が ")" で迷うので関数に)
head_tree() {
  git ls-tree -r -l HEAD | awk '{ sz=$4; $1=$2=$3=$4=""; sub(/^ +/, ""); print sz, $0 }'
}
collect() {
  if [ "$MODE" = --staged ]; then
    git diff --cached --name-only --diff-filter=AM | while IFS= read -r f; do
      printf '%s %s\n' "$(git cat-file -s ":$f" 2>/dev/null || echo 0)" "$f"
    done
  elif [ -z "$MODE" ]; then
    head_tree
  else
    {
      head_tree
      git rev-list --objects "$MODE" \
        | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' \
        | awk '$1=="blob" && NF>=3 { $1=""; sub(/^ +/, ""); print }'
    } | sort -u -k2
  fi
}
candidates=$(collect)

# 隣に同名のソースがあるか (HEAD のツリーで見る。staged 中のソースも数える)
tree=$( { git ls-tree -r --name-only HEAD; [ "$MODE" = --staged ] && git diff --cached --name-only; } 2>/dev/null | sort -u )
has_source() {
  dir=$(dirname "$1"); stem=$(basename "$1" | sed -E 's/\.[^.]+$//' | tr 'A-Z' 'a-z')
  printf '%s\n' "$tree" | tr 'A-Z' 'a-z' | grep -qiE "^$(printf '%s' "$dir" | sed 's/[.[\*^$]/\\&/g')/$(printf '%s' "$stem" | sed 's/[.[\*^$]/\\&/g')$SRC_RE"
}

NL='
'
big=""; ext=""
while IFS= read -r line; do
  [ -z "$line" ] && continue
  sz=${line%% *}; path=${line#* }
  if [ "$sz" -gt "$LIMIT" ] 2>/dev/null; then big="$big$(printf '%10d  %s' "$sz" "$path")$NL"; fi
  if printf '%s' "$path" | grep -qiE "$EXT_RE" && ! has_source "$path"; then ext="$ext$(printf '%10d  %s' "$sz" "$path")$NL"; fi
done <<EOF
$candidates
EOF

label="HEAD"; [ "$MODE" = --staged ] && label="staged"; [ -n "$MODE" ] && [ "$MODE" != --staged ] && label="HEAD または $MODE"
if [ -z "$big" ] && [ -z "$ext" ]; then
  echo "持ち込み物は無い (容量 ${LIMIT} バイト超 / 出自不明の binary 拡張子、$label)"
  exit 0
fi
if [ -n "$big" ]; then
  echo "容量 ${LIMIT} バイトを超えるファイル ($label):"; printf '%s' "$big"; echo
fi
if [ -n "$ext" ]; then
  echo "著作物になりそうな拡張子で、隣に同名のソース (.asm/.c/.rs/…) が無いファイル ($label):"; printf '%s' "$ext"; echo
fi
echo "像 (ディスクイメージ等) は rustx86-images に置き、tools/images/sh/fetch-images.sh で web/ に取る。"
echo "自作の binary なら同じ名前のソースを隣に置く (出自がリポジトリの中にあるものだけ通す)。"
echo "push 済みなら履歴の書き換えが要る (docs/reference/ci.md「持ち込み物の門番」)。"
exit 1
