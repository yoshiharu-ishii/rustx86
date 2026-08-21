; BOUNCE.COM — VGA mode 13h で色とりどりのボールが跳ね回る。
;
;   nasm -f bin bounce.asm -o BOUNCE.COM
;
; 1990年前後のDOSゲームの骨格をそのまま小さくしたもの:
;   1. INT 10h AX=0013h で 320x200x256色へ
;   2. DAC (0x3C8/0x3C9) に自分のパレット (ボール8色 + 壁) を流し込む
;   3. 毎フレーム 0x3DA の垂直帰線を待ってから描き換える (ちらつき防止 + テンポ)
;   4. 0xA0000 へ直接バイトを置く (BIOSは呼ばない — 遅いから)
;   5. INT 16h AH=01 でキーを覗き、押されたらテキストモードへ戻って INT 20h
; ボールは表 (x, y, vx, vy, 色) で持ち、毎フレーム「全部消す → 全部動かす →
; 全部描く」の3周。重なった瞬間に片方が欠けて見えるのは当時のゲームも同じ。
; エミュレータにとっては、合成した垂直帰線でゲームのテンポが
; 正しく決まるかの実地試験でもある (70Hz → 1秒に70回描き換わる)。
	org 0x100

BALL	equ 10			; ボールの直径 (画素)
NBALLS	equ 8
COL_FIRST equ 32		; ボールの色はパレット 32..39 (自分で定義する側)
COL_WALL equ 40
COL_BG	 equ 0
; 表の1行: x, y, vx, vy, 色 (各ワード)
B_X	equ 0
B_Y	equ 2
B_VX	equ 4
B_VY	equ 6
B_COL	equ 8
B_SIZE	equ 10

start:
	mov ax, 0x0013
	int 0x10
	mov ax, 0xA000
	mov es, ax

	; --- パレット: 32..39 がボール、40 が壁。6bit値の R,G,B が並ぶ表を流し込む ---
	mov dx, 0x3C8
	mov al, COL_FIRST
	out dx, al
	inc dx
	mov si, palette
	mov cx, (NBALLS+1)*3
.pal:
	lodsb
	out dx, al		; Bを書くたびに色番号が自動歩進する
	loop .pal

	; --- 壁: 画面の縁1画素 ---
	mov al, COL_WALL
	xor di, di
	mov cx, 320
	rep stosb		; 上辺
	mov di, 199*320
	mov cx, 320
	rep stosb		; 下辺
	xor di, di
	mov cx, 200
.side:
	mov [es:di], al
	mov [es:di+319], al
	add di, 320
	loop .side

main:
	; --- 垂直帰線を待つ: 帰線中なら抜けるのを待ち、次の帰線の頭で進む ---
	; (「帰線の頭」に揃えるので、1フレームに1回きっかり回る)
	mov dx, 0x3DA
.in_retrace:
	in al, dx
	test al, 8
	jnz .in_retrace
.wait_retrace:
	in al, dx
	test al, 8
	jz .wait_retrace

	; --- 1周目: 全部消す ---
	mov bp, balls
	mov cx, NBALLS
.erase:
	push cx
	mov al, COL_BG
	call draw_ball
	add bp, B_SIZE
	pop cx
	loop .erase

	; --- 2周目: 全部動かす。壁 (1画素) に当たったら向きを反転 ---
	mov bp, balls
	mov cx, NBALLS
.move:
	mov ax, [bp+B_X]
	add ax, [bp+B_VX]
	cmp ax, 1
	jg .x_lo_ok
	neg word [bp+B_VX]
	mov ax, 1
.x_lo_ok:
	cmp ax, 320-1-BALL
	jl .x_hi_ok
	neg word [bp+B_VX]
	mov ax, 320-1-BALL
.x_hi_ok:
	mov [bp+B_X], ax

	mov ax, [bp+B_Y]
	add ax, [bp+B_VY]
	cmp ax, 1
	jg .y_lo_ok
	neg word [bp+B_VY]
	mov ax, 1
.y_lo_ok:
	cmp ax, 200-1-BALL
	jl .y_hi_ok
	neg word [bp+B_VY]
	mov ax, 200-1-BALL
.y_hi_ok:
	mov [bp+B_Y], ax
	add bp, B_SIZE
	loop .move

	; --- 3周目: 全部描く ---
	mov bp, balls
	mov cx, NBALLS
.paint:
	push cx
	mov al, [bp+B_COL]
	call draw_ball
	add bp, B_SIZE
	pop cx
	loop .paint

	; --- キーを覗く。押されていなければ次のフレームへ ---
	mov ah, 0x01
	int 0x16
	jz main
	xor ah, ah		; 押されたキーを取り除く (DOSに残さない)
	int 0x16

	mov ax, 0x0003		; テキストモードへ戻す (画面も消える)
	int 0x10
	int 0x20

; BP が指すボールを AL の色で描く。形は行ごとの (左の空き, 幅) の表
draw_ball:
	push ax
	mov ax, [bp+B_Y]
	mov bx, 320
	mul bx			; AX = y*320 (DXを壊す — mainが毎周 0x3DA を入れ直す)
	add ax, [bp+B_X]
	mov di, ax
	pop ax
	mov si, shape
	mov cx, BALL
.row:
	push cx
	push di
	xor bh, bh
	mov bl, [si]		; 左の空き
	add di, bx
	mov cl, [si+1]		; 幅
	xor ch, ch
	rep stosb
	pop di
	add di, 320
	add si, 2
	pop cx
	loop .row
	ret

; 直径10の丸。(左の空き, 幅) × 10行
shape:	db 3,4, 1,8, 1,8, 0,10, 0,10, 0,10, 0,10, 1,8, 1,8, 3,4

; パレット (6bit)。32=橙 33=赤 34=黄 35=緑 36=水 37=青 38=紫 39=桃、40=藍 (壁)
palette:
	db 63,30, 0
	db 63, 8, 8
	db 63,60, 8
	db 16,60,16
	db 16,60,63
	db 20,30,63
	db 50,16,63
	db 63,40,50
	db 16,16,50

; ボール8個: x, y, vx, vy, 色。速さと向きをばらけさせてある
balls:
	dw  60, 40,  2,  1, 32
	dw 200, 30, -1,  2, 33
	dw 120,150,  3, -1, 34
	dw 250,100, -2, -2, 35
	dw  30,120,  1,  3, 36
	dw 160, 80, -3,  1, 37
	dw 280,170,  2, -3, 38
	dw  90, 60, -1, -1, 39
