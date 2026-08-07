; ブートセクタ Hello, World (リアルモード 8086)
; BIOSのINT 10h AH=0Eh (テレタイプ出力) で1文字ずつ表示する。
; エミュレータ側はINT 10hをHLEフックして標準出力/バッファに流す。
;
; アセンブル: nasm -f bin -o hello.bin hello.asm

org 0x7C00
bits 16

start:
    mov  si, msg
    cld
.next:
    lodsb                ; AL = [SI++]
    or   al, al
    jz   .halt
    mov  ah, 0x0E        ; テレタイプ出力
    int  0x10
    jmp  .next
.halt:
    hlt
    jmp  .halt

msg: db "Hello, World!", 0

times 510-($-$$) db 0
dw 0xAA55                ; ブートシグネチャ
