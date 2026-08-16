#!/bin/sh
# gcc入りディスク — 共有の木 (make-gcc-root.sh) をsquashfsに焼く。
#
#   tools/images/sh/make-gcc-disk.sh
#   DISK=images/disk-gcc.img INITRD=images/initramfs-mini cargo run --release --example run -- images/vmlinuz-lts
#
# initramfs版 (make-gcc-initramfs.sh) との違いは**RAMの食い方**:
# initramfsは展開した中身が全部tmpfsに載る (gcc入りは256MB要る) が、
# ディスクは読んだ分しかページキャッシュに載らないので**128MBで済む**。
# ミニinitramfsのinitがvdaを見つけ、tmpfsを上に重ねて (overlay) 移り住む。
#
# squashfsを選んだ理由: rootfsは読み専用が正しい姿 (Live CDと同じ)。
# 書ける層はoverlayのtmpfsが持つので、ゲストからは普通に書ける。
#
# **圧縮しない。** 圧縮はゲストのFSではなく輸送路の仕事 — squashfsを圧縮すると
# ゲストの (エミュレートされた) CPUが読むたびに解凍する。実測 (cc1 45MBの
# 冷read / gcc hello.c):
#   gzip 35MB: sys 15.6s / 8.1s   zstd 32MB: 16.3s / 8.4s
#   lz4  41MB:     3.3s / 3.6s   無圧縮 88MB: 0.91s / 2.8s ← 採用
# ブラウザへの配布は .gz で運び、**ホスト側で1回だけ**解凍する
# (DecompressionStream — ネイティブ速度)。転送34MB・実行2.8sの両取り。
set -e
cd "$(dirname "$0")/../../.."
# イメージ焼きは道具箱 (Linuxコンテナ) の中で。スクリプトごと中に入るので、
# mktempもmksquashfsも同じ世界に居る (以前はmksquashfsだけコンテナ越しで、
# ホストのmktempが向こうから見えず「Cannot stat source directory」を踏んだ)
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
sh tools/images/sh/make-gcc-root.sh "$work/root"

# ディスクから起きたことの印。initはこれを見て「もう移り住んだ」と知る
# (無いと、ディスクの中のinitがまたディスクを探しに行って輪になる)
touch "$work/root/.rustx86-disk"

# -all-root: 所有者を全部rootに (ホストのuidを持ち込まない)
mksquashfs "$work/root" images/disk-gcc.img \
  -noI -noD -noF -noX -all-root -no-xattrs -noappend -quiet
# ブラウザ版は .gz で配る (輸送路の圧縮。ホスト側で解いてからvdaに挿す)
gzip -9 -c images/disk-gcc.img > web/disk-gcc.img.gz
rm -f web/disk-gcc.img
echo "images/disk-gcc.img: $(du -h images/disk-gcc.img | cut -f1) (無圧縮squashfs) / web/disk-gcc.img.gz: $(du -h web/disk-gcc.img.gz | cut -f1)"
