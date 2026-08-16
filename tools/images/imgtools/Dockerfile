# イメージ焼きの道具箱 — Linuxのファイルシステムを焼く仕事だけをここでやる。
#
# **開発全体をコンテナに入れるのではない。** Rust/wasmのビルドと速度測定は
# ネイティブのまま (定規はネイティブのみ — コンテナ越しの測定はVMの配置
# ノイズが乗る)。ホスト (macOS/Windows) に無いのは mksquashfs や mke2fs
# だけなので、その道具だけをAlpineから借りる。
#
# ビルド:  docker build -t rustx86-imgtools tools/images/imgtools
# 使い方:  tools/images/in-linux.sh <コマンド...>  (リポジトリを /w に見せて実行)
#
# ゲストと同じ Alpine v3.24 を使う — 焼いたFSを読むのはゲストのカーネルなので、
# 道具の癖 (圧縮の既定値など) がゲストの世代と揃っているのが一番事故が少ない
FROM alpine:3.24

RUN apk add --no-cache \
    squashfs-tools \
    e2fsprogs \
    cpio \
    tar \
    python3 \
 && cp /usr/share/apk/keys/x86/* /etc/apk/keys/
# 最後のcp: x86 (32bit) のパッケージ署名鍵。コンテナは自分の石の鍵しか
# 鍵輪に持たないが、うちが組むrootfsはx86 — 鍵が無いと署名検証を
# 切る羽目になる (--allow-untrusted)。鍵は同梱の alpine-keys から写すだけ

WORKDIR /w
