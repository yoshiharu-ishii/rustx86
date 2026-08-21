#!/usr/bin/env bash
# bounce.asm から BOUNCE.COM を焼き直す。
# 生成物 (BOUNCE.COM) はコミットしてあるので、fetch-images.sh はこれを
# 実行しない — nasm の無いホストでもイメージが組めるようにするため。
set -euo pipefail
cd "$(dirname "$0")"
nasm -f bin bounce.asm -o BOUNCE.COM
ls -l BOUNCE.COM
