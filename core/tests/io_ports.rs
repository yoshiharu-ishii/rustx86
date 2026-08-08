//! `IN`/`OUT` 命令のテスト。
//!
//! この4形式だけはco-simで検証できない。Unicornはポートアクセスをフックで
//! 外部に委ねる設計で、フックを付けなければ挙動が未定義になるため、
//! 「オラクルの正解」が存在しない。ポートの中身はそもそも装置が決めるものであり、
//! **CPU側の責務はポート番号の算出と幅の扱いに尽きる**。そこを直接叩く。
//!
//! 振り分けそのもの (どのポートがどの装置か) は `bus.rs` のテストで見ている。
//! 装置の中身は Tier 2b。

use rustx86_core::cpu::{AX, DX};
use rustx86_core::Machine;

/// CS:IP=0000:0500 にコードを置いて走らせる (0x0000-0x03FF はIVTなので避ける)
fn run(code: &[u8], setup: impl FnOnce(&mut Machine)) -> Machine {
    const BASE: u32 = 0x0500;
    let mut m = Machine::new();
    for (i, b) in code.iter().enumerate() {
        m.write8(BASE + i as u32, *b);
    }
    m.cpu.set_cs_ip(0, BASE as u16);
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
    // MOV AL,0x5A / OUT 0x21,AL   — 0x21 は 8259 PIC の割り込みマスクレジスタ
    let m = run(&[0xB0, 0x5A, 0xE6, 0x21], |_| {});
    assert_eq!(m.devices.pic[0].imr, 0x5A);
}

#[test]
fn in_imm8_reads_port() {
    // IN AL,0x21  — PICのマスクを読み返す
    let m = run(&[0xE4, 0x21], |m| m.devices.pic[0].imr = 0xC3);
    assert_eq!(m.cpu.regs[AX] as u8, 0xC3);
}

#[test]
fn dx_form_uses_dx_as_port_number() {
    // MOV DX,0x03F8 / IN AL,DX   — 0x3F8 は COM1 (UART 16550) の受信レジスタ。
    // imm8形式では8bitしか指定できないので、0xFF を超えるポートはDX形式が要る
    let m = run(&[0xBA, 0xF8, 0x03, 0xEC], |m| m.devices.uart.feed(b"A"));
    assert_eq!(m.cpu.regs[AX] as u8, b'A');
    assert_eq!(m.cpu.regs[DX] as u16, 0x03F8);
}

#[test]
fn word_io_spans_two_consecutive_ports() {
    // MOV AX,0x0341 / MOV DX,0x3F8 / OUT DX,AX
    // 0x3F8 = 送信、0x3F9 = 割り込み許可。1命令で別レジスタ2つに届く
    let m = run(&[0xB8, 0x41, 0x03, 0xBA, 0xF8, 0x03, 0xEF], |_| {});
    assert_eq!(m.devices.uart.tx, b"A", "下位バイトが先のポートへ");
    assert_eq!(m.devices.uart.ier, 0x03, "上位バイトが次のポートへ");
}

#[test]
fn word_in_reassembles_little_endian() {
    // IN AX,0x21  — 0x21 (PICマスク) と 0x22 (未接続=0xFF) を跨ぐ
    let m = run(&[0xE5, 0x21], |m| m.devices.pic[0].imr = 0x34);
    assert_eq!(m.cpu.regs[AX] as u16, 0xFF34, "上位は未接続ポートの0xFF");
}

/// OUTがALだけを使い、AHを巻き込まないこと (8bit形式の幅の扱い)
#[test]
fn byte_out_ignores_ah() {
    // MOV AX,0xFF7E / OUT 0x21,AL
    let m = run(&[0xB8, 0x7E, 0xFF, 0xE6, 0x21], |_| {});
    assert_eq!(m.devices.pic[0].imr, 0x7E);
    assert!(!m.unhandled_io.contains(&0x22), "8bit OUTが隣のポートに触れていない");
}

/// 未接続のポートを読むと 0xFF が返り、番号が記録される。
/// OSはこの値で装置の有無を探るので、panicしてはいけない
#[test]
fn probing_an_absent_device_reads_all_ones() {
    // IN AL,0x80  — 0x80 は DMAページレジスタ (未実装)
    let m = run(&[0xE4, 0x80], |_| {});
    assert_eq!(m.cpu.regs[AX] as u8, 0xFF);
    assert!(m.unhandled_io.contains(&0x80), "触られた番号が残る");
}
