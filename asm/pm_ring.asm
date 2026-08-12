; 特権リング — リング3で走り、リング0へ落ち、また3へ帰る。
;
; OSの権力の正体はこの往復である。ユーザープログラム (リング3) は
; I/OもHLTも触れず、**割り込みゲートという狭い門**を通ってカーネル
; (リング0) に頼み事をする。現代のシステムコールの祖形。
;
; 旅程:
;   ring0: TSSとIDTを**RAMに組み立て** (512バイトの床に表を置く余白は無い。
;          実機のOSも表はRAMに作る — ブートセクタに焼く方が例外だった)
;          LTR → iretd で**わざとリング3へ降りる**
;          (降りる専用命令は無い。「戻る」命令で行った事のない場所へ戻る)
;   ring3: 目印を書く → int 0x30 でリング0を呼ぶ
;   ring0: ゲートで受ける。**スタックはTSSのSS0:ESP0に差し替わっている**
;          目印を書いて iretd (外側リングへの復帰 = ESP,SSも取り出す)
;   ring3: 帰還の目印を書く → int 0x31 (終了の門) → ring0で HLT
;
; メモリ配置 (RAM):
;   0x0800  TSS (ESP0=0x6000, SS0=0x10)
;   0x1000  IDT (0x40本ぶんの枠。0x30と0x31だけ実体、他はP=0の空)
;   0x5000  ring3のスタック / 0x6000 ring0のスタック
;
; アセンブル: nasm -f bin -o pm_ring.bin pm_ring.asm

org 0x7C00
bits 16

start:
    cli
    xor  ax, ax
    mov  ds, ax
    lgdt [gdtr]
    lidt [idtr]
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

    ; TSS: リング0へ落ちた瞬間に使うスタックの2項目だけ書く
    mov  dword [0x0800 + 4], 0x6000              ; ESP0
    mov  dword [0x0800 + 8], 0x10                ; SS0

    ; IDTの門を2つ、RAMに直接組む。ゲート記述子8バイトの並びは
    ;   [off15:0 | sel]  [off31:16 | 属性 | 0]
    ; 属性 0xEE00 = P=1 **DPL=3** 32bit割り込みゲート。
    ; DPL=3が要点 — 門の許可が3でなければ、リング3は int を打てない
    mov  dword [0x1000 + 0x30*8],     (0x08 << 16) | (svc_handler - $$ + 0x7C00)
    mov  dword [0x1000 + 0x30*8 + 4], ((svc_handler - $$ + 0x7C00) & 0xFFFF0000) | 0xEE00
    mov  dword [0x1000 + 0x31*8],     (0x08 << 16) | (exit_handler - $$ + 0x7C00)
    mov  dword [0x1000 + 0x31*8 + 4], ((exit_handler - $$ + 0x7C00) & 0xFFFF0000) | 0xEE00

    mov  ax, 0x28
    ltr  ax                          ; TR に TSS を積む (0F 00 /3)

    ; iretd の皮を被った「リング3への降下」。
    ; 外側リングへの iretd は EIP, CS, EFLAGS に加えて **ESP, SS も取り出す**。
    ; その5つを積んでおけば、行ったことのない場所へ「戻れる」
    push dword 0x23                  ; SS  (ring3 data, RPL=3)
    push dword 0x5000                ; ESP (ring3のスタック)
    push dword 0x0002                ; EFLAGS
    push dword 0x1B                  ; CS  (ring3 code, RPL=3)
    push dword ring3                 ; EIP
    iretd

ring3:
    mov  ax, 0x23                    ; ring3のデータセグメント (DPL=3なので持てる)
    mov  ds, ax
    mov  dword [0x500], 0x00003333   ; 目印: リング3に居る
    int  0x30                        ; カーネル呼び出し
    ; 帰還後のDSは**ヌルに落とされている** — 外側リングへのiretdは、そこでは
    ; 持てないデータセグメントを黙って没収する (実CPUの仕様)。積み直してから書く。
    ; 以前はここを省いても動いた = エミュレータがヌルDSの使用を咎めていなかった
    mov  ax, 0x23
    mov  ds, ax
    mov  dword [0x508], 0x00BAC703   ; 目印: リング0から帰ってきた
    int  0x31                        ; 終了の門

; ---- リング0の受け手 ----
svc_handler:
    ; ここはリング0。スタックはTSSの 0x10:0x6000 に差し替わっていて、
    ; [EIP, CS, EFLAGS, ESP, SS] の5つが積まれている
    mov  ax, 0x10
    mov  ds, ax
    mov  dword [0x504], 0x0000C0DE   ; 目印: リング0で受けた
    iretd

exit_handler:
    hlt

; ---- GDT ----
align 8
gdt:
    dq 0
    dw 0xFFFF, 0x0000                ; 0x08: ring0 code
    db 0x00, 0x9A, 0xCF, 0x00
    dw 0xFFFF, 0x0000                ; 0x10: ring0 data
    db 0x00, 0x92, 0xCF, 0x00
    dw 0xFFFF, 0x0000                ; 0x18|3=0x1B: ring3 code (access 0xFA: DPL=3)
    db 0x00, 0xFA, 0xCF, 0x00
    dw 0xFFFF, 0x0000                ; 0x20|3=0x23: ring3 data (access 0xF2)
    db 0x00, 0xF2, 0xCF, 0x00
    dw 103                           ; 0x28: TSS (32bit TSSの定義サイズ104-1)
    dw 0x0800                        ; base = RAMのTSS
    db 0x00
    db 0x89                          ; P=1 DPL=0 32bit TSS (available)
    db 0x00, 0x00
gdt_end:

gdtr:
    dw gdt_end - gdt - 1
    dd gdt

idtr:
    dw 0x40*8 - 1                    ; 0x40本ぶんの枠 (RAM側。空はP=0)
    dd 0x1000

times 510-($-$$) db 0
dw 0xAA55
