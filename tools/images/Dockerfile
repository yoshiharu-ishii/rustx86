# イメージ焼きの道具箱 — **tools/images/sh/ に居るスクリプトは全部この中で走る**。
#
# レイアウトの規約 (2026-08-16):
#   tools/images/Dockerfile   この道具箱の定義
#   tools/images/in-linux.sh  入口 (リポジトリを /w に見せて実行)
#   tools/images/sh/*.sh      コンテナ内で走るもの (先頭の番人が自分で入る)
#   tools/images/*.sh         ネイティブに残るもの (エミュレータを走らせる係)
#
# **開発全体をコンテナに入れるのではない。** Rust/wasmのビルドと速度測定は
# ネイティブのまま (定規はネイティブのみ — コンテナ越しの測定はVMの配置
# ノイズが乗る)。ここに寄せるのは「ホストの道具に依存する素材加工」だけ。
#
# ゲストと同じ Alpine v3.24 を使う — 焼いたFSを読むのはゲストのカーネルなので、
# 道具の癖 (圧縮の既定値など) がゲストの世代と揃っているのが一番事故が少ない
FROM alpine:3.24

# bash:      fetch-images.sh / extract-vmlinux.sh がbashで書いてある
# curl/unzip/mtools/nasm: イメージの取得と組み立て (fetch-images.sh)
# xz/zstd:   bzImageの解凍 (extract-vmlinux.sh)
RUN apk add --no-cache \
    squashfs-tools \
    e2fsprogs \
    cpio \
    tar \
    python3 \
    bash \
    curl \
    unzip \
    mtools \
    nasm \
    xz \
    zstd \
 && cp /usr/share/apk/keys/x86/* /etc/apk/keys/
# 最後のcp: x86 (32bit) のパッケージ署名鍵。コンテナは自分の石の鍵しか
# 鍵輪に持たないが、うちが組むrootfsはx86 — 鍵が無いと署名検証を
# 切る羽目になる (--allow-untrusted)。鍵は同梱の alpine-keys から写すだけ

WORKDIR /w
