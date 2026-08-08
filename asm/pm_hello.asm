; プロテクトモードの Hello World。
;
; リアルモードの hello.asm が「画面に文字を出す」だったのに対し、こちらの
; ゴールは**モードの境界を越えて生きて戻ること**である。画面には何も出さない。
; 32bitコードで EAX に目印を書いて HLT できたら成功。
;
;   1. GDT を用意して LGDT               ← 「セグメントとは何か」を機械に教え直す
;   2. CR0 の PE ビットを立てる          ← ここからプロテクトモード。だが**まだ16bit**
;   3. far jump                          ← パイプラインを流し、CSに記述子を積んで初めて完成
;   4. 32bitコードで1命令実行して HLT
;
; PE=1 にした瞬間ではなく far jump で遷移が完成する、という2段構えが要点で、
; 386の設計者がプリフェッチ済みの16bit命令を安全に流すために選んだ手順である。
; 現代のブートローダも起動のたびに毎回この3行を通っている。
;
; アセンブル: nasm -f bin -o pm_hello.bin pm_hello.asm

org 0x7C00
bits 16

start:
    cli                         ; 遷移中に割り込みが来ると死ぬ (IDTがまだ無い)
    xor  ax, ax
    mov  ds, ax

    lgdt [gdtr]                 ; GDTの場所と大きさを教える (0F 01 /2)

    mov  eax, cr0               ; CR0を読む (0F 20 /r)
    or   eax, 1                 ; PE (Protection Enable)
    mov  cr0, eax               ; ここからプロテクトモード (0F 22 /r)

    ; far jump で CS にセレクタ 0x08 を積む。**記述子の隠しレジスタが
    ; ロードされて初めて遷移が完成**する。ここまでは16bitのまま走っている
    jmp  0x08:pm_entry

bits 32
pm_entry:
    mov  ax, 0x10               ; データセグメント (2番目の記述子)
    mov  ds, ax
    mov  ss, ax
    mov  esp, 0x7C00

    mov  eax, 0x32B17600        ; 目印: 「32BIT」+ 0x600 (テストが読む)
    mov  [0x500], eax           ; 32bitのアドレッシングでメモリにも書く
    hlt

; ---- GDT ----
;
; 記述子は8バイト。base と limit が細切れに散っているのは、286の16bit記述子
; (6バイト) に**後方互換の形で**32bit分の桁を継ぎ足したためである。
; ここにも地層がある。
align 8
gdt:
    dq 0                        ; 0番: ヌル記述子 (セレクタ0を封じる番人)
    ; 1番 (0x08): コード。base=0 limit=0xFFFFF granularity=4K → 4GB全部
    ;   access=0x9A (P=1 DPL=0 code, readable)  flags=0xC (G=1 D=1: 32bit)
    dw 0xFFFF                   ; limit[15:0]
    dw 0x0000                   ; base[15:0]
    db 0x00                     ; base[23:16]
    db 0x9A                     ; access
    db 0xCF                     ; flags | limit[19:16]
    db 0x00                     ; base[31:24]
    ; 2番 (0x10): データ。access=0x92 (P=1 DPL=0 data, writable)
    dw 0xFFFF
    dw 0x0000
    db 0x00
    db 0x92
    db 0xCF
    db 0x00
gdt_end:

gdtr:
    dw gdt_end - gdt - 1        ; limit (大きさ-1)
    dd gdt                      ; base (32bitの物理アドレス)

times 510-($-$$) db 0
dw 0xAA55
