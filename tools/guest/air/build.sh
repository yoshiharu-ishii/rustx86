#!/usr/bin/env bash
# 楽譜 (bach-air.ly) から AIR.COM を焼き直す。
# 生成物 (notes.inc / AIR.COM) はコミットしてあるので、fetch-images.sh は
# これを実行しない — nasm の無いホストでもイメージが組めるようにするため。
set -euo pipefail
cd "$(dirname "$0")"
python3 ly2notes.py bach-air.ly > notes.inc
nasm -f bin air.asm -o AIR.COM
ls -l AIR.COM
