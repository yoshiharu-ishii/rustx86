#!/bin/sh
# gcc入りinitramfs — 共有の木 (make-gcc-root.sh) を1本のcpioに詰める。
#
# **games版と違って継ぎ足しではなく、1本に詰め直す。** カーネルは連結cpioも
# 展開できるが、gzipが末尾で名乗る展開後の大きさ (ISIZE) は**最後の塊の分**
# しか無い。ローダはその数字を見てRAMの要不要を判断する (initrd_ram_needed)
# ので、継ぎ足すと少なく見積もって「起動はするが中身が欠ける」を素通しする。
#
# **これは重い。** 展開後90MiBあり、initramfs方式だとRAMを256MB要求する
# (圧縮イメージと展開後の中身が同時にRAMに載るため)。同じ木をディスクで積む
# make-gcc-disk.sh なら128MBで済む — こちらは「ディスク無しでも動く」用の保険。
#
# 使い方: tools/images/make-gcc-initramfs.sh
set -e
cd "$(dirname "$0")/../.."
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
sh tools/images/make-gcc-root.sh "$work/root"

# 1本のcpioに詰め直す。実行ビットは mkcpio.py が元ファイルから拾う。
# /dev/console のノードはミニ側と同じく自前で足す (無いとinitが盲目で走る)
python3 tools/images/mkcpio.py "$work/gcc.cpio" "$work/root" --console
gzip -c "$work/gcc.cpio" > images/initramfs-gcc
cp images/initramfs-gcc web/initramfs-gcc
echo "images/initramfs-gcc: $(du -h images/initramfs-gcc | cut -f1) (web/ へも複製)"
python3 - <<'SIZES'
import struct
d = open("images/initramfs-gcc", "rb").read()
mib = 1 << 20
print(f"  圧縮 {len(d) / mib:.1f} MiB → 展開後 {struct.unpack('<I', d[-4:])[0] / mib:.1f} MiB")
SIZES
