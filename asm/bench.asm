; 実行速度ベンチマーク用のワークロード (リアルモード 8086)
;
; 目的は「インタプリタが1秒あたり何命令こなせるか」の再現可能な測定。
; 装置や割り込みを足した後に同じものを流し、劣化を見るための基準線になる。
;
; 設計方針:
;
; - **1命令のループにしない**。`inc ax` を延々回すようなループは分岐予測にも
;   命令キャッシュにも都合が良すぎて、実際のOSの実行速度とかけ離れる。
;   ALU・メモリ・シフト・スタック・比較・分岐を混ぜた10命令の本体を回す
; - **HLTで必ず止まる**。上限命令数で打ち切られると測定値がぶれるため、
;   ワークロード側が終端を持つ
; - 命令数は固定 (下記OUTER × INNER × 本体長)。実行時間ではなく
;   命令数が定数なので、環境が変わっても MIPS の比較ができる
;
; アセンブル: nasm -f bin -o bench.bin bench.asm
; 実行:       cargo run --release --example bench -- asm/bench.bin

org 0x7C00
bits 16

OUTER equ 512                   ; 外側ループ回数
INNER equ 0xFFFF                ; 内側ループ回数 (LOOP命令の最大)

start:
    xor  ax, ax
    mov  ds, ax
    mov  es, ax
    mov  ss, ax
    mov  sp, 0x7C00             ; スタックはコードの直下に置く
    mov  dx, 0x1234             ; 適当な初期値 (毎周変化させる種)

    mov  bp, OUTER
.outer:
    mov  cx, INNER
.inner:
    ; --- 本体: 命令の種類を散らす ---
    mov  ax, [scratch]          ; メモリ読み (moffs形式)
    add  ax, dx                 ; ALU reg,reg
    xor  bx, ax                 ; ALU reg,reg
    shl  ax, 1                  ; シフト
    mov  [scratch], ax          ; メモリ書き
    inc  dx                     ; INC (CFを変えない特殊系)
    push ax                     ; スタック書き
    pop  di                     ; スタック読み
    cmp  di, bx                 ; 比較
    jne  .skip                  ; 条件分岐 (ほぼ成立する)
    nop
.skip:
    loop .inner                 ; CX-- して非ゼロなら継続

    dec  bp
    jnz  .outer

    ; --- 終端。「HLTで必ず止まる」の約束をここで守る ---
    ;
    ; BIOS相当がPIT ch0を回しているので、素のHLTは次のタイマ割り込みで
    ; 起こされ、IRETで**HLTの次**から再開してしまう。その先はデータと
    ; ゼロ埋めで、IPは64KBを一周してワークロードを最初からやり直す —
    ; 上限20Gまで走り続ける機械が実際にできた (測定値が全部嘘になる)。
    ; 制御語だけ書いてカウントを積まなければ 8254 は止まる。
    ; 起こされても寝直すループで、どの割り込みが残っていても必ず沈む
    mov  al, 0x30               ; ch0, lo/hi, mode 0 — カウント再装填まで停止
    out  0x43, al
.halt:
    hlt
    jmp  .halt

scratch: dw 0

times 510-($-$$) db 0
dw 0xAA55                       ; ブートシグネチャ
