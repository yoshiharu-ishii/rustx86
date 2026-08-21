#!/usr/bin/env bash
# bounce.c から i386 の静的バイナリを焼き直す (Docker の Alpine x86 で)。
# 生成物 (bounce) はコミットしてあるので、イメージ焼きはこれを実行しない
set -euo pipefail
cd "$(dirname "$0")"
docker run --rm --platform linux/386 -v "$PWD:/src" -w /src alpine:3.24 \
  sh -c "apk add -q gcc musl-dev linux-headers >/dev/null 2>&1 && gcc -static -O2 -Wall -o bounce bounce.c"
ls -l bounce
