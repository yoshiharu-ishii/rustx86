; AIR.COM — G線上のアリア (Bach, BWV 1068) をPCスピーカーで演奏する。
;
;   nasm -f bin air.asm -o AIR.COM
;
; 音の出し方は1980年代のBASICの PLAY 文と同じ: PITカウンタ2に分周値を
; 書いて矩形波を作り、ポート0x61のbit0/1でスピーカーへ通す。
; 音符の長さはBIOSのティック (INT 1Ah、18.2Hz) で数える。待つあいだは
; HLT で寝る — 割り込みで起きて数え直す行儀のよい待ち方で、
; エミュレータ側のアイドル早送りもそのまま効く。
; 何かキーを押すと途中でやめる。
	org 0x100

start:
	mov si, notes
next_note:
	lodsw			; AX = PIT分周値 (0=休符, 0xFFFF=終端)
	cmp ax, 0xFFFF
	je done
	mov bx, ax
	lodsw			; AX = 長さ (BIOSティック)
	mov bp, ax
	test bx, bx
	jz rest
	mov al, 0xB6		; ch2, LoHi, モード3 (矩形波)
	out 0x43, al
	mov al, bl
	out 0x42, al
	mov al, bh
	out 0x42, al
	in al, 0x61
	or al, 0x03		; ゲートとスピーカーを開く
	out 0x61, al
	jmp wait_ticks
rest:
	in al, 0x61
	and al, 0xFC
	out 0x61, al

wait_ticks:			; BPティックだけ待つ
	call read_tick
	mov di, dx		; 開始ティック (下位ワードで足りる: 曲は数分)
.wait:
	hlt			; 次の割り込み (たいていIRQ0) まで寝る
	mov ah, 0x01		; キーが来ていたら演奏をやめる
	int 0x16
	jnz stop
	call read_tick
	sub dx, di		; 巻き戻り (深夜0時) は unsigned 減算が吸収する
	cmp dx, bp
	jb .wait
	jmp next_note

stop:
	mov ah, 0x00		; 押されたキーは食べておく (DOSに漏らさない)
	int 0x16
done:
	in al, 0x61		; 帰る前に必ず黙る
	and al, 0xFC
	out 0x61, al
	int 0x20

read_tick:			; DX = BIOSティックカウンタ下位
	xor ah, ah
	int 0x1a
	ret

notes:
%include "notes.inc"
