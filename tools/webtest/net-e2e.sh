#!/usr/bin/env bash
#
# ネットワークE2Eを**閉じた世界**で回す (CIとローカルで同じ手順)。
#
#   bash tools/webtest/net-e2e.sh
#
# ## なぜ閉じるのか
#
# 16bit (FreeDOS) と 32bit (Linux) のネットE2Eは本来 1.1.1.1 と
# example.com を叩く。**外に出るCIは3つの意味で弱い**:
#
#   - 相手のサイトが落ちればこちらが赤くなる (自分の変更と無関係に)
#   - ICMPには権限が要る (共有ランナーで通る保証がない)
#   - 誰かの本番サーバに毎push負荷をかける
#
# なので宛先を内側に畳む。**ゲスト側が通る道は1バイトも変わらない**:
#
#   ping 10.0.2.2   → SLiRPのゲートウェイ宛。wsslirpのnetstackが自分で
#                     答えるので、外にもICMPの権限にも依存しない
#   http://<host>   → このスクリプトが立てたHTTPサーバ。wsslirpdを
#                     -allow-private で立てて、ホストのループバックへ繋ぐ
#
# 見張れるのは「NICがゲストから見えるか・DHCPが通るか・ICMPが往復するか・
# TCPが流れるか・ゲストの時計が実時間か」。2026-08-14に16bitのNICが
# PCI化で消えたデグレは、これがあれば当日CIで捕まっていた。
#
# TLS (https) は閉じた世界では検証できない (信頼できる証明書が無い) ので、
# 実インターネット向けの手動E2Eが受け持つ — 台帳は docs/reference/ci.md。
#
# 立てたものは**このスクリプトが必ず片づける** (trap)。残骸のwsslirpdは
# 古いバイナリで偽のバグを生むので、起動と停止を1つの器に閉じ込める。
set -euo pipefail
cd "$(dirname "$0")/../.."

PORT_WS=${PORT_WS:-8188}
PORT_HTTP=${PORT_HTTP:-8199}
TOKEN=ci-net-e2e
# wsslirpのバージョンは固定する (@latestは再現しない)。上げるときはここ
WSSLIRP_PKG=${WSSLIRP_PKG:-github.com/yoshiharu-ishii/wsslirp/cmd/wsslirpd@main}

work=$(mktemp -d)
pids=()
cleanup() {
  # kill のあと wait で看取る — 看取らないとシェルが
  # 「Terminated: 15 ...」を出力に混ぜ、レポートがノイズで汚れる
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  for p in "${pids[@]:-}"; do wait "$p" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

# ---- ゲストが引きにいく中身。合言葉を1つ置くだけ ----
#
# **起動するのは必ず本体そのもの**にする (subshell や `go run` を挟まない)。
# 挟むと $! が中間プロセスを指し、それを殺しても中の本体が生き残る —
# 実際に wsslirpd と http.server が居座り、次の走行が古いバイナリに
# 繋がる形になった。残骸は偽のバグを生むので、構造で断つ
echo 'rustx86-net-ok' > "$work/index.html"
python3 -m http.server "$PORT_HTTP" --bind 0.0.0.0 --directory "$work" >/dev/null 2>&1 &
pids+=($!)

# ---- SLiRP backend。先にバイナリへ焼いてから起動する ----
# -allow-private: 既定は公開アドレスしか通さない (開いたリレーにしないための
# 門番)。ここは自分で立てた自分宛なので開けてよい
if [ -n "${WSSLIRP_DIR:-}" ]; then
  (cd "$WSSLIRP_DIR" && go build -o "$work/wsslirpd" ./cmd/wsslirpd)
else
  GOBIN="$work" go install "$WSSLIRP_PKG"
fi
"$work/wsslirpd" -listen "127.0.0.1:$PORT_WS" -token "$TOKEN" -allow-private \
  >"$work/wsslirpd.log" 2>&1 &
pids+=($!)

# 立ち上がりを待つ
for i in $(seq 30); do
  if curl -s -o /dev/null "http://127.0.0.1:$PORT_WS/" 2>/dev/null; then break; fi
  sleep 1
done
if ! curl -s -o /dev/null "http://127.0.0.1:$PORT_WS/" 2>/dev/null; then
  echo "wsslirpd が上がらない:"; cat "$work/wsslirpd.log"; exit 1
fi

# ゲストから見たホストの番地。**127.0.0.1 は使えない** — ゲストにとって
# それは自分自身で、線にすら出ない。wsslirpdは受け取った宛先IPへそのまま
# dialするので、ホスト自身の実アドレス (LAN側) を渡す必要がある。
# 外向きUDPソケットのローカル側を見て決める (パケットは飛ばない。
# ifconfig/ip の出力を刻むより移植性が高い — macOSとLinuxで同じ)
HOST_IP=$(python3 -c "import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(('1.1.1.1', 80))
print(s.getsockname()[0])" 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$HOST_IP" ] || { echo "ホストの番地が決まらない"; exit 1; }
echo "ホストの番地: $HOST_IP:$PORT_HTTP (ゲストはここへ取りに行く)"

export RUSTX86_NET_E2E_URL="ws://127.0.0.1:$PORT_WS/net?token=$TOKEN"
export RUSTX86_NET_E2E_PING=10.0.2.2
export RUSTX86_NET_E2E_HTTP="http://$HOST_IP:$PORT_HTTP/"
export RUSTX86_NET_E2E_EXPECT=rustx86-net-ok

fail=0
echo "### 16bit (FreeDOS + mTCP)"
node tools/webtest/netping.mjs || fail=1
echo
echo "### 32bit (Linux + udhcpc/wget)"
node tools/webtest/netlinux.mjs || fail=1

if [ "$fail" -ne 0 ]; then
  echo
  echo "--- wsslirpd のログ ---"
  tail -30 "$work/wsslirpd.log"
fi
exit "$fail"
