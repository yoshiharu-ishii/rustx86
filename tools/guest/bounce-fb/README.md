# bounce — Linux の fbdev で色とりどりのボールが跳ねる

「Linux (フレームバッファ)」機で `~ # bounce`。何かキーで終わる。
DOS版 ([tools/guest/bounce](../bounce/)) の Linux 版で、やっていることは同じ。
違うのは**画素の置き場所の見つけ方**だけ:

| | DOS (mode 13h) | Linux (fbdev) |
|---|---|---|
| 画面に入る | `INT 10h AX=0013h` | `open("/dev/fb0")` |
| 解像度・画素形式 | 320×200×8bpp と決まっている | `ioctl(FBIOGET_VSCREENINFO)` で聞く |
| 画素を置く | `0xA0000` に直書き | `mmap` した先に書く |
| テンポ | `0x3DA` の垂直帰線を待つ | `nanosleep` で 70Hz |
| 終わる | `INT 16h` でキーを覗く | 端末を raw にして `poll` |

画素形式 (bpp・R/G/B の位置) は **ioctl が返す値から組み立てる**。決め打ちしない —
busybox の fbsplash は 24bpp を BGR 決め打ちで書くので、赤が下位の rustx86 では
色が入れ替わる。この作法なら、どんな並びの fbdev でも正しい色になる。

```
bounce.c ─── gcc -static -O2 (i386 musl) ─── bounce (生成物、コミット済み)
                                                │  make-mini-initramfs.sh が /bin/bounce に置く
                                                ▼
                                       ~ # bounce
```

焼き直すときは `./build.sh` (Docker の Alpine x86 で静的にリンクする)。
