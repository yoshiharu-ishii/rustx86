#!/bin/sh
# FreeDOS の C: (BIOS INT 13h のドライブ 0x80) — DOS 版 DOOM (shareware 1.9) + 自作ゲーム + mTCP。
# フロッピー (A:) で起動して、C:\DOOM と C:\GAMES を使う。
#
#   tools/images/sh/make-doom-hdd.sh            # images/doom/unpacked/ から焼く
#   HDD=images/freedos-hdd.img cargo run --release --example boot -- images/fd14boot.img
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
out=images/freedos-hdd.img
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
# DEFAULT.CFG: 音の装置を決めておく (SETUP.EXE を通さずに鳴るように)。
# 効果音 = PC スピーカー (1、6s で実装済み)、音楽 = Adlib/OPL2 (2、6t)。
# DOOM 既定は両方 0 (無音) で、DMX は装置が無ければポートに触りもしない
cfg=$(mktemp)
printf 'snd_sfxdevice\t\t1\r\nsnd_musicdevice\t\t2\r\nsnd_channels\t\t3\r\nsnd_musicvolume\t\t12\r\nsnd_sfxvolume\t\t12\r\nmouse_sensitivity\t5\r\nusemouse\t\t0\r\nusejoystick\t\t0\r\nscreenblocks\t\t10\r\ndetaillevel\t\t0\r\n' > "$cfg"
mcopy -i "$out@@$off" "$cfg" ::/DOOM/DEFAULT.CFG
rm -f "$cfg"
# 自作ゲーム・DEBUG・mTCP はフロッピー (fd14games.img) から写す — A: と同じ物が C: にも居て、
# プロンプトを C:\> に置けるように。フロッピーが無ければ DOOM だけ
fd=images/fd14games.img
if [ -f "$fd" ]; then
  mmd -i "$out@@$off" ::/GAMES
  tmp=$(mktemp -d)
  for f in ELIZA.EXE RESPONSE.DAT BOUNCE.COM ZMIY.EXE ROW4T.COM HANGMAN.EXE AIR.COM DEBUG.COM; do
    mcopy -i "$fd" "::/$f" "$tmp/" 2>/dev/null && mcopy -i "$out@@$off" "$tmp/$f" "::/GAMES/" || true
  done
  for f in NE2000.COM PING.EXE DHCP.EXE HTGET.EXE MTCP.CFG; do
    mcopy -i "$fd" "::/$f" "$tmp/" 2>/dev/null && mcopy -i "$out@@$off" "$tmp/$f" "::/" || true
  done
  rm -rf "$tmp"
fi
# 小さな案内 (DOS の CR+LF)
printf 'C:\\DOOM> DOOM        (DOS 版 DOOM shareware 1.9)\r\nC:\\GAMES> BOUNCE    (mode 13h のボール)  AIR (PC スピーカー)  ZMIY / ELIZA / ROW4T\r\nC:\\> NE2000 0x60 3 0x300   SET MTCPCFG=C:\\MTCP.CFG   DHCP   PING 1.1.1.1\r\n' > "$out.readme.txt"
mcopy -i "$out@@$off" "$out.readme.txt" ::/README.TXT
rm -f "$out.readme.txt"
mdir -i "$out@@$off" ::/DOOM; mdir -i "$out@@$off" ::/GAMES 2>/dev/null || true
gzip -9 -c "$out" > web/freedos-hdd.img.gz
echo "$out: $(du -h "$out" | cut -f1) / web/freedos-hdd.img.gz: $(du -h web/freedos-hdd.img.gz | cut -f1)"
