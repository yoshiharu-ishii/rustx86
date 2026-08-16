#!/bin/sh
# initramfs にゲームを同梱した派生版を作る。
#
# カーネルの initramfs は**連結された cpio をぜんぶ展開する**仕様なので、
# 元の initramfs-lts (gzip圧縮cpio) の後ろに、自前の cpio を繋げるだけでよい。
# 元のファイルは触らない。
#
# 使い方: tools/images/sh/make-games-initramfs.sh
set -e
cd "$(dirname "$0")/../../.."
# イメージ焼きは道具箱 (Linuxコンテナ) の中で — sh/ に居るものは全部この作法
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
[ -f images/initramfs-lts ] || { echo "images/initramfs-lts が無い (tools/images/sh/fetch-images.sh linux)"; exit 1; }
[ -f tools/guest/snake ] || { echo "tools/guest/snake が無い (先にビルドする)"; exit 1; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root/bin"
cp tools/guest/snake "$work/root/bin/snake"
chmod 755 "$work/root/bin/snake"
(cd "$work/root" && find . | cpio -o -H newc --quiet | gzip) > "$work/extra.cpio.gz"
cat images/initramfs-lts "$work/extra.cpio.gz" > images/initramfs-games
echo "images/initramfs-games: $(du -h images/initramfs-games | cut -f1) (snake入り)"
