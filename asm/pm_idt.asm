; プロテクトモードの割り込みと例外。
;
; pm_hello が「境界を越えて生きて戻る」だったのに対し、こちらは
; **割り込みの作法がモードで丸ごと入れ替わる**ことを確かめる。
;
; リアルモード: IVT (0番地の 4バイト×256 の表) を引いて seg:off へ飛ぶ
; 保護モード:   IDT (8バイトのゲート記述子の表) を引き、**ゲートが
;               「どのセグメントの、どこへ、どの作法で」を全部言う**
;
; 確かめる2経路:
;   1. int 0x40           — ソフトウェア割り込みがゲートを通る。iretd で戻る
;   2. ud2 (0F 0B)        — **例外 (#UD, ベクタ6)**。CPUが自分で起こす割り込み。
;                           handler は戻らず目印を書いて HLT (フォールトは
;                           同じ命令へ戻るので、iretd すると永久ループになる)
;
; アセンブル: nasm -f bin -o pm_idt.bin pm_idt.asm

org 0x7C00
bits 16

start:
    cli
    xor  ax, ax
    mov  ds, ax
    lgdt [gdtr]
    lidt [idtr]                 ; ★ IDTもここで教える (0F 01 /3)
    mov  eax, cr0
    or   eax, 1
    mov  cr0, eax
    jmp  0x08:pm_entry

bits 32
pm_entry:
    mov  ax, 0x10
    mov  ds, ax
    mov  ss, ax
    mov  esp, 0x7000

    ; --- 経路1: ソフトウェア割り込み。ゲートを通って、iretd で戻ってくる ---
    ; ベクタ7を使うのは**表を8本で済ませて512バイトに収める**ため。
    ; 0x41本並べると表だけで520バイトになりブートセクタから溢れる
    int  7
    ; (0x504 の目印は handler 側が書く)
    mov  dword [0x508], 0xBAC2_5AFE ; iretd で帰ってきた証拠

    ; --- 経路2: 例外。ud2 は「わざと #UD を起こす」ための公式の命令 ---
    ud2
    ; ここへは戻らない (handlerがHLTする)。戻ったらこの目印が書かれてしまう
    mov  dword [0x50C], 0xDEAD_0BAD
    hlt

; int 7 の受け手。目印を書いて iretd で帰る
soft_handler:
    mov  dword [0x504], 0x50F7_1234
    iretd

; #UD の受け手。フォールトは同じ命令へ戻るので、帰らずに目印を書いて止まる
ud_handler:
    mov  dword [0x500], 0x0BAD_0F0B
    hlt

; ---- GDT (pm_hello と同じ: null / 32bitコード / 32bitデータ) ----
align 8
gdt:
    dq 0
    dw 0xFFFF, 0x0000
    db 0x00, 0x9A, 0xCF, 0x00
    dw 0xFFFF, 0x0000
    db 0x00, 0x92, 0xCF, 0x00
gdt_end:

; ---- IDT ----
;
; ゲート記述子も8バイト。offsetが上下に割れているのはGDTの記述子と
; 同じ理由 (286の形に32bit分を継ぎ足した)。
;   type 0x8E = 割り込みゲート (present, DPL=0, 32bit。IFを落として入る)
%macro idt_gate 1
    dw (%1 - $$ + 0x7C00) & 0xFFFF  ; offset[15:0]
    dw 0x08                          ; セレクタ (コードセグメント)
    db 0
    db 0x8E                          ; P=1 DPL=0 32bit interrupt gate
    dw ((%1 - $$ + 0x7C00) >> 16)   ; offset[31:16]
%endmacro

align 8
idt:
    ; ベクタ0..5: 空 (P=0)。踏んだら「not present」でエミュレータが咎める
    times 6 dq 0
    idt_gate ud_handler             ; ベクタ6 = #UD
    idt_gate soft_handler           ; ベクタ7 (ソフト割り込みの実験台)
idt_end:

gdtr:
    dw gdt_end - gdt - 1
    dd gdt

idtr:
    dw idt_end - idt - 1
    dd idt

times 510-($-$$) db 0
dw 0xAA55
