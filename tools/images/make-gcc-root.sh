#!/bin/sh
# gcc入りrootfsの木を組む (共有部品)。
#
#   tools/images/make-gcc-root.sh <出力dir>
#
# ミニinitramfsの中身に Alpine の gcc/binutils/musl-dev を重ね、
# 開発者向けの荷物を削って「Cが1本通る最小の木」を作る。
# **詰め方は呼ぶ側の仕事**: initramfs (cpio+gz) にするのも、
# ディスク (squashfs) にするのも、この木が同じ出発点になる。
set -e
cd "$(dirname "$0")/../.."
# イメージ焼きは道具箱 (Linuxコンテナ) の中で (make-mini-initramfs.shと同じ判断)
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
[ -f images/initramfs-mini ] || { echo "images/initramfs-mini が無い (tools/images/make-mini-initramfs.sh)"; exit 1; }
root=$1
[ -n "$root" ] || { echo "使い方: make-gcc-root.sh <出力dir>"; exit 1; }
# 以降 cd するので、出力先は絶対パスに直しておく
case "$root" in /*) ;; *) root="$PWD/$root" ;; esac

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# 1. Alpine v3.24 x86 の gcc 一式を**apk本人に**引かせる。依存の閉包も署名も
#    apkの仕事で、こちらはパッケージ名を3つ言うだけ (以前はAPKINDEXを自前で
#    読むPythonが100行あった — 道具箱に本物が居るなら借りる)。
#    --arch x86: コンテナ (arm64/x86_64) と違う石のrootfsを組むための指定。
#    --no-scripts: post-installはx86バイナリの実行なので、ここでは走らせない
# --keys-dir: apkは鍵輪を**組み立て先root側**から読む (--rootの罠)。
# 空のrootに鍵は無いので、道具箱の鍵輪 (x86の鍵は焼き込み済み) を指す
apk --arch x86 --root "$work/pkg" --initdb -U --no-scripts \
  --keys-dir /etc/apk/keys \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/main \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/community \
  add gcc musl-dev binutils
# apkの帳簿とキャッシュはゲストに運ばない (apkの無い世界なので意味を持たない)
rm -rf "$work/pkg/lib/apk" "$work/pkg/var" "$work/pkg/etc/apk" "$work/pkg/dev"

# 2. ゲストに運ぶものだけ選ぶ。**削るのは「開発者向けの荷物」だけ** で、
#    コンパイルの通り道 (cc1・cpp・as・ld・collect2・crt・ヘッダ・libc.a) は
#    全部残す。ここを削りすぎて `cannot execute 'cc1'` や
#    `undefined reference to __stack_chk_fail_local` を順に踏んだ
mkdir -p "$root"
(cd "$root" && gunzip -c "$OLDPWD/images/initramfs-mini" | cpio -idm --quiet 2>/dev/null)
(cd "$work/pkg" && cp -a . "$root/")
cd "$root"
# GCCプラグイン開発用のヘッダ (479ファイル・使わない)
rm -rf usr/lib/gcc/*/*/plugin usr/libexec/gcc/*/*/plugin usr/lib/bfd-plugins
# インストーラの道具 (fixincludes など。ビルド済みなので出番が無い)
rm -rf usr/libexec/gcc/*/*/install-tools
# ldの既定スクリプトは実行ファイルに内蔵されている。外のは -T で明示したい人向け
rm -rf usr/*-alpine-linux-musl/lib/ldscripts
# LTOと別言語 (C++ のモジュールサーバ)。C を1本通すのに要らない
rm -f usr/libexec/gcc/*/*/lto1 usr/libexec/gcc/*/*/lto-wrapper usr/libexec/gcc/*/*/g++-mapper-server
rm -f usr/lib/libcc1.so* usr/lib/libgomp.so* usr/lib/libitm.so*
# /usr/bin に居る binutils 一式は重い (1本5MB前後・libbfdを抱えている)。
# **gccが実際に起動するのは三つ組ディレクトリ側** (/usr/i586-alpine-linux-musl/bin)
# なので、そちらは丸ごと残し、こちらは gcc が名前で呼ぶ分だけ残す
for f in usr/bin/*; do
  case "${f##*/}" in
    gcc|cpp|as|ld|ld.bfd) ;;
    *) rm -f "$f" ;;
  esac
done
# 動くことの確認用。`gcc hello.c && ./a.out` がそのまま打てる
cat > hello.c <<'HELLO'
#include <stdio.h>

int main(void) {
    printf("hello, world\n");
    return 0;
}
HELLO
cd - >/dev/null
echo "gccの木: $root ($(du -sh "$root" | cut -f1))"
