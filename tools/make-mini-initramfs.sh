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
# busybox を取り出す
(cd "$work" && gzcat "$OLDPWD/images/initramfs-lts" | cpio -idm --quiet bin/busybox 2>/dev/null)
[ -f "$work/bin/busybox" ] || { echo "busyboxが取り出せない"; exit 1; }

mkdir -p "$work/root/bin" "$work/root/proc" "$work/root/sys" "$work/root/dev"
cp "$work/bin/busybox" "$work/root/bin/busybox"
cp tools/guest/snake "$work/root/bin/snake"
chmod 755 "$work/root/bin/busybox" "$work/root/bin/snake"
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
(cd "$work/root" && find . | cpio -o -H newc --quiet | gzip) > images/initramfs-mini
echo "images/initramfs-mini: $(du -h images/initramfs-mini | cut -f1)"
