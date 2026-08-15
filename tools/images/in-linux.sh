#!/bin/sh
# Linuxの道具 (mksquashfs / mke2fs 等) をコンテナ経由で借りる。
#
#   tools/images/in-linux.sh mksquashfs root.d disk.squashfs -comp gzip
#
# リポジトリのルートを /w に見せて、そこを作業場所に実行する。
# イメージ (rustx86-imgtools) が無ければその場で焼く — 初回だけ数十秒。
#
# **なぜコンテナか**: squashfs/ext2を焼く道具はLinuxのものが本物で、
# macOSに移植を探したり自前でパッカーを書いたりするより、本物を借りる方が
# 事故が少ない (道具の癖がゲストのカーネルと揃う)。逆にRustのビルドと
# 速度測定はここを**通さない** — 定規はネイティブのみ。
set -e
cd "$(dirname "$0")/../.."

command -v docker >/dev/null 2>&1 || {
  echo "dockerが無い。イメージ焼きの道具 (mksquashfs等) はLinuxコンテナから借りる" >&2
  exit 1
}
docker image inspect rustx86-imgtools >/dev/null 2>&1 || {
  echo "道具箱を焼く (初回のみ)..." >&2
  docker build -q -t rustx86-imgtools tools/images/imgtools >&2
}

exec docker run --rm -v "$PWD:/w" -w /w rustx86-imgtools "$@"
