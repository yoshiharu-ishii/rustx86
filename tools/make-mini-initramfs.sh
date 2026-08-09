#!/bin/sh
# ミニinitramfs — busybox + snake + 3行のinitで**まっすぐシェルへ**。
#
# Alpineのinitスクリプトはブートメディア探し (nlplug-findfs) を通るが、
# エミュレータにはまだブロックデバイスが無い。探させない。
# busybox は Alpine の initramfs-lts から借りる (静的リンク・動作実績あり)。
set -e
cd "$(dirname "$0")/.."
[ -f images/initramfs-lts ] || { echo "images/initramfs-lts が無い"; exit 1; }
[ -f tools/guest/snake ] || { echo "tools/guest/snake が無い"; exit 1; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
# busybox と musl (Alpineのbusyboxは**動的リンク** — ローダとlibcも要る。
# 最初これを忘れて "No working init found" で1敗した)
(cd "$work" && gzcat "$OLDPWD/images/initramfs-lts" | cpio -idm --quiet 2>/dev/null)
[ -f "$work/bin/busybox" ] || { echo "busyboxが取り出せない"; exit 1; }
[ -f "$work/lib/ld-musl-i386.so.1" ] || { echo "ld-muslが取り出せない"; exit 1; }

mkdir -p "$work/root/bin" "$work/root/lib" "$work/root/proc" "$work/root/sys" "$work/root/dev"
cp "$work/bin/busybox" "$work/root/bin/busybox"
cp "$work/lib/ld-musl-i386.so.1" "$work/root/lib/"
cp "$work/lib/libc.musl-x86.so.1" "$work/root/lib/" 2>/dev/null || true
cp tools/guest/snake "$work/root/bin/snake"
chmod 755 "$work/root/bin/busybox" "$work/root/bin/snake" "$work/root/lib/"*
cat > "$work/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sys /sys
/bin/busybox mount -t devtmpfs dev /dev 2>/dev/null
/bin/busybox --install -s /bin
echo
echo "  rustx86 mini initramfs — busybox shell"
echo "  ゲーム: snake"
echo
exec /bin/busybox sh
INIT
chmod 755 "$work/root/init"
# cpioは自前で書く (tools/mkcpio.py) — /dev/console ノードを非rootで
# 含めるため。コンソールノードが無いとinitは入出力ゼロの盲目で走る
python3 tools/mkcpio.py "$work/mini.cpio" "$work/root" --console
gzip -c "$work/mini.cpio" > images/initramfs-mini
echo "images/initramfs-mini: $(du -h images/initramfs-mini | cut -f1)"
