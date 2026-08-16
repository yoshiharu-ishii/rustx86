#!/bin/sh
# ミニinitramfs — busybox + snake + 3行のinitで**まっすぐシェルへ**。
#
# Alpineのinitスクリプトはブートメディア探し (nlplug-findfs) を通るが、
# エミュレータにはまだブロックデバイスが無い。探させない。
# busybox は Alpine の initramfs-lts から借りる (静的リンク・動作実績あり)。
set -e
cd "$(dirname "$0")/../.."
[ -f images/initramfs-lts ] || { echo "images/initramfs-lts が無い"; exit 1; }
[ -f tools/guest/snake ] || { echo "tools/guest/snake が無い"; exit 1; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
# busybox と musl (Alpineのbusyboxは**動的リンク** — ローダとlibcも要る。
# 最初これを忘れて "No working init found" で1敗した)
# gzcat は macOS の名前で Linux (CI) には無い。gunzip -c は両方に居る。
# cpio コマンドの無いホスト (Windows) は自前の読み側 (mkcpio.py --extract) で開く
if command -v cpio >/dev/null 2>&1; then
  (cd "$work" && gunzip -c "$OLDPWD/images/initramfs-lts" | cpio -idm --quiet 2>/dev/null)
else
  python3 tools/images/mkcpio.py --extract images/initramfs-lts "$work"
fi
[ -f "$work/bin/busybox" ] || { echo "busyboxが取り出せない"; exit 1; }
[ -f "$work/lib/ld-musl-i386.so.1" ] || { echo "ld-muslが取り出せない"; exit 1; }

mkdir -p "$work/root/bin" "$work/root/lib" "$work/root/proc" "$work/root/sys" "$work/root/dev" "$work/root/tmp"
cp "$work/bin/busybox" "$work/root/bin/busybox"
cp "$work/lib/ld-musl-i386.so.1" "$work/root/lib/"
cp "$work/lib/libc.musl-x86.so.1" "$work/root/lib/" 2>/dev/null || true
cp tools/guest/snake "$work/root/bin/snake"
# ネットワークのモジュール。Alpineのinitramfsから3つだけ借りる —
# カーネルと同じ荷物から取るので vermagic が必ず合う。
#   8390 + ne2k-pci  NICのドライバ (RTL8029 = PCI版NE2000)
#   af_packet        生ソケット (udhcpc がDHCPを喋るのに要る)
mod_dir=$(dirname "$(find -L "$work/lib" "$work/usr/lib" -name ne2k-pci.ko 2>/dev/null | head -1)")
pkt=$(find -L "$work/lib" "$work/usr/lib" -name af_packet.ko 2>/dev/null | head -1)
mkdir -p "$work/root/lib/modules"
cp "$mod_dir/8390.ko" "$mod_dir/ne2k-pci.ko" "$pkt" "$work/root/lib/modules/"
# ディスクのモジュール (virtio-blk-pci)。これも同じ荷物から借りる。
#   virtio + virtio_ring     リングの共通機構
#   virtio_pci (+legacy/modern_dev)  PCIの上に建つ口
#   virtio_blk               ブロック装置本体 → /dev/vda
for mod in virtio_ring virtio virtio_pci_modern_dev virtio_pci_legacy_dev virtio_pci virtio_blk; do
  ko=$(find -L "$work/lib" "$work/usr/lib" -name "$mod.ko" 2>/dev/null | head -1)
  [ -n "$ko" ] && cp "$ko" "$work/root/lib/modules/"
done
# TLS一式 — busyboxのwgetはhttpsを外部ヘルパ ssl_client に投げる。
# 全部Alpineのinitramfs-ltsから借りる (busybox/muslと同じ出自)。
#   ssl_client            13KB  wgetのTLS口 (libssl/libcryptoに動的リンク)
#   libssl/libcrypto.so.3 4.5MB OpenSSL本体。**同梱物で一番重い**が、TLSの
#                               実体そのものなので削れない (gzip後 約2.1MB)
#   ca-certificates.crt   179KB 信頼の根。減らすと「特定サイトだけ謎に失敗」
#                               を生むので全束のまま入れる
# 前提はcore側に2つ: MMX (libcryptoが#UDせず動く) と RTCの実時刻注入
# (証明書の有効期間検証)。どちらが欠けてもhttpsは開かない
cp "$work/bin/ssl_client" "$work/root/bin/ssl_client"
cp "$work/usr/lib/libssl.so.3" "$work/usr/lib/libcrypto.so.3" "$work/root/lib/"
mkdir -p "$work/root/etc/ssl/certs"
cp "$work/etc/ssl/certs/ca-certificates.crt" "$work/root/etc/ssl/certs/"
# OpenSSLの既定CAファイルは /etc/ssl/cert.pem (OPENSSLDIR直下)。
# certs/ だけ入れても見つけてくれない — Alpineと同じ別名を張る
ln -sf certs/ca-certificates.crt "$work/root/etc/ssl/cert.pem"
chmod 755 "$work/root/bin/ssl_client"
chmod 755 "$work/root/bin/busybox" "$work/root/bin/snake" "$work/root/lib/"ld-musl* "$work/root/lib/"libc* 2>/dev/null || true
# udhcpc がリースを**実際に適用する**スクリプト。busybox の udhcpc は
# 取ったリースを自分では適用せず、このスクリプトに渡すだけである
# (無いと「取れたのにアドレスが付かない」になる)
mkdir -p "$work/root/usr/share/udhcpc"
cat > "$work/root/usr/share/udhcpc/default.script" <<'DHCP'
#!/bin/busybox sh
# $1: deconfig | bound | renew。変数は udhcpc が環境で渡す
case "$1" in
  deconfig) busybox ifconfig "$interface" 0.0.0.0 ;;
  bound|renew)
    busybox ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}"
    [ -n "$router" ] && busybox route add default gw "$router" dev "$interface"
    [ -n "$dns" ] && echo "nameserver $dns" > /etc/resolv.conf
    ;;
esac
DHCP
chmod 755 "$work/root/usr/share/udhcpc/default.script"
mkdir -p "$work/root/etc"

cat > "$work/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sys /sys
/bin/busybox mount -t devtmpfs dev /dev 2>/dev/null
/bin/busybox --install -s /bin
# シリアルコンソールにはTERMが無い。viやlessがフルスクリーン描画の
# 作法を選べるように、素直なxtermを名乗っておく
export TERM=xterm
/bin/busybox stty rows 24 cols 80
# ループバックを上げる。**通常のLinuxではinitスクリプトの仕事**で、
# うちのミニinitramfsは誰もやっていなかった — `ping 127.0.0.1` が
# 100% packet loss になる (自分自身にすら届かない、妙な機械だった)。
# lo が下りていると、UNIXドメインではなくTCPで自分に繋ぐ道具も全部黙る
/bin/busybox ifconfig lo 127.0.0.1 netmask 255.0.0.0 up
# NICのドライバを挿す。**カードが無くてもエラーにはならない** — ドライバは
# 載るがbindする相手が居ないだけ (実機にカードを挿していないのと同じ)。
# 依存の順: ne2k-pci は 8390 の上に建つ
/bin/busybox insmod /lib/modules/af_packet.ko 2>/dev/null
/bin/busybox insmod /lib/modules/8390.ko 2>/dev/null
/bin/busybox insmod /lib/modules/ne2k-pci.ko 2>/dev/null
# ディスクのドライバ。**カードが無くてもエラーにならない**のはNICと同じ。
# 依存の順は .ko の depends= の実測から: ring が土台で virtio がその上
# (直感と逆。逆順で挿すと virtio_blk が Unknown symbol で落ちる — 実際に落ちた)
for mod in virtio_ring virtio virtio_pci_modern_dev virtio_pci_legacy_dev virtio_pci virtio_blk; do
  /bin/busybox insmod /lib/modules/$mod.ko 2>/dev/null
done
# カードが挿さっていればDHCPを裏で回す (ELKSの rc.sys が ktcp を上げるのと
# 同じ作法)。**挿さっていなければ何もしない** — NIC無し起動は素のまま。
# -b: リースが取れるまで裏で粘る (線が後から生きても拾える)
if [ -e /sys/class/net/eth0 ]; then
  /bin/busybox ifconfig eth0 up
  /bin/busybox udhcpc -i eth0 -b -q -s /usr/share/udhcpc/default.script >/dev/null 2>&1
fi
echo
echo "  rustx86 mini initramfs — busybox shell"
echo "  ゲーム: snake   エディタ: vi"
echo
# **シェルはexecせずforkで起こす。** 2つ理由がある:
#
# 1. 制御端末: initから直接execすると制御端末が無く、^CのSIGINTを配る
#    相手が居ない (pingが止められなかった)。setsidで新セッションを起こし、
#    そのリーダーに実体の /dev/ttyS0 を開かせて制御端末にする
#    (/dev/console は制御端末になれない。cttyhackはAlpineのbusyboxに無い)
# 2. **シェルがPID 1のままだと ^Z が永久に効かない。** カーネルの孤児
#    プロセスグループ判定は「親が global init のメンバーは数えない」
#    (is_global_init(p->real_parent)) ので、シェル=PID1だと全ジョブが
#    孤児扱いになり、TSTP/TTIN/TTOU は仕様で捨てられる (SIGSTOPだけ効く
#    という奇妙な姿になる — 実際になった)。forkすれば親はPID1でなくなる
#
# --- gcc入りイメージ (make-gcc-initramfs.sh) のための細工 ---
#
# **gccは自分の居場所を argv[0] から逆算する。** シェルが `gcc` という裸の
# 名前で起動すると (PATHで見つけても argv[0] は "gcc" のまま)、gcc内部の
# make_relative_prefix が相対形を返し、探索路が `../libexec/gcc/…` になる。
# カレントが / なら `/../libexec` = `/libexec` で、存在しないので
#
#   gcc: fatal error: cannot execute 'cc1': posix_spawnp: No such file or directory
#
# になる (`/usr/bin/gcc` と絶対パスで呼べば当たる。同じ実体なのに呼び名で
# 挙動が変わるので、原因に辿り着くまで遠回りした)。**環境変数で居場所を
# 教えれば裸の名前でも通る**:
#   GCC_EXEC_PREFIX  cc1 / collect2 / as / ld を探す起点
#   LIBRARY_PATH     crtbegin.o・libgcc.a と libc の置き場
# gccの入っていないミニイメージでは丸ごと無効 (ディレクトリが無い)
if [ -d /usr/libexec/gcc ]; then
  export GCC_EXEC_PREFIX=/usr/libexec/gcc/
  for d in /usr/lib/gcc/*/*/; do
    [ -d "$d" ] && export LIBRARY_PATH="$d:/usr/lib:/lib"
  done
fi
#
# シェルが死んだら起こし直す (getty代わり)。孤児の回収はPID1のashが
# 子待ちのついでにやる
while :; do
  /bin/busybox setsid /bin/busybox sh -c \
    'exec /bin/busybox sh </dev/ttyS0 >/dev/ttyS0 2>&1'
done
INIT
chmod 755 "$work/root/init"
# cpioは自前で書く (tools/images/mkcpio.py) — /dev/console ノードを非rootで
# 含めるため。コンソールノードが無いとinitは入出力ゼロの盲目で走る
python3 tools/images/mkcpio.py "$work/mini.cpio" "$work/root" --console
gzip -c "$work/mini.cpio" > images/initramfs-mini
# ブラウザ版 (linux-machine.js) は web/ から読む。置き忘れると initrd 無しで
# 起動して VFS パニックになる (実際になった) ので、作ったその場で配る
cp images/initramfs-mini web/initramfs-mini
echo "images/initramfs-mini: $(du -h images/initramfs-mini | cut -f1) (web/ へも複製)"
