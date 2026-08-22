#!/bin/sh
# 起動できる ISO (El Torito、no-emulation) を焼く — isolinux + 自前の Linux (vmlinuz-lts +
# initramfs-mini)。6c (ISO 起動) の検証用: 「実物のブートローダが INT 13h の CD を読んで
# カーネルを上げる」が通るかを見る。
#
#   tools/images/sh/make-test-iso.sh            → images/test-linux.iso
#   cargo run --release --example boot -- images/test-linux.iso   (BIOS 経由で起動)
#
# isolinux は Alpine の syslinux パッケージ (x86 専用) から借りる。道具箱は arm64 なので
# `apk --arch x86 fetch` で取って展開するだけ (rootfs と同じ手)。
set -e
cd "$(dirname "$0")/../../.."
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
[ -f images/vmlinuz-lts ] && [ -f images/initramfs-mini ] || { echo "images/vmlinuz-lts と images/initramfs-mini が要る (fetch-images.sh linux / make-mini-initramfs.sh)" >&2; exit 1; }
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
mkdir -p "$work/pkg" "$work/iso/isolinux"
# 引数: [出力 ISO] [isolinux.bin] — 環境変数はコンテナに渡らないので引数で
# (4.x の isolinux.bin は単体で動き ldlinux.c32 が要らない)
out=${1:-images/test-linux.iso}
ISOLINUX_BIN=${2:-}
if [ -n "$ISOLINUX_BIN" ]; then
  cp "$ISOLINUX_BIN" "$work/iso/isolinux/isolinux.bin"
else
  apk --arch x86 fetch --no-cache -o "$work/pkg" syslinux >/dev/null
  tar -xzf "$work/pkg"/syslinux-*.apk -C "$work/pkg" 2>/dev/null || true
  cp "$work/pkg/usr/share/syslinux/isolinux.bin" "$work/pkg/usr/share/syslinux/ldlinux.c32" "$work/iso/isolinux/"
fi
cp images/vmlinuz-lts "$work/iso/vmlinuz"
cp images/initramfs-mini "$work/iso/initramfs"
cat > "$work/iso/isolinux/isolinux.cfg" <<'CFG'
SERIAL 0 115200
DEFAULT linux
PROMPT 0
TIMEOUT 1
LABEL linux
  KERNEL /vmlinuz
  INITRD /initramfs
  APPEND console=ttyS0 quiet
CFG
xorriso -as mkisofs -quiet -o "$out" \
  -b isolinux/isolinux.bin -c isolinux/boot.cat -no-emul-boot -boot-load-size 4 -boot-info-table \
  -V RUSTX86 "$work/iso"
echo "$out: $(du -h "$out" | cut -f1) (isolinux + vmlinuz-lts + initramfs-mini)"
