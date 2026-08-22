#!/bin/sh
# 1MB 超のファイルがリポジトリに入っていないかを見る門番。
#
#   tools/build/check-large-files.sh                  # HEAD のツリーだけ
#   tools/build/check-large-files.sh origin/main..HEAD  # + その範囲のコミットが持ち込む blob
#   LIMIT=500000 tools/build/check-large-files.sh     # 閾値 (バイト、既定 1,000,000)
#
# なぜあるか (2026-08-22): FreeDOS + DOOM のディスクイメージ (4.5MB、GPL/シェアウェア) が
# public のこのリポジトリに commit され、履歴の書き換え (filter-repo + 保護の外し戻し) で
# 消す羽目になった。.gitignore は「置かない約束」でしかなく、名指しの穴から漏れた。
# これは「置けない仕組み」— CI の必須チェックと pre-commit の両方から同じ判定を呼ぶ。
# push ruleset (サーバ側の大きさ制限) は Organization 配下限定で、個人の public には付かない。
#
# 見るのは 2 つ:
#   1. HEAD のツリー (今あるもの)
#   2. 範囲を渡されたら、その範囲のコミットが到達させる blob (入れてすぐ消しても履歴に残る)
# 像 (ディスクイメージ・initramfs・ISO) は rustx86-images と fetch-images.sh が受け持つ。
set -eu
LIMIT=${LIMIT:-1000000}
RANGE=${1:-}

found=$(
  {
    # ls-tree -l: <mode> <type> <sha> <size>\t<path>
    git ls-tree -r -l HEAD | awk -v l="$LIMIT" '$4+0 > l { $1=$2=$3=""; sub(/^ +/, ""); print }'
    if [ -n "$RANGE" ]; then
      git rev-list --objects "$RANGE" \
        | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' \
        | awk -v l="$LIMIT" '$1=="blob" && $2+0 > l { $1=""; sub(/^ +/, ""); print }'
    fi
  } | sort -u
)

if [ -z "$found" ]; then
  echo "1MB 超のファイルは無い (閾値 ${LIMIT} バイト${RANGE:+、範囲 $RANGE})"
  exit 0
fi
echo "閾値 ${LIMIT} バイトを超えるファイルがある${RANGE:+ (HEAD または $RANGE)}:"
echo "$found" | awk '{ printf "  %10d  %s\n", $1, substr($0, index($0, $2)) }'
echo
echo "像 (ディスクイメージ等) は rustx86-images に置き、tools/images/sh/fetch-images.sh で web/ に取る。"
echo "既に commit していたら、push 前なら git reset で落とす。push 済みなら履歴の書き換えが要る"
echo "(docs/reference/ci.md「大物の門番」)。"
exit 1
