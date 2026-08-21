; VGA mode 13h (320x200x8bpp) の一筆書き (リアルモード 8086)
;
; グラフィックスの検証はこの3点で足りる:
;   1. INT 10h AX=0013h でモードが入る
;   2. 0x3C8/0x3C9 でパレットが流し込める (Bを書くと色番号が自動歩進)
;   3. 0xA0000 に置いたバイトがそのまま画素になる (ただのRAM)
; 加えて 0x3DA (垂直帰線) を1回だけ読む — ポートが生きていることの確認。
; 帰線待ちループにしないのは、テストの命令数を決定的に保つため。
;
; アセンブル: nasm -f bin -o mode13.bin mode13.asm

org 0x7C00
bits 16

start:
    xor  ax, ax
    mov  ds, ax
    mov  ss, ax
    mov  sp, 0x7C00

    ; --- 1. mode 13h へ ---
    mov  ax, 0x0013
    int  0x10

    ; --- 2. パレット: 色16=赤 色17=緑 色18=青 (6bit値、連続書きで歩進) ---
    mov  dx, 0x3C8
    mov  al, 16
    out  dx, al
    inc  dx                  ; 0x3C9
    mov  al, 63              ; 色16 = (63, 0, 0)
    out  dx, al
    xor  al, al
    out  dx, al
    out  dx, al
    out  dx, al              ; 色17 = (0, 63, 0) — インデックスは書き直さない
    mov  al, 63
    out  dx, al
    xor  al, al
    out  dx, al
    out  dx, al              ; 色18 = (0, 0, 63)
    out  dx, al
    mov  al, 63
    out  dx, al

    ; --- 3. 画素: 先頭行に 0..255 の色番号を並べる (グラデーション) ---
    mov  ax, 0xA000
    mov  es, ax
    xor  di, di
    xor  al, al
.grad:
    stosb                    ; ES:[DI++] = AL
    inc  al
    jnz  .grad               ; 256画素 (ALが一周したら終わり)

    ; 2行目 (offset 320) の先頭に色16,17,18を1画素ずつ
    mov  di, 320
    mov  al, 16
    stosb
    inc  al
    stosb
    inc  al
    stosb

    ; --- 見た目の検証用: EGA16色の縦帯で画面の残りを埋める ---
    ; (行4〜195、20画素幅×16本。テストが見るのは上の2行だけで、
    ;  ここはブラウザのスクリーンショットで人間が確かめる領域)
    mov  di, 4*320
    mov  cx, 192             ; 192行
.band_row:
    push cx
    xor  bl, bl              ; 色 0 から
.band:
    mov  al, bl
    mov  cx, 20
    rep  stosb               ; 20画素ぶん同じ色
    inc  bl
    cmp  bl, 16
    jb   .band
    pop  cx
    loop .band_row

    ; --- 0x3DA を読む (値は捨てる。ポートの生存確認) ---
    mov  dx, 0x3DA
    in   al, dx

    hlt

times 510-($-$$) db 0
dw 0xAA55
