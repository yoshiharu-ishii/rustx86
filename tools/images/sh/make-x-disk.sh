#!/bin/sh
# X入りディスク — 共有の木 (make-x-root.sh) をsquashfsに焼く。
#
#   tools/images/sh/make-x-disk.sh
#   LFB=1 DISK=images/disk-x.img INITRD=images/initramfs-mini CMDLINE='console=ttyS0 console=tty0' \
#     cargo run --release --example run -- images/vmlinuz-lts
#
# gcc ディスク (make-gcc-disk.sh) と同じ立て付け: 無圧縮 squashfs (ゲストのCPUに
# 解凍させない)、ブラウザへは .gz で運んでホスト側で1回だけ解く。
# ミニinitramfsのinitがvdaを見つけ、tmpfsを上に重ねて (overlay) 移り住む。
set -e
cd "$(dirname "$0")/../../.."
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
sh tools/images/sh/make-x-root.sh "$work/root"
# ディスクから起きたことの印 (無いと、ディスクの中のinitがまたディスクを探して輪になる)
touch "$work/root/.rustx86-disk"
mksquashfs "$work/root" images/disk-x.img \
  -noI -noD -noF -noX -all-root -no-xattrs -noappend -quiet
gzip -9 -c images/disk-x.img > web/disk-x.img.gz
rm -f web/disk-x.img
echo "images/disk-x.img: $(du -h images/disk-x.img | cut -f1) (無圧縮squashfs) / web/disk-x.img.gz: $(du -h web/disk-x.img.gz | cut -f1)"
