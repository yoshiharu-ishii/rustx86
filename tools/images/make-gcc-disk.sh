#!/bin/sh
# gcc入りディスク — 共有の木 (make-gcc-root.sh) をsquashfsに焼く。
#
#   tools/images/make-gcc-disk.sh
#   DISK=images/disk-gcc.img INITRD=images/initramfs-mini cargo run --release --example run -- images/vmlinuz-lts
#
# initramfs版 (make-gcc-initramfs.sh) との違いは**RAMの食い方**:
# initramfsは展開した中身が全部tmpfsに載る (gcc入りは256MB要る) が、
# ディスクは読んだ分しかページキャッシュに載らないので**128MBで済む**。
# ミニinitramfsのinitがvdaを見つけ、tmpfsを上に重ねて (overlay) 移り住む。
#
# squashfsを選んだ理由: rootfsは読み専用が正しい姿 (Live CDと同じ)。
# 書ける層はoverlayのtmpfsが持つので、ゲストからは普通に書ける。
# 圧縮はgzip — ゲストのカーネル (Alpine lts) が確実に読める方式。
set -e
cd "$(dirname "$0")/../.."
# 木は**リポジトリの中**に組む — mksquashfsはコンテナ越しで、コンテナには
# リポジトリしか見えていない (/var/folders のmktempは向こうに存在しない。
# 「Cannot stat source directory」で実際に踏んだ)
work=$PWD/.imgbuild.$$; trap 'rm -rf "$work"' EXIT
mkdir -p "$work"
sh tools/images/make-gcc-root.sh "$work/root"

# ディスクから起きたことの印。initはこれを見て「もう移り住んだ」と知る
# (無いと、ディスクの中のinitがまたディスクを探しに行って輪になる)
touch "$work/root/.rustx86-disk"

# mksquashfsはLinuxの道具なのでコンテナから借りる (tools/images/in-linux.sh)。
# -all-root: 所有者を全部rootに (macOSのuidを持ち込まない)
tools/images/in-linux.sh mksquashfs ".imgbuild.$$/root" images/disk-gcc.img \
  -comp gzip -all-root -no-xattrs -noappend -quiet
echo "images/disk-gcc.img: $(du -h images/disk-gcc.img | cut -f1) (squashfs)"
