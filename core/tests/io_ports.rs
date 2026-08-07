//! IN/OUT (I/Oポート空間) のテスト。
//!
//! この4形式だけはco-simで検証できない。Unicornはポートアクセスを
//! フックで外部に委ねる設計で、フックを付けなければ挙動が未定義になるため、
//! 「オラクルの正解」が存在しない。ポートの中身はそもそも装置が決めるものであり、
//! CPU側の責務は**ポート番号の算出と幅の扱い**に尽きる。そこを直接叩く。
//!
//! 装置そのものの検証は Tier 2a (PIC/PIT/UART) で、実機の初期化列を
//! 流して行う。

use rustx86_core::cpu::{AX, DX};
use rustx86_core::Machine;

/// CS:IP=0000:0000 にコードを置いて指定命令数だけ走らせる
fn run(code: &[u8], setup: impl FnOnce(&mut Machine)) -> Machine {
    let mut m = Machine::new();
    for (i, b) in code.iter().enumerate() {
        m.write8(i as u32, *b);
    }
    m.cpu.set_cs_ip(0, 0);
    setup(&mut m);
    for _ in 0..code.len() {
        if m.halted {
            break;
        }
        m.step();
    }
    m
}

#[test]
fn out_imm8_writes_port() {
    // MOV AL,0x5A / OUT 0x21,AL   — 0x21 は 8259 PIC のマスクレジスタ
    let m = run(&[0xB0, 0x5A, 0xE6, 0x21], |_| {});
    assert_eq!(m.ports[0x21], 0x5A);
}

#[test]
fn in_imm8_reads_port() {
    // IN AL,0x40  — 0x40 は 8254 PIT のカウンタ0
    let m = run(&[0xE4, 0x40], |m| m.ports[0x40] = 0xC3);
    assert_eq!(m.cpu.regs[AX] as u8, 0xC3);
}

#[test]
fn dx_form_uses_dx_as_port_number() {
    // MOV DX,0x03F8 / IN AL,DX   — 0x3F8 は COM1 (UART 16550) のデータレジスタ。
    // imm8形式では8bitしか指定できないので、0xFF を超えるポートはDX形式が要る
    let m = run(&[0xBA, 0xF8, 0x03, 0xEC], |m| m.ports[0x3F8] = b'A');
    assert_eq!(m.cpu.regs[AX] as u8, b'A');
    assert_eq!(m.cpu.regs[DX] as u16, 0x03F8);
}

#[test]
fn word_io_spans_two_consecutive_ports() {
    // MOV AX,0xBEEF / OUT DX,AX (DX=0x100)
    let m = run(&[0xB8, 0xEF, 0xBE, 0xBA, 0x00, 0x01, 0xEF], |_| {});
    assert_eq!(m.ports[0x100], 0xEF, "下位バイトが先のポートへ");
    assert_eq!(m.ports[0x101], 0xBE, "上位バイトが次のポートへ");
}

#[test]
fn word_in_reassembles_little_endian() {
    // IN AX,0x60  — 0x60 はキーボードコントローラ
    let m = run(&[0xE5, 0x60], |m| {
        m.ports[0x60] = 0x34;
        m.ports[0x61] = 0x12;
    });
    assert_eq!(m.cpu.regs[AX] as u16, 0x1234);
}

/// OUTがALだけを使い、AHを巻き込まないこと (8bit形式の幅の扱い)
#[test]
fn byte_out_ignores_ah() {
    // MOV AX,0xFF7E / OUT 0x80,AL
    let m = run(&[0xB8, 0x7E, 0xFF, 0xE6, 0x80], |_| {});
    assert_eq!(m.ports[0x80], 0x7E);
    assert_eq!(m.ports[0x81], 0x00, "8bit OUTが隣のポートを汚していない");
}
