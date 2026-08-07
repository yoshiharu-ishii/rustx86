; 割り込み機構のE2Eテスト (リアルモード 8086)
;
; 「OSが割り込みを乗っ取る」という一連の流れをブートセクタ1枚で再現する。
;
;   1. 自前のハンドラをIVTに登録し、INT 0x80 で呼ぶ
;   2. 書き換えていないINT 10hは、BIOS HLE のまま動き続ける
;   3. ゼロ除算 (#DE = INT 0) でマシンが落ちず、自前ハンドラへ飛ぶ
;   4. ハンドラがスタック上の戻り先を進めて、除算の次から再開する
;
; 成功すると "ABD!" が出る。
;
; アセンブル: nasm -f bin -o interrupt.bin interrupt.asm

org 0x7C00
bits 16

start:
    xor  ax, ax
    mov  ds, ax
    mov  es, ax
    mov  ss, ax
    mov  sp, 0x7C00

    ; --- IVTを自分のハンドラで書き換える。OSが起動時にやることと同じ ---
    ; IVTは 0x0000 から 4バイト×256個。n番目に [オフセット, セグメント] が並ぶ
    mov  word [0x80*4],   handler80    ; INT 0x80 のオフセット
    mov  word [0x80*4+2], 0            ;          セグメント
    mov  word [0*4],      handler_de   ; INT 0 (ゼロ除算)
    mov  word [0*4+2],    0

    ; 1. 自前ハンドラを呼ぶ
    int  0x80

    ; 2. INT 10h はIVTを書き換えていないので BIOS HLE のまま
    mov  al, 'B'
    mov  ah, 0x0E
    int  0x10

    ; 3. ゼロ除算。実機はここでマシンが止まらず #DE ハンドラへ飛ぶ
    mov  ax, 100
    xor  cx, cx
    div  cx                            ; ← 2バイト (F7 F1)

    ; 4. ハンドラが戻り先を進めたのでここへ来る
    mov  al, '!'
    mov  ah, 0x0E
    int  0x10

    hlt

; INT 0x80 のハンドラ。中からさらにBIOSを呼んでいる (割り込みの入れ子)
handler80:
    mov  al, 'A'
    mov  ah, 0x0E
    int  0x10
    iret

; ゼロ除算ハンドラ。
;
; #DE は**フォールト**なので、積まれている戻り先は「失敗したDIVそのもの」である。
; そのままIRETすると同じDIVをもう一度実行して無限ループになる。
; ここでは原因を直せないので、戻り先をDIVの次へ進めてから戻る。
handler_de:
    mov  al, 'D'
    mov  ah, 0x0E
    int  0x10
    push bp
    mov  bp, sp
    ; スタック: [bp]=退避したBP, [bp+2]=IP, [bp+4]=CS, [bp+6]=FLAGS
    add  word [bp+2], 2                ; DIV CX の長さだけ進める
    pop  bp
    iret

times 510-($-$$) db 0
dw 0xAA55
