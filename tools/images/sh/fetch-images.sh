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
#     tools/images/sh/fetch-images.sh          # 全部
#     tools/images/sh/fetch-images.sh elks     # ELKSだけ
#     tools/images/sh/fetch-images.sh freedos  # FreeDOS (ゲーム入り) だけ
#
# 出来上がるもの:
#
#     images/fd2880.img     ELKS 0.9.1 (ゲームもネット一式もELKS本体が持っている)
#     images/fd14boot.img   FreeDOS 1.4 の素の起動フロッピー
#     images/fd14games.img  FreeDOS + テキストモードのゲーム
#
# ブラウザ版は web/ に置いたものを読むので、同じものをそちらへも複製する。

set -euo pipefail

# 置き場所を自分で探す。リポジトリでは tools/ の親がルートだが、
# **配布zipではスクリプトがルート直下に居る** — 決め打ちすると展開フォルダの
# 外に web/ を作ってしまう (Releaseで実際に起きた)。web/ の在り処で判定する
here=$(cd "$(dirname "$0")" && pwd)
if [ -d "$here/web" ]; then
  cd "$here"
elif [ -d "$here/../web" ]; then
  cd "$here/.."
elif [ -d "$here/../../web" ]; then
  cd "$here/../.."
elif [ -d "$here/../../../web" ]; then
  cd "$here/../../.."
else
  echo "web/ が見つからない。リポジトリのルートか、配布zipの展開先で実行する" >&2
  exit 1
fi
# 道具 (curl/unzip/mtools/nasm) はLinuxコンテナから借りる — sh/ に居るものは
# 全部この作法。ただし**このスクリプトだけはネイティブでも動ける**まま残す:
# 配布zipの利用者はdockerを持っていないことがあり、ELKS/FreeDOSの取得は
# curl+unzip+mtoolsで足りる (道具箱が要るのはイメージを「焼く」側)
if [ ! -f /.dockerenv ] && [ -f tools/images/in-linux.sh ] && command -v docker >/dev/null 2>&1; then
  exec tools/images/in-linux.sh bash "tools/images/sh/fetch-images.sh" "$@"
fi
IMAGES=images
WEB=web
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# 2.88MB版 = ELKS公式の全部入り (ゲーム + ktcp/telnet/urlget のネット一式)。
# 1.44MB版はネット系ユーザーランドが丸ごと入っていない (2026-08-14 実測)
ELKS_URL=https://github.com/ghaerr/elks/releases/download/v0.9.1/fd2880-minix.img
FREEDOS_URL=https://download.freedos.org/1.4/FD14-FloppyEdition.zip
GAMES_BASE=http://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/games
DEVEL_BASE=http://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/devel
NET_BASE=http://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/net
# Alpine の netboot 配布物 (32bit x86)。カーネルと initramfs の2つで起動できる
# 版は固定する (latest-stableは中身が動き、System.mapとカーネルの版ズレで
# プロファイルが全部ゴミになる — 2026-08-12に踏んだ)。上げるときは3点セットで
ALPINE_BASE=https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86/netboot

# 入れるゲーム。形式は「パッケージ名:イメージへ入れるファイル…」
#
# **画面は 80x25 のテキストだけ**なので、動くものと動かないものがある。
# 動かないものもあえて入れてある — **Tier 6 が要る理由の実物**になるからで、
# 欠け方の理由がそれぞれ違うのが面白い。
#
#   ELIZA   ✅ 80x25 のテキストだけで動く
#   ROW4T   ✅ 同上 (罫線はコードページ437)
#   ZMIY    ✅ 50行の盤面を描き、見える25行の窓をCRTCで蛇に追従させる
#             (ハードウェアスクロール)。**これが動かず追いかけた結果、
#             こちらが CRTC の表示開始位置を無視していたのが見つかった**
#   HANGMAN ⚠️  CGAグラフィックス (モード0x04) を要求する。Tier 6 まで動かない
GAMES=(
  "eliza:GAMES/ELIZA/ELIZA.EXE:GAMES/ELIZA/RESPONSE.DAT"
  "zmiy:GAMES/ZMIY/ZMIY.EXE"
  "row4:GAMES/ROW4/ROW4T.COM"
  "hangman:GAMES/HANGMAN/HANGMAN.EXE"
)

say() { printf '\033[36m==>\033[0m %s\n' "$*"; }

# zipから1ファイルだけ取り出す (パスは捨てる)。unzipの無いホスト (Windows) は
# Pythonの標準ライブラリで開く
unzip_one() { # zip member destdir
  if command -v unzip >/dev/null 2>&1; then
    unzip -oqj "$1" "$2" -d "$3"
  else
    python3 - "$1" "$2" "$3" <<'PY'
import os, sys, zipfile
zip_, member, dest = sys.argv[1:4]
with zipfile.ZipFile(zip_) as z, z.open(member) as s:
    out = os.path.join(dest, os.path.basename(member))
    with open(out, "wb") as d:
        d.write(s.read())
PY
  fi
}

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
  fetch "$ELKS_URL" "$IMAGES/fd2880.img"
  # /bootopts の「#net=ne0」を有効化する。minixファイルシステムはホストで
  # マウントできないが、**同じ長さのバイト置換ならfsを壊さずに済む**
  # (8文字 → 8文字。コメント記号を末尾の空白に置き換えるだけ)
  python3 - "$IMAGES/fd2880.img" <<'PY'
import sys
p = sys.argv[1]
d = open(p, 'rb').read()
patched = d.replace(b'#net=ne0', b'net=ne0 ', 1)
patched = patched.replace(b'#ne0=12,0x300,,0x80', b'ne0=3,0x300,,0x80  ', 1)  # IRQはこちらのカードの3に合わせる
if patched != d:
    open(p, 'wb').write(patched)
    print('==> /bootopts の net=ne0 を有効化した')
PY
  say "ELKS 完了 (ゲーム一式 + ktcp/telnet/urlget。net=ne0有効)"
}

build_linux() {
  mkdir -p "$IMAGES"
  # カーネルとinitramfsは**対で取る**。Alpineのnetbootは常に最新を指すので、
  # 別々の時期に取ると版がずれる — モジュール (initramfs側) のvermagicが
  # カーネルと合わず insmod が黙って失敗する (実際に 6.18/6.12 で踏んだ)。
  # 片方でも欠けていたら両方を取り直す
  if [ ! -f "$IMAGES/vmlinuz-lts" ] || [ ! -f "$IMAGES/initramfs-lts" ]; then
    rm -f "$IMAGES/vmlinuz-lts" "$IMAGES/initramfs-lts" "$IMAGES/System.map-lts"
  fi
  fetch "$ALPINE_BASE/vmlinuz-lts" "$IMAGES/vmlinuz-lts"
  fetch "$ALPINE_BASE/initramfs-lts" "$IMAGES/initramfs-lts"
  # 全モジュール (242MB)。initramfs-lts に無いモジュール (PS/2マウス等) は
  # ここから借りる。カーネルと同じ netboot の荷物なので vermagic が合う
  [ -f "$IMAGES/modloop-lts" ] || fetch "$ALPINE_BASE/modloop-lts" "$IMAGES/modloop-lts"
  # System.map (ブート解剖 bootprof 用)。ファイル名に版が入るので一覧から拾う
  # head -1 が先にパイプを閉じ、まだ書いている側がSIGPIPE (141) を受ける。
  # pipefailはそれを失敗と数える — GNU grepはバッファで隠れ、箱のbusybox
  # grepで顕在化した (CIのmainで実際に落ちた)。System.mapは無くても
  # 起動には困らない解剖用の素材なので、この行だけ失敗を飲む
  SYSMAP=$(curl -sL --max-time 30 "$ALPINE_BASE/" | grep -o 'System.map-[^"<]*' | head -1 || true)
  [ -n "$SYSMAP" ] && fetch "$ALPINE_BASE/$SYSMAP" "$IMAGES/System.map-lts"
  say "Linux (Alpine lts カーネル + initramfs + System.map) 完了"
}

build_freedos() {
  mkdir -p "$IMAGES" "$WORK/games"
  fetch "$FREEDOS_URL" "$WORK/fd14flop.zip"

  # 8086ビルドの起動フロッピー。配布zipの 144m/x86BOOT.img がそれ。
  # 同じディスクに KERNEL.SYS (8086) と KERNL386.SYS (386) の両方が入っている
  say "起動フロッピーを取り出す"
  unzip_one "$WORK/fd14flop.zip" "144m/x86BOOT.img" "$WORK"
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

  # おまけ2つ。どちらも無くても起動には困らないので、取れなければ諦める
  # - AIR.COM: PCスピーカーでG線上のアリアを演奏する自作デモ
  #   (tools/guest/air/。生成物をコミットしてあるので nasm 不要)
  # - DEBUG.COM: FreeDOS公式の lDebug。起動フロッピーにはデバッガが
  #   入っていないので、DOSの中から AIR.COM を逆アセンブルできるように载せる
  [ -f tools/guest/air/AIR.COM ] && files+=(tools/guest/air/AIR.COM)
  # - BOUNCE.COM: mode 13h でボールが跳ねる自作デモ (tools/guest/bounce/)。
  #   垂直帰線待ち・DAC・0xA0000直書きと、Tier 6a の部品が全部通る
  [ -f tools/guest/bounce/BOUNCE.COM ] && files+=(tools/guest/bounce/BOUNCE.COM)
  if fetch "$DEVEL_BASE/ldebug.zip" "$WORK/ldebug.zip"; then
    unzip_one "$WORK/ldebug.zip" "BIN/ldebug.com" "$WORK"
    cp "$WORK/ldebug.com" "$WORK/DEBUG.COM"
    files+=("$WORK/DEBUG.COM")
  fi

  # ネットワーク道具 (ADR-0017)。仮想NE2000 + wsslirp で16bitからpingを打つ:
  #   A:\> NE2000 0x60 3 0x300        (パケットドライバ: INT 60h, IRQ3, 0x300)
  #   A:\> SET MTCPCFG=A:\MTCP.CFG
  #   A:\> DHCP                        (wsslirpから 10.0.2.15 をもらう)
  #   A:\> PING 1.1.1.1
  say "ネットワーク道具 (Crynwrパケットドライバ + mTCP) を載せる"
  fetch "$NET_BASE/crynwr.zip" "$WORK/crynwr.zip"
  unzip_one "$WORK/crynwr.zip" "DRIVERS/CRYNWR/NE2000.COM" "$WORK"
  files+=("$WORK/NE2000.COM")
  fetch "$NET_BASE/mtcp.zip" "$WORK/mtcp.zip"
  for exe in ping.exe dhcp.exe htget.exe; do
    unzip_one "$WORK/mtcp.zip" "NET/mTCP/$exe" "$WORK"
    files+=("$WORK/$exe")
  done
  # mTCPの設定ファイル。PACKETINTだけ書いておけば、残り (IP/DNS/GW) は
  # DHCP.EXE が wsslirp からもらってこのファイルに書き足す
  printf 'PACKETINT 0x60\r\nHOSTNAME RUSTX86\r\n' > "$WORK/MTCP.CFG"
  files+=("$WORK/MTCP.CFG")

  cp "$IMAGES/fd14boot.img" "$IMAGES/fd14games.img"
  copy_into_image "$IMAGES/fd14games.img" "${files[@]}"
  say "FreeDOS 完了 (eliza / zmiy / row4t / hangman + AIR / BOUNCE / DEBUG + mTCP)"
}

publish_to_web() {
  mkdir -p "$WEB"
  [ -f "$IMAGES/fd2880.img" ] && cp "$IMAGES/fd2880.img" "$WEB/fd2880.img"
  # ブラウザ版はゲーム入りの方を使う。**素の起動ディスクだと
  # プロンプトに着いても何も入っていない**ので、それでは動かないのと同じである
  [ -f "$IMAGES/fd14games.img" ] && cp "$IMAGES/fd14games.img" "$WEB/fd14boot.img"
  [ -f "$IMAGES/vmlinuz-lts" ] && cp "$IMAGES/vmlinuz-lts" "$WEB/vmlinuz-lts"
  [ -f "$IMAGES/initramfs-lts" ] && cp "$IMAGES/initramfs-lts" "$WEB/initramfs-lts"
  # ISO 起動の実物 (Tiny Core)。ライブラリは居れば並べる (probe)
  [ -f "$IMAGES/Core-current.iso" ] && cp "$IMAGES/Core-current.iso" "$WEB/Core-current.iso"
  say "web/ へ複製した"
}

# 起動フロッピーだけ (ゲーム無し・mtools不要)。CIの起動回帰が使う —
# プロンプト到達の確認にゲームは要らず、ibiblioへの依存も減らせる
build_freedos_boot() {
  mkdir -p "$IMAGES"
  fetch "$FREEDOS_URL" "$WORK/fd14flop.zip"
  say "起動フロッピーを取り出す (ゲーム無し)"
  unzip_one "$WORK/fd14flop.zip" "144m/x86BOOT.img" "$WORK"
  cp "$WORK/x86BOOT.img" "$IMAGES/fd14boot.img"
}


# test386.asm — CPU互換テストROM (互換ピラミッドL1、GPLv3)。
# ソースをpinしたコミットで取り、nasmでROMを焼く (COM1へASCII出力する構成)。
# バイナリを配らないのはOSイメージと同じ理由 — ビルド経路がここに一意に決まる
TEST386_COMMIT="master"   # 導入時点のHEAD。壊れたら実測で選び直す
build_test386() {
  command -v nasm >/dev/null || { echo "nasm が要る (brew install nasm / apt-get install nasm)" >&2; exit 1; }
  mkdir -p "$IMAGES"
  fetch "https://github.com/barotto/test386.asm/archive/refs/heads/$TEST386_COMMIT.tar.gz" "$WORK/test386-src.tar.gz"
  say "test386.asm をビルドする (COM1出力構成)"
  rm -rf "$WORK/test386-src"
  mkdir -p "$WORK/test386-src"
  tar xzf "$WORK/test386-src.tar.gz" -C "$WORK/test386-src" --strip-components=1
  ( cd "$WORK/test386-src" \
    && sed -i.bak 's/^COM_PORT equ 0$/COM_PORT equ 1/' src/configuration.asm \
    && nasm -i./src/ -f bin src/test386.asm -w-all -o test386.bin )
  cp "$WORK/test386-src/test386.bin" "$IMAGES/test386.bin"
  cp "$WORK/test386-src/test386-EE-reference.txt" "$IMAGES/test386-EE-reference.txt"
}

# DOS 版 DOOM (shareware 1.9、再配布自由) を BIOS のハードディスク像 (C:) に。
# idgames の doom19s.zip は DEICE の分割 (DOOMS_19.1/.2) で、結合すると PKZIP の
# 自己解凍書庫 — unzip で開ける。DOOM.EXE は DOS/4GW 同梱、DOOM1.WAD は 4.2MB
DOOM_URL=https://www.gamers.org/pub/idgames/idstuff/doom/doom19s.zip
build_doom() {
  say "DOOM shareware 1.9 (DOS 版) → C: の像"
  mkdir -p "$WORK/doom"
  fetch "$DOOM_URL" "$WORK/doom/doom19s.zip"
  ( cd "$WORK/doom" && unzip -oq doom19s.zip \
    && cat DOOMS_19.1 DOOMS_19.2 > DOOMS_19.EXE \
    && mkdir -p unpacked && cd unpacked && unzip -oq ../DOOMS_19.EXE )
  sh tools/images/sh/make-doom-hdd.sh "$WORK/doom/unpacked"
}

# Tiny Core Linux (x86、20MB の ISO)。ISO 起動 (6c) の実物の当て先 — isolinux 4.05 から
# 本物の Linux が上がり tc@box: に着く。そのまま images/ に置くだけ (焼き直しは無い)
TINYCORE_URL=http://tinycorelinux.net/16.x/x86/release/Core-current.iso
build_tinycore() {
  say "Tiny Core Linux の ISO"
  fetch "$TINYCORE_URL" "$IMAGES/Core-current.iso"
}

case "${1:-all}" in
  elks) build_elks ;;
  doom) build_doom ;;
  tinycore) build_tinycore ;;
  freedos) build_freedos ;;
  freedos-boot) build_freedos_boot ;;
  linux) build_linux ;;
  test386) build_test386 ;;
  all) build_elks; build_freedos; build_linux ;;
  *) echo "使い方: $0 [all|elks|freedos|freedos-boot|linux|test386|doom|tinycore]" >&2; exit 1 ;;
esac
publish_to_web

say "完了。python3 web/serve.py で開ける"
