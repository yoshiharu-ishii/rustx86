; BOUNCE.COM — VGA mode 13h でボールが跳ね回る。
;
;   nasm -f bin bounce.asm -o BOUNCE.COM
;
; 1990年前後のDOSゲームの骨格をそのまま小さくしたもの:
;   1. INT 10h AX=0013h で 320x200x256色へ
;   2. DAC (0x3C8/0x3C9) に自分のパレットを流し込む
;   3. 毎フレーム 0x3DA の垂直帰線を待ってから描き換える (ちらつき防止 + テンポ)
;   4. 0xA0000 へ直接バイトを置く (BIOSは呼ばない — 遅いから)
;   5. INT 16h AH=01 でキーを覗き、押されたらテキストモードへ戻って INT 20h
; エミュレータにとっては、合成した垂直帰線でゲームのテンポが
; 正しく決まるかの実地試験でもある (70Hz → 1秒に70回描き換わる)。
	org 0x100

BALL	equ 10			; ボールの直径 (画素)
COL_BALL equ 32			; パレットの色番号 (自分で定義する側)
COL_WALL equ 33
COL_BG	 equ 0

start:
	mov ax, 0x0013
	int 0x10
	mov ax, 0xA000
	mov es, ax

	; --- パレット: 32=橙 (ボール) 33=藍 (壁)。6bit値 ---
	mov dx, 0x3C8
	mov al, COL_BALL
	out dx, al
	inc dx
	mov al, 63		; R
	out dx, al
	mov al, 30		; G
	out dx, al
	xor al, al		; B
	out dx, al		; → 自動歩進で 33 へ
	mov al, 16		; R
	out dx, al
	mov al, 16		; G
	out dx, al
	mov al, 50		; B
	out dx, al

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

	mov word [x], 60
	mov word [y], 40
	mov word [vx], 2
	mov word [vy], 1

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

	; --- 前の位置を消す ---
	mov al, COL_BG
	call draw_ball

	; --- 動かす。壁 (1画素) に当たったら向きを反転 ---
	mov ax, [x]
	add ax, [vx]
	cmp ax, 1
	jg .x_lo_ok
	neg word [vx]
	mov ax, 1
.x_lo_ok:
	cmp ax, 320-1-BALL
	jl .x_hi_ok
	neg word [vx]
	mov ax, 320-1-BALL
.x_hi_ok:
	mov [x], ax

	mov ax, [y]
	add ax, [vy]
	cmp ax, 1
	jg .y_lo_ok
	neg word [vy]
	mov ax, 1
.y_lo_ok:
	cmp ax, 200-1-BALL
	jl .y_hi_ok
	neg word [vy]
	mov ax, 200-1-BALL
.y_hi_ok:
	mov [y], ax

	; --- 新しい位置に描く ---
	mov al, COL_BALL
	call draw_ball

	; --- キーを覗く。押されていなければ次のフレームへ ---
	mov ah, 0x01
	int 0x16
	jz main
	xor ah, ah		; 押されたキーを取り除く (DOSに残さない)
	int 0x16

	mov ax, 0x0003		; テキストモードへ戻す (画面も消える)
	int 0x10
	int 0x20

; AL の色で (x,y) に丸を描く。形は行ごとの (左の空き, 幅) の表
draw_ball:
	push ax
	mov ax, [y]
	mov bx, 320
	mul bx			; AX = y*320 (DXを壊す — mainが毎周 0x3DA を入れ直す)
	add ax, [x]
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

x:	dw 0
y:	dw 0
vx:	dw 0
vy:	dw 0
