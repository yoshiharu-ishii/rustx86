#!/usr/bin/env bash
#
# ディスクイメージを取ってきて組み立てる。
#
# ## なぜリポジトリに置かないのか
#
# ELKS も FreeDOS もゲームも自由ソフトウェアなので、**再配布は許されている**。
# 禁止されているから置かないのではない。理由は2つある。
#
# - GPLのバイナリ配布には**ソースの提供義務**が付く。守れない話ではないが、
#   エミュレータの教材が他人のOSの再頒布者になると、その責任が恒久的に付いて回る
# - **手で組み立てたイメージは再現できない。** 実際、手元でゲームを載せた
#   イメージと、他の人が持っているイメージの中身が食い違って
#   「アプリが入っていない」が起きた。**リポジトリに置いても、
#   置き忘れれば同じことが起きる。**スクリプトなら中身が一意に決まる
#
# ## 使い方
#
#     tools/fetch-images.sh          # 全部
#     tools/fetch-images.sh elks     # ELKSだけ
#     tools/fetch-images.sh freedos  # FreeDOS (ゲーム入り) だけ
#
# 出来上がるもの:
#
#     images/fd1440.img     ELKS 0.9.1 (ゲーム同梱。ELKS本体が持っている)
#     images/fd14boot.img   FreeDOS 1.4 の素の起動フロッピー
#     images/fd14games.img  FreeDOS + テキストモードのゲーム
#
# ブラウザ版は web/ に置いたものを読むので、同じものをそちらへも複製する。

set -euo pipefail

cd "$(dirname "$0")/.."
IMAGES=images
WEB=web
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

ELKS_URL=https://github.com/ghaerr/elks/releases/download/v0.9.1/fd1440.img
FREEDOS_URL=https://download.freedos.org/1.4/FD14-FloppyEdition.zip
GAMES_BASE=http://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/games

# 入れるゲーム。形式は「パッケージ名:イメージへ入れるファイル…」
#
# **画面は 80x25 のテキストだけ**なので、動くものと動かないものがある。
# 動かないものもあえて入れてある — **Tier 6 が要る理由の実物**になるからで、
# 欠け方の理由がそれぞれ違うのが面白い。
#
#   ELIZA   ✅ 80x25 のテキストだけで動く
#   ROW4T   ✅ 同上 (罫線はコードページ437)
#   ZMIY    ⚠️  80x50 を前提に描く。VGAの50行テキストが要る (下半分が画面外)
#   HANGMAN ⚠️  CGAグラフィックス (モード0x04) を要求する (何も出ない)
#
# ZMIY は `INT 10h AH=1A` で表示装置を尋ねてくるので非VGAと答えているが、
# それでも50行で描く。**1本のアプリのためにBIOSの細部を追い続けるのが
# 「底が無い」側の仕事**なので、ここで止めてある (ADR-0004)。
GAMES=(
  "eliza:GAMES/ELIZA/ELIZA.EXE:GAMES/ELIZA/RESPONSE.DAT"
  "zmiy:GAMES/ZMIY/ZMIY.EXE"
  "row4:GAMES/ROW4/ROW4T.COM"
  "hangman:GAMES/HANGMAN/HANGMAN.EXE"
)

say() { printf '\033[36m==>\033[0m %s\n' "$*"; }

fetch() { # url dest
  [ -f "$2" ] && { say "$(basename "$2") は取得済み"; return; }
  say "取得: $(basename "$2")"
  curl -fsSL --retry 3 -o "$2" "$1"
}

# イメージにファイルを入れる。macOS は hdiutil、Linux は mtools を使う
copy_into_image() { # image file...
  local img=$1; shift
  if command -v hdiutil >/dev/null 2>&1; then
    local dev vol
    dev=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nobrowse "$img" | awk 'NR==1{print $1}')
    vol=$(hdiutil info | awk -v d="$dev" '$0 ~ d {found=1} found && /\/Volumes\//{print substr($0, index($0, "/Volumes/")); exit}')
    cp "$@" "$vol/"
    rm -f "$vol"/._*        # macOS が置くメタデータ。DOSからは邪魔なだけ
    hdiutil detach "$dev" -quiet
  elif command -v mcopy >/dev/null 2>&1; then
    MTOOLS_SKIP_CHECK=1 mcopy -o -i "$img" "$@" ::/
  else
    echo "エラー: hdiutil (macOS) か mtools (Linux) が要る" >&2
    echo "  Debian/Ubuntu: sudo apt install mtools" >&2
    exit 1
  fi
}

build_elks() {
  mkdir -p "$IMAGES"
  fetch "$ELKS_URL" "$IMAGES/fd1440.img"
  say "ELKS 完了 (tetris / invaders / ttypong / sl / matrix が最初から入っている)"
}

build_freedos() {
  mkdir -p "$IMAGES" "$WORK/games"
  fetch "$FREEDOS_URL" "$WORK/fd14flop.zip"

  # 8086ビルドの起動フロッピー。配布zipの 144m/x86BOOT.img がそれ。
  # 同じディスクに KERNEL.SYS (8086) と KERNL386.SYS (386) の両方が入っている
  say "起動フロッピーを取り出す"
  unzip -oqj "$WORK/fd14flop.zip" "144m/x86BOOT.img" -d "$WORK"
  cp "$WORK/x86BOOT.img" "$IMAGES/fd14boot.img"

  say "ゲームを取得して載せる"
  local files=()
  for entry in "${GAMES[@]}"; do
    IFS=: read -r pkg paths <<<"${entry%%:*}:${entry#*:}"
    fetch "$GAMES_BASE/$pkg.zip" "$WORK/$pkg.zip"
    unzip -oq "$WORK/$pkg.zip" -d "$WORK/games"
    IFS=: read -ra list <<<"$paths"
    for p in "${list[@]}"; do files+=("$WORK/games/$p"); done
  done

  cp "$IMAGES/fd14boot.img" "$IMAGES/fd14games.img"
  copy_into_image "$IMAGES/fd14games.img" "${files[@]}"
  say "FreeDOS 完了 (eliza / zmiy / row4t / hangman)"
}

publish_to_web() {
  mkdir -p "$WEB"
  [ -f "$IMAGES/fd1440.img" ] && cp "$IMAGES/fd1440.img" "$WEB/fd1440.img"
  # ブラウザ版はゲーム入りの方を使う。**素の起動ディスクだと
  # プロンプトに着いても何も入っていない**ので、それでは動かないのと同じである
  [ -f "$IMAGES/fd14games.img" ] && cp "$IMAGES/fd14games.img" "$WEB/fd14boot.img"
  say "web/ へ複製した"
}

case "${1:-all}" in
  elks) build_elks ;;
  freedos) build_freedos ;;
  all) build_elks; build_freedos ;;
  *) echo "使い方: $0 [all|elks|freedos]" >&2; exit 1 ;;
esac
publish_to_web

say "完了。python3 web/serve.py で開ける"
