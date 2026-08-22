#!/bin/sh
# DOS 版 DOOM (shareware 1.9) 入りのハードディスク像 — BIOS INT 13h のドライブ 0x80 (C:)。
#
#   tools/images/sh/make-doom-hdd.sh            # images/doom/unpacked/ から焼く
#   HDD=images/doom-hdd.img cargo run --release --example boot -- images/fd14boot.img
#
# 素材: idgames の doom19s.zip (shareware、再配布自由)。中の DOOMS_19.1/.2 を結合すると
# PKZIP の自己解凍書庫なので unzip で開く (DOOM.EXE は DOS/4GW 同梱、DOOM1.WAD 4.2MB):
#   curl -L -o images/doom/doom19s.zip https://www.gamers.org/pub/idgames/idstuff/doom/doom19s.zip
#   (cd images/doom && unzip -o doom19s.zip && cat DOOMS_19.1 DOOMS_19.2 > DOOMS_19.EXE \
#      && mkdir -p unpacked && cd unpacked && unzip -o ../DOOMS_19.EXE)
#
# 形状は core の Disk::hdd_from_image と同じ 16ヘッド×63セクタ。MBR の区画表は
# python で書き、FAT16 は mtools (mformat/mcopy) で区画のオフセットに直接焼く —
# ループデバイスもルート権限も要らない。DOS はこの像を BIOS 経由で読むだけなので
# ATA の素子は要らない (INT 13h の高位エミュレーションで足りる)
set -e
cd "$(dirname "$0")/../../.."
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
src=${1:-images/doom/unpacked}
out=images/doom-hdd.img
[ -f "$src/DOOM.EXE" ] && [ -f "$src/DOOM1.WAD" ] || { echo "$src に DOOM.EXE / DOOM1.WAD が無い (上のコメントの手順で展開)" >&2; exit 1; }

MB=${MB:-16}
HEADS=16; SPT=63
CYL=$((MB * 1024 * 1024 / (HEADS * SPT * 512)))
TOTAL=$((CYL * HEADS * SPT))
START=$SPT                       # 区画は 0/1/1 (LBA 63) から — DOS の定石
COUNT=$((TOTAL - START))
python3 - "$out" $CYL $HEADS $SPT $START $COUNT <<'PY'
import struct, sys
out, cyl, heads, spt, start, count = sys.argv[1], *map(int, sys.argv[2:])
total = cyl * heads * spt
def chs(lba):
    c = lba // (heads * spt); h = (lba // spt) % heads; s = lba % spt + 1
    return bytes([h, ((c >> 2) & 0xC0) | s, c & 0xFF])
mbr = bytearray(512)
# 区画 1: 起動印なし、種別 0x06 (FAT16、32MB 超も可)。CHS と LBA の両方を書く
mbr[446:462] = b'\x00' + chs(start) + b'\x06' + chs(start + count - 1) + struct.pack('<II', start, count)
mbr[510:512] = b'\x55\xAA'
with open(out, 'wb') as f:
    f.write(mbr)
    f.truncate(total * 512)
print(f"{out}: {total * 512 // 1048576}MB, CHS {cyl}/{heads}/{spt}, 区画 LBA {start}+{count}")
PY
off=$((START * 512))
# FAT16 を区画の位置に焼く。-H = 隠しセクタ (区画の前のセクタ数) を BPB に書く
mformat -i "$out@@$off" -t $CYL -h $HEADS -s $SPT -H $START -v DOOM ::
mmd -i "$out@@$off" ::/DOOM
mcopy -i "$out@@$off" "$src"/DOOM.EXE "$src"/DOOM1.WAD "$src"/SETUP.EXE "$src"/README.TXT ::/DOOM/
# 小さな案内 (DOS の CR+LF)
printf 'C:\\DOOM> DOOM\r\n  -nomouse -nosound などは DOOM.EXE の引数\r\n' > "$out.readme.txt"
mcopy -i "$out@@$off" "$out.readme.txt" ::/README.TXT
rm -f "$out.readme.txt"
mdir -i "$out@@$off" ::/DOOM
gzip -9 -c "$out" > web/doom-hdd.img.gz
echo "$out: $(du -h "$out" | cut -f1) / web/doom-hdd.img.gz: $(du -h web/doom-hdd.img.gz | cut -f1)"
