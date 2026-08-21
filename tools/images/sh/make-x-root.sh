#!/bin/sh
# X入りrootfsの木を組む (共有部品)。
#
#   tools/images/sh/make-x-root.sh <出力dir>
#
# ミニinitramfsの中身に Alpine の Xorg (fbdevドライバ・evdev入力) と
# twm/xterm を重ね、「X が1枚上がる最小の木」を作る。gcc版 (make-gcc-root.sh)
# と同じ立て付けで、詰め方 (squashfs) は make-x-disk.sh の仕事。
#
# ## なぜ fbdev + evdev か
#
# この機械のグラフィックスは efifb のリニアFB (/dev/fb0) だけで、GPUは無い
# (ADR-0011)。X の描画は全部ソフトウェアで、fbdev ドライバが /dev/fb0 に
# 直接描く。入力は PS/2 キーボードとマウスが evdev (/dev/input/event*) に
# 出ているので、evdev ドライバで読む。udev が居ないので xorg.conf に全部書く。
set -e
cd "$(dirname "$0")/../../.."
# イメージ焼きは道具箱 (Linuxコンテナ) の中で (make-mini-initramfs.shと同じ判断)
[ -f /.dockerenv ] || exec tools/images/in-linux.sh sh "$0" "$@"
[ -f images/initramfs-mini ] || { echo "images/initramfs-mini が無い (tools/images/sh/make-mini-initramfs.sh)"; exit 1; }
root=$1
[ -n "$root" ] || { echo "使い方: make-x-root.sh <出力dir>"; exit 1; }
case "$root" in /*) ;; *) root="$PWD/$root" ;; esac
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# 1. Alpine v3.24 x86 の X 一式を apk 本人に引かせる (依存の閉包も署名も apk の仕事)。
#    xorg-server         X サーバ本体
#    xf86-video-fbdev    /dev/fb0 に描くドライバ (GPU無しの唯一の道)
#    xf86-input-evdev    /dev/input/event* を読む入力ドライバ
#    xinit               xinit (startx は自前で書き直す — xauth/mcookie を要らなくする)
#    xkbcomp + xkeyboard-config  キーボードの対応表 (無いと X がキー入力を捨てる)
#    twm + xterm         最小のウィンドウマネージャと端末
#    font-misc-misc + font-cursor-misc  xterm/twm が要る bitmap フォントとカーソル
mkdir -p "$work/pkg"
apk --arch x86 --root "$work/pkg" --initdb -U --no-scripts \
  --keys-dir /etc/apk/keys \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/main \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/community \
  add xorg-server xf86-video-fbdev xf86-input-evdev xinit xkbcomp xkeyboard-config \
      twm xterm font-misc-misc font-cursor-misc
rm -rf "$work/pkg/lib/apk" "$work/pkg/var/cache" "$work/pkg/etc/apk" "$work/pkg/dev"

# 2. ミニの木の上に重ねる
mkdir -p "$root"
(cd "$root" && gunzip -c "$OLDPWD/images/initramfs-mini" | cpio -idm --quiet 2>/dev/null)
(cd "$work/pkg" && cp -a . "$root/")
cd "$root"
# 文書・man・ロケール・pkgconfig・静的ライブラリは運ばない
rm -rf usr/share/doc usr/share/man usr/share/licenses usr/share/info usr/lib/pkgconfig usr/include
find usr/lib -name "*.a" -delete 2>/dev/null || true
# **GLX と mesa/LLVM は捨てる。** GPUの無い fbdev に GLX は無意味で、しかも
# xorg-server の依存で入る mesa の llvmpipe が libLLVM (195MB) を引きずる —
# 木の 2/3 がこれだった。xorg.conf で glx を Disable し、実体も消す
rm -rf usr/lib/libLLVM* usr/lib/dri usr/lib/libGL* usr/lib/libGLES* usr/lib/libEGL* \
       usr/lib/libgbm* usr/lib/libglapi* usr/lib/xorg/modules/extensions/libglx.so \
       usr/lib/libvulkan* usr/share/vulkan usr/share/glvnd usr/lib/libLLVM*
mkdir -p tmp/.X11-unix var/log etc/X11 root
chmod 1777 tmp tmp/.X11-unix

# 3. xorg.conf — udev が無いので装置を全部明示する。
#    event0 = AT Translated Set 2 keyboard (i8042 KBD)、event1 = PS/2 マウス (AUX)。
#    番号は initramfs の insmod の順 (psmouse → mousedev → evdev) で決まる
cat > etc/X11/xorg.conf <<'XORG'
Section "Module"
    Disable "glx"
EndSection

Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
    Option "BlankTime" "0"
    Option "StandbyTime" "0"
    Option "SuspendTime" "0"
    Option "OffTime" "0"
EndSection

Section "InputDevice"
    Identifier "kbd"
    Driver "evdev"
    Option "Device" "/dev/input/event0"
    Option "XkbLayout" "us"
EndSection

Section "InputDevice"
    Identifier "mouse"
    Driver "evdev"
    Option "Device" "/dev/input/event1"
EndSection

Section "Device"
    Identifier "fb"
    Driver "fbdev"
    Option "fbdev" "/dev/fb0"
EndSection

Section "Screen"
    Identifier "scr"
    Device "fb"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
    EndSubSection
EndSection

Section "ServerLayout"
    Identifier "layout"
    Screen "scr"
    InputDevice "kbd" "CoreKeyboard"
    InputDevice "mouse" "CorePointer"
EndSection
XORG

# 4. startx — xinit のものは xauth/mcookie を要求するので、自前の短いのに置き換える。
#    vt2 で上げる (シェルは tty1 に居る)。終わると tty1 へ戻る
cat > usr/bin/startx <<'STARTX'
#!/bin/sh
# X を上げる: twm + xterm。何も引数が無ければ ~/.xinitrc。
# HOME と SHELL は決め打ち — init のシェルは HOME=/ で来るので ~/.xinitrc が
# 見つからず、xterm は SHELL 未設定だと /etc/passwd からログインシェルを引いて
# (この木には無い) "No absolute path found for shell" と言って即死する
export HOME=/root SHELL=/bin/sh
exec /usr/bin/xinit "${1:-/root/.xinitrc}" -- /usr/bin/Xorg :0 vt2 -config /etc/X11/xorg.conf -nolisten tcp -logfile /var/log/Xorg.0.log
STARTX
chmod 755 usr/bin/startx
cat > root/.xinitrc <<'XINITRC'
#!/bin/sh
xsetroot -solid '#204060' 2>/dev/null
twm &
exec xterm -geometry 78x24+8+8 -fn fixed -e /bin/sh
XINITRC
chmod 755 root/.xinitrc
export HOME=/root
echo 'export HOME=/root' >> etc/profile 2>/dev/null || true
cd - >/dev/null
echo "Xの木: $root ($(du -sh "$root" | cut -f1))"
