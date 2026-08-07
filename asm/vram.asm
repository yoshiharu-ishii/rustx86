; テキストVRAM (0xB8000) への直接書き込み (リアルモード 8086)
;
; DOSやUNIXのコンソールはBIOSを呼ばず、**ビデオメモリに直接書く**。
; BIOS経由 (INT 10h) は1文字ごとに割り込みが要るので遅すぎるためである。
;
; 0xB8000 から 2バイトで1文字: [文字コード][属性]。
; 属性は上位4bitが背景色、下位4bitが前景色。
;
; アセンブル: nasm -f bin -o vram.bin vram.asm

org 0x7C00
bits 16

start:
    xor  ax, ax
    mov  ds, ax
    mov  ss, ax
    mov  sp, 0x7C00

    mov  ax, 0xB800          ; セグメント値 x 16 = 0xB8000
    mov  es, ax

    ; 1行目の先頭に "BUS" を置く
    mov  di, 0
    mov  si, msg
    mov  ah, 0x0F            ; 属性: 黒背景 + 明るい白
.next:
    lodsb                    ; AL = [SI++]
    or   al, al
    jz   .row2
    stosw                    ; ES:[DI] = AX (AL=文字, AH=属性)、DI += 2
    jmp  .next

    ; 2行目 (80桁 x 2バイト = 160バイト先) にも書く。
    ; 「行」がメモリ上で連続していることの確認
.row2:
    mov  di, 160
    mov  si, msg2
.next2:
    lodsb
    or   al, al
    jz   .done
    stosw
    jmp  .next2

.done:
    hlt

msg:  db "BUS", 0
msg2: db "OK", 0

times 510-($-$$) db 0
dw 0xAA55
