; ページング — 線形アドレスと物理アドレスの間に「表」を挟む。
;
; ここまで線形アドレスはそのまま物理だった。ページングを入れると、
; 上位20bitが**2段の表を引く鍵**になり、下位12bit (ページ内オフセット) だけが
; 素通しになる。「0xC0000000 はカーネルの定位置」のような、物理メモリの
; 実際の場所と無関係な仮想番地が、これで初めて意味を持つ。
;
; この実験:
;   - ページテーブルPTを1枚: 物理 0..1MB を素直に写す (恒等写像)
;   - ページディレクトリPD: エントリ0 と エントリ768 の両方が同じPTを指す。
;     768 = 0xC0000000 >> 22。つまり **0xC0000000 は物理0の別名**になる
;   - PG を立てる → 恒等写像のおかげでコードは走り続ける
;   - 高位の別名 0xC0000500 へ書く → 低位 0x500 を読んで、同じ物理を
;     指していることを確かめる
;
; メモリ配置 (物理):
;   0x2000 ページディレクトリ / 0x3000 ページテーブル
;
; アセンブル: nasm -f bin -o pm_paging.bin pm_paging.asm

org 0x7C00
bits 16

start:
    cli
    xor  ax, ax
    mov  ds, ax
    lgdt [gdtr]
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

    ; --- ページテーブルを組む (まだページング前なので線形=物理で書ける) ---
    ; PT @ 0x3000: 256エントリ (=1MB) を恒等写像。entry[i] = (i<<12) | 3
    ;   3 = present(bit0) + writable(bit1)
    mov  edi, 0x3000
    xor  ebx, ebx                   ; 物理アドレス、0から
    mov  ecx, 256
.fill_pt:
    mov  eax, ebx
    or   eax, 3
    mov  [edi], eax
    add  edi, 4
    add  ebx, 0x1000
    loop .fill_pt

    ; PD @ 0x2000: エントリ0 と 768 が同じPTを指す
    mov  dword [0x2000 + 0*4],   0x3000 | 3
    mov  dword [0x2000 + 768*4], 0x3000 | 3   ; 768 = 0xC0000000 >> 22

    mov  eax, 0x2000
    mov  cr3, eax                   ; CR3 = ページディレクトリの物理番地

    mov  eax, cr0
    or   eax, 0x80000000            ; CR0.PG
    mov  cr0, eax                   ; ★ ここからアドレスは表を通る

    ; 高位の別名へ書く。0xC0000500 → (PD[768]→PT→物理0x500)
    mov  dword [0xC0000500], 0xCAFE_F00D

    ; 低位 (恒等) から同じ物理を読む。別名が効いていれば一致する
    mov  eax, [0x00000500]
    mov  [0x0600], eax              ; 検証しやすいよう別の場所へ写す

    hlt

; ---- GDT (32bit flat) ----
align 8
gdt:
    dq 0
    dw 0xFFFF, 0x0000
    db 0x00, 0x9A, 0xCF, 0x00
    dw 0xFFFF, 0x0000
    db 0x00, 0x92, 0xCF, 0x00
gdt_end:

gdtr:
    dw gdt_end - gdt - 1
    dd gdt

times 510-($-$$) db 0
dw 0xAA55
