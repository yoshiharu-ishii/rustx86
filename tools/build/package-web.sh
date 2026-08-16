#!/usr/bin/env bash
#
# 配布zipを組み立てる — 「落として2クリックで動く」形にする。
#
#   bash tools/build/package-web.sh          # dist/rustx86-web.zip
#   VERSION=network-final bash tools/build/package-web.sh
#
# ## なぜスクリプトにするか
#
# 前回のリリース (16bit-final) の zip は手で組んだので、**中に何を入れたかが
# 再現できない**。START.command すらリポジトリに残っていなかった。
# これは fetch-images.sh の冒頭に書いた話と同じ型で、「手で組み立てたものは
# 再現できない」— 組み立て方をコードにすれば中身が一意に決まる。
#
# ## 入れるもの・入れないもの
#
# - 入れる: web/ の一式 (wasm込み)、キャッシュを切るサーバー、起動の2クリック、
#   イメージの取得スクリプト
# - **入れない: OSのディスクイメージ**。ELKSもFreeDOSもAlpineも自由ソフトウェア
#   なので再頒布は許されているが、GPLのバイナリ配布にはソースの提供義務が付く。
#   教材が他人のOSの再頒布者になると、その責任が恒久的に付いて回る
#   (fetch-images.sh の冒頭に同じ判断を書いてある)
# - **入れない: SLiRP backend (wsslirpd)**。ネットワークを使うには別途デーモンが
#   要る — 別リポジトリのGoプログラムで、ビルド済みバイナリを同梱すると
#   「どのOS向けか」を配る責任が生まれる。案内だけ置く
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=${VERSION:-dev}
out="dist/rustx86-web-$VERSION.zip"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
root="$work/rustx86"
mkdir -p "$root"

[ -f web/pkg/rustx86_wasm_bg.wasm ] || { echo "先に bash tools/build/build-web.sh"; exit 1; }

# ---- web一式 (イメージは除く。除き方を「並べる」ではなく「弾く」にしておくと、
#      新しいファイルが増えたときに勝手に入る — 入れ忘れより安全) ----
mkdir -p "$root/pkg"
cp web/*.html web/*.js web/serve.py "$root/"
cp web/pkg/*.js web/pkg/*.wasm "$root/pkg/"
# .d.ts と package.json は動作に要らない (型定義はビルド時の産物)

# ---- イメージの取得スクリプト。zipのルートに置く ----
# (fetch-images.sh は web/ の在り処で自分の位置を判断するので、ルート直下でも動く)
cp tools/images/sh/fetch-images.sh "$root/"
mkdir -p "$root/tools/images"
cp tools/images/mkcpio.py tools/images/sh/extract-vmlinux.sh tools/images/sh/make-mini-initramfs.sh "$root/tools/images/" 2>/dev/null || true

# ---- 2クリックで起動する入口 ----
# **index.html の直接ダブルクリックでは動かない** — ESモジュールとwasmは
# file:// では読めない (CORS)。だからキャッシュを切る小さなサーバーを同梱し、
# それを起こす入口を置く
cat > "$root/START.command" <<'MAC'
#!/bin/sh
# ダブルクリックで起動 (macOS / Linux)
cd "$(dirname "$0")"
python3 serve.py 8080 &
sleep 1
(command -v open >/dev/null && open http://localhost:8080/) || \
  (command -v xdg-open >/dev/null && xdg-open http://localhost:8080/) || \
  echo "ブラウザで http://localhost:8080/ を開いてください"
wait
MAC
chmod +x "$root/START.command"

cat > "$root/START.bat" <<'WIN'
@echo off
rem ダブルクリックで起動 (Windows)
cd /d "%~dp0"
start "" http://localhost:8080/
python serve.py 8080
WIN

# ---- 同梱物の案内。**同梱していないもの**の理由もここに書く ----
cat > "$root/README.txt" <<'TXT'
rustx86 — ブラウザで動く x86 エミュレータ

■ 動かす
  macOS / Linux : START.command をダブルクリック
  Windows       : START.bat をダブルクリック
  (index.html の直接ダブルクリックでは動きません。ESモジュールとwasmは
   file:// では読めないため、同梱の小さなサーバー越しに開きます)

■ OSのイメージは入っていません
  ELKS / FreeDOS / Alpine Linux はいずれも自由ソフトウェアで再頒布は
  許されていますが、GPLのバイナリ配布にはソースの提供義務が付きます。
  教材が他人のOSの再頒布者になると責任が恒久的に付いて回るので、
  取得はスクリプトに任せています。

    ./fetch-images.sh          # 全部
    ./fetch-images.sh elks     # ELKS だけ
    ./fetch-images.sh linux    # Linux (Alpine) だけ

  イメージが無くても、デバッガと実行速度の計測は試せます。

■ ネットワークを使うには
  ゲストを外に出すには SLiRP backend (wsslirpd) が別途要ります。
  ユーザーモードNATで、管理者権限もTAPデバイスも要りません。

    https://github.com/yoshiharu-ishii/wsslirp
    go run ./cmd/wsslirpd -listen 127.0.0.1:8087 -token <好きな文字列>

  立てたら画面左の「ネットワーク」で接続先を指定します。

■ 詳しく
  https://github.com/yoshiharu-ishii/rustx86
TXT

mkdir -p dist
rm -f "$out"
(cd "$work" && zip -qr "$OLDPWD/$out" rustx86)
echo "$out: $(du -h "$out" | cut -f1)"
unzip -l "$out" | tail -3
