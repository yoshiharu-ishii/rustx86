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
#    xterm               端末
#    font-misc-misc + font-cursor-misc  xterm が要る bitmap フォントとカーソル
#    icewm               ウィンドウマネージャ (タスクバー+メニュー。古典デスクトップの絵)
#    dillo / w3m / links + ca-certificates  ブラウザ3種 (GUI / 端末 / 両用) と TLS の信頼束。
#                        ネットワーク編の wsslirp 経由で https が通る
#    feh / xfe           画像・ファイラ (mupdf-x11 は libmupdf 51MB で見送り)
#    xclock / xeyes / xcalc / xmessage / xsetroot  X の定番の小物
#    xboard + gnuchess   ゲーム
#    font-dejavu + fontconfig  Xft を使うアプリ (dillo/icewm) の文字
#    font-ipaex          日本語 (IPAex 明朝/ゴシック、約11MB。noto-cjk は 88MB なので見送り)
mkdir -p "$work/pkg"
apk --arch x86 --root "$work/pkg" --initdb -U --no-scripts \
  --keys-dir /etc/apk/keys \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/main \
  -X https://dl-cdn.alpinelinux.org/alpine/v3.24/community \
  add xorg-server xf86-video-fbdev xf86-input-evdev xinit xkbcomp xkeyboard-config \
      xterm font-misc-misc font-cursor-misc \
      icewm \
      dillo w3m links ca-certificates \
      feh xfe \
      xclock xeyes xcalc xmessage xsetroot \
      xboard gnuchess \
      font-dejavu fontconfig font-ipaex
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
       usr/lib/libvulkan* usr/share/vulkan usr/share/glvnd \
       usr/lib/libgallium* usr/lib/gallium-pipe usr/lib/libxatracker*
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
# PATH も export する — init 由来のシェルは PATH を子へ渡さず、icewm は
# メニューの各項目の実行ファイルを PATH で探して**見つからない項目を隠す**
# (prog "xterm" すら消えて、Windows/Settings/Logout だけの空メニューになった)
export HOME=/root SHELL=/bin/sh PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
exec /usr/bin/xinit "${1:-/root/.xinitrc}" -- /usr/bin/Xorg :0 vt2 -config /etc/X11/xorg.conf -nolisten tcp -logfile /var/log/Xorg.0.log
STARTX
chmod 755 usr/bin/startx
cat > root/.xinitrc <<'XINITRC'
#!/bin/sh
xsetroot -solid '#204060' 2>/dev/null
xterm -geometry 100x30+8+8 -fn fixed -e /bin/sh &
exec icewm-session
XINITRC
chmod 755 root/.xinitrc
# icewm のメニュー (タスクバー左の icewm ボタン)。入れたアプリを役割ごとに並べる。
# 端末系 (w3m/links) は xterm の中で起こす。/etc/icewm/menu は全ユーザー共通の既定
mkdir -p etc/icewm
cat > etc/icewm/menu <<'MENU'
prog "xterm" xterm xterm -fn fixed -geometry 100x30
separator
menu "ブラウザ" folder {
    prog "Dillo (GUI)" dillo dillo
    prog "links -g (グラフィック)" links links -g https://pocraft.net/
    prog "w3m (xterm)" xterm xterm -fn fixed -geometry 100x36 -e w3m https://pocraft.net/
    prog "links (xterm)" xterm xterm -fn fixed -geometry 100x36 -e links https://pocraft.net/
}
menu "道具" folder {
    prog "xfe (ファイラ)" xfe xfe
    prog "feh (画像)" feh feh /usr/share/icewm
    prog "xcalc" xcalc xcalc
    prog "xclock" xclock xclock -geometry 150x150
    prog "xeyes" xeyes xeyes
}
menu "ゲーム" folder {
    prog "xboard (gnuchess)" xboard xboard -fcp gnuchess
}
separator
prog "bounce (fbdev、X終了後に)" xterm xterm -fn fixed -e sh -c 'echo "X を終了してから ~ # bounce"; sleep 3'
separator
restart "icewm を再起動" icewm icewm
MENU
# icewm のメニューと xterm の日本語: icewm は Xft (fontconfig) で IPAex を拾う。
# xterm は bitmap の fixed だと日本語が出ないので、Xft 版の項目を別に用意する
sed -i 's#^prog "xterm" xterm xterm -fn fixed -geometry 100x30$#prog "xterm" xterm xterm -fn fixed -geometry 100x30\nprog "xterm (日本語, Xft)" xterm xterm -u8 -fa "IPAexGothic" -fs 11 -geometry 100x30#' etc/icewm/menu
mkdir -p etc/icewm
cat > etc/icewm/preferences <<'PREFS'
# 日本語が出るフォント (Xft)。無い環境なら DejaVu に落ちる
TitleFontNameXft="IPAexGothic:size=10:bold"
MenuFontNameXft="IPAexGothic:size=10"
StatusFontNameXft="IPAexGothic:size=10"
QuickSwitchFontNameXft="IPAexGothic:size=10"
NormalButtonFontNameXft="IPAexGothic:size=10"
ActiveButtonFontNameXft="IPAexGothic:size=10:bold"
NormalTaskBarFontNameXft="IPAexGothic:size=10"
ActiveTaskBarFontNameXft="IPAexGothic:size=10:bold"
ToolButtonFontNameXft="IPAexGothic:size=10"
ListBoxFontNameXft="IPAexGothic:size=10"
LabelFontNameXft="IPAexGothic:size=10"
ClockFontNameXft="IPAexGothic:size=10"
PREFS
# dillo (FLTK/Xft) は書体を**名指し**で開き、fontconfig の字形単位のフォールバックは
# 効かない — 日本語のページが四角 (tofu) になった (2026-08-22)。dillorc で IPAex を
# 指定しても通らない: Dillo は FLTK の font 一覧と名前を突き合わせるが、FLTK は
# FcNameUnparse の「:」の手前 = **家族名の並び "IPAexGothic,IPAexゴシック"** を
# 名前にするので "IPAexGothic" は一覧に無く、並びそのものを書いても not found。
# そこで fontconfig 側で「DejaVu を頼まれたら IPAex を返す」— Xft を使う全員
# (dillo / icewm / xterm -fa) に効き、Latin も IPAex に入っているので困らない
mkdir -p etc/fonts
cat > etc/fonts/local.conf <<'FONTS'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <!-- rustx86: 日本語が出る書体へ (font-ipaex)。dillo の既定は DejaVu 名指し -->
  <match target="pattern">
    <test name="family"><string>DejaVu Sans</string></test>
    <edit name="family" mode="prepend" binding="strong"><string>IPAexGothic</string></edit>
  </match>
  <match target="pattern">
    <test name="family"><string>DejaVu Serif</string></test>
    <edit name="family" mode="prepend" binding="strong"><string>IPAexMincho</string></edit>
  </match>
  <match target="pattern">
    <test name="family"><string>DejaVu Sans Mono</string></test>
    <edit name="family" mode="prepend" binding="strong"><string>IPAexGothic</string></edit>
  </match>
</fontconfig>
FONTS
# root の行 — xterm は SHELL の他に /etc/passwd も引き、無いと
# "No absolute path found for shell" と警告する (動きはする)。whoami 等にも効く
mkdir -p etc
grep -q '^root:' etc/passwd 2>/dev/null || echo 'root:x:0:0:root:/root:/bin/sh' >> etc/passwd
grep -q '^root:' etc/group 2>/dev/null || echo 'root:x:0:' >> etc/group
export HOME=/root
echo 'export HOME=/root' >> etc/profile 2>/dev/null || true
cd - >/dev/null
echo "Xの木: $root ($(du -sh "$root" | cut -f1))"
