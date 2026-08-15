#!/bin/sh
# gcc入りinitramfs — ミニinitramfsに gcc/binutils/musl-dev を重ねて詰め直す。
#
# initの gcc 用の細工は make-mini-initramfs.sh 側に入っている
# (/usr/libexec/gcc の有無で自動的に効く書き方なので、ミニ側は壊れない)。
#
# **games版と違って継ぎ足しではなく、1本に詰め直す。** カーネルは連結cpioも
# 展開できるが、gzipが末尾で名乗る展開後の大きさ (ISIZE) は**最後の塊の分**
# しか無い。ローダはその数字を見てRAMの要不要を判断する (initrd_ram_needed)
# ので、継ぎ足すと少なく見積もって「起動はするが中身が欠ける」を素通しする。
#
# **これは重い。** 展開後90MiBあり、initramfs方式だとRAMを256MB要求する
# (圧縮イメージと展開後の中身が同時にRAMに載るため)。`cargo run --example run`
# は initrd の大きさからRAMを自動で決めるので、そのまま起動すればよい。
#
# 使い方: tools/images/make-gcc-initramfs.sh
set -e
cd "$(dirname "$0")/../.."
[ -f images/initramfs-mini ] || { echo "images/initramfs-mini が無い (tools/images/make-mini-initramfs.sh)"; exit 1; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# 1. Alpine v3.24 x86 から依存の閉包ごと取ってくる (ダウンロード71MB)
python3 tools/images/fetch-gcc-pkgs.py "$work/pkg"

# 2. ゲストに運ぶものだけ選ぶ。**削るのは「開発者向けの荷物」だけ** で、
#    コンパイルの通り道 (cc1・cpp・as・ld・collect2・crt・ヘッダ・libc.a) は
#    全部残す。ここを削りすぎて `cannot execute 'cc1'` や
#    `undefined reference to __stack_chk_fail_local` を順に踏んだ
python3 tools/images/mkcpio.py --extract images/initramfs-mini "$work/root"
(cd "$work/pkg" && find . -depth 1 -exec cp -R {} "$work/root/" \;)
cd "$work/root"
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

# 3. 1本のcpioに詰め直す。実行ビットは mkcpio.py が元ファイルから拾う。
#    /dev/console のノードはミニ側と同じく自前で足す (無いとinitが盲目で走る)
python3 tools/images/mkcpio.py "$work/gcc.cpio" "$work/root" --console
gzip -c "$work/gcc.cpio" > images/initramfs-gcc
cp images/initramfs-gcc web/initramfs-gcc
echo "images/initramfs-gcc: $(du -h images/initramfs-gcc | cut -f1) (web/ へも複製)"
python3 - <<'PY'
import struct
d = open("images/initramfs-gcc", "rb").read()
mib = 1 << 20
print(f"  圧縮 {len(d) / mib:.1f} MiB → 展開後 {struct.unpack('<I', d[-4:])[0] / mib:.1f} MiB")
PY
