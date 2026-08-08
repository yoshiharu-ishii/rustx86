//! BIOS サービスのテスト。
//!
//! ELKSは8042やVRAMを**直接**触るので、BIOSが薄くても動いていた。
//! DOSのようにBIOS越しに触るOSでは、ここが本番になる。
//! FreeDOSを載せる前に、イメージ無しで確かめられる範囲を固めておく。

use rustx86_core::bus::{TEXT_COLS, VRAM_TEXT_BASE};
use rustx86_core::cpu::{AX, BX, CX, DX, ZF};
use rustx86_core::Machine;

/// INT を1つ呼んで、BIOS HLE が終わるまで進める
fn call_int(m: &mut Machine, n: u8, max: u32) {
    // CS:IP=0000:0500 に INT n; HLT を置く
    const CODE: u32 = 0x0500;
    m.write8(CODE, 0xCD);
    m.write8(CODE + 1, n);
    m.write8(CODE + 2, 0xF4);
    m.cpu.set_cs_ip(0, CODE as u16);
    m.halted = false;
    for _ in 0..max {
        m.step();
        if m.halted {
            return;
        }
    }
}

fn machine() -> Machine {
    let sector = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../asm/hello.bin")).unwrap();
    let mut m = Machine::new();
    m.load_boot_sector(&sector).unwrap();
    m.cpu.regs[4] = 0x7C00; // SP
    m
}

fn cell(m: &Machine, row: usize, col: usize) -> (u8, u8) {
    let a = VRAM_TEXT_BASE + ((row * TEXT_COLS + col) * 2) as u32;
    (m.read8(a), m.read8(a + 1))
}

fn put(m: &mut Machine, row: usize, col: usize, ch: u8, attr: u8) {
    let a = VRAM_TEXT_BASE + ((row * TEXT_COLS + col) * 2) as u32;
    m.write8(a, ch);
    m.write8(a + 1, attr);
}

// ---------- INT 16h キーボード ----------

/// スキャンコードをASCIIに直すのは**BIOSの仕事**。
/// 装置が返すのはキーの位置で、対応表はファームウェアが持つ
#[test]
fn int16_translates_scancodes_to_ascii() {
    let mut m = machine();
    m.devices.keyboard.type_ascii("A");
    m.cpu.regs[AX] = 0x0000; // AH=00: 待って1つ取る
    call_int(&mut m, 0x16, 100_000);
    assert_eq!(m.cpu.regs[AX] as u8, b'A', "AL に ASCII");
    assert_eq!((m.cpu.regs[AX] >> 8) as u8, 0x1E, "AH にスキャンコード (Aの位置)");
}

/// 記号もShiftの上げ下げを解釈して組み立てる
#[test]
fn int16_handles_shifted_symbols() {
    for (ch, want) in [('@', b'@'), (':', b':'), ('!', b'!'), ('a', b'a')] {
        let mut m = machine();
        m.devices.keyboard.type_ascii(&ch.to_string());
        m.cpu.regs[AX] = 0x0000;
        call_int(&mut m, 0x16, 100_000);
        assert_eq!(m.cpu.regs[AX] as u8, want, "{ch:?} が取れない");
    }
}

/// **キーが無ければ待つ。** IRETせずに戻ることでINTがやり直され、
/// 実BIOSが割り込みを待って回っているのと同じ状態になる
#[test]
fn int16_blocks_until_a_key_arrives() {
    let mut m = machine();
    m.cpu.regs[AX] = 0x0000;
    call_int(&mut m, 0x16, 5_000);
    assert!(!m.halted, "キーが無いのに先へ進んでしまった");

    // 後からキーを入れると進む
    m.devices.keyboard.type_ascii("z");
    for _ in 0..100_000 {
        m.step();
        if m.halted {
            break;
        }
    }
    assert!(m.halted, "キーが来ても進まない");
    assert_eq!(m.cpu.regs[AX] as u8, b'z');
}

/// AH=01 は取らずに覗く。無ければ ZF=1
#[test]
fn int16_peek_does_not_consume() {
    let mut m = machine();
    m.cpu.regs[AX] = 0x0100;
    call_int(&mut m, 0x16, 10_000);
    assert!(m.cpu.flag(ZF), "空なのに ZF が立っていない");

    m.devices.keyboard.type_ascii("k");
    m.cpu.regs[AX] = 0x0100;
    call_int(&mut m, 0x16, 10_000);
    assert!(!m.cpu.flag(ZF), "キーがあるのに ZF が立っている");
    assert_eq!(m.cpu.regs[AX] as u8, b'k');

    // 覗いただけなので、取れば同じものが出る
    m.cpu.regs[AX] = 0x0000;
    call_int(&mut m, 0x16, 100_000);
    assert_eq!(m.cpu.regs[AX] as u8, b'k', "覗いたキーが消えている");
}

// ---------- INT 10h 画面 ----------

/// AH=06 で範囲を1行上へずらす。**DOSの改行はここを通る**
#[test]
fn int10_scrolls_a_window_up() {
    let mut m = machine();
    for (row, ch) in [(0, b'a'), (1, b'b'), (2, b'c')] {
        put(&mut m, row, 0, ch, 0x07);
    }
    m.cpu.regs[AX] = 0x0601; // AH=06 AL=1行
    m.cpu.regs[BX] = 0x0700; // 埋める属性
    m.cpu.regs[CX] = 0x0000; // 左上 (0,0)
    m.cpu.regs[DX] = 0x024F; // 右下 (2,79)
    call_int(&mut m, 0x10, 10_000);

    assert_eq!(cell(&m, 0, 0).0, b'b', "1行上がっていない");
    assert_eq!(cell(&m, 1, 0).0, b'c');
    assert_eq!(cell(&m, 2, 0), (b' ', 0x07), "空いた行が埋まっていない");
}

/// AL=0 は「範囲を丸ごと空白にする」。画面クリアはこの形で来る
#[test]
fn int10_clears_a_window_when_lines_is_zero() {
    let mut m = machine();
    for row in 0..3 {
        put(&mut m, row, 0, b'x', 0x07);
    }
    m.cpu.regs[AX] = 0x0600;
    m.cpu.regs[BX] = 0x1F00; // 青地に白
    m.cpu.regs[CX] = 0x0000;
    m.cpu.regs[DX] = 0x024F;
    call_int(&mut m, 0x10, 10_000);
    for row in 0..3 {
        assert_eq!(cell(&m, row, 0), (b' ', 0x1F), "{row}行目が消えていない");
    }
}

/// AH=09 はカーソル位置に文字と属性を繰り返し置く
#[test]
fn int10_writes_char_with_attribute() {
    let mut m = machine();
    m.cpu.regs[AX] = 0x092A; // AH=09 AL='*'
    m.cpu.regs[BX] = 0x4E00; // 赤地に黄
    m.cpu.regs[CX] = 3;
    call_int(&mut m, 0x10, 10_000);
    for col in 0..3 {
        assert_eq!(cell(&m, 0, col), (b'*', 0x4E), "{col}桁目");
    }
    assert_ne!(cell(&m, 0, 3).0, b'*', "指定より多く書いている");
}

/// AH=08 はカーソル位置の文字と属性を読む
#[test]
fn int10_reads_char_at_cursor() {
    let mut m = machine();
    put(&mut m, 0, 0, b'Z', 0x1C);
    m.cpu.regs[AX] = 0x0800;
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(m.cpu.regs[AX] as u8, b'Z');
    assert_eq!((m.cpu.regs[AX] >> 8) as u8, 0x1C);
}

/// AH=13 は文字列をまとめて置く
#[test]
fn int10_writes_a_string() {
    let mut m = machine();
    const SRC: u32 = 0x0600;
    for (i, c) in b"HI".iter().enumerate() {
        m.write8(SRC + i as u32, *c);
    }
    m.cpu.sregs[0] = 0; // ES
    m.cpu.regs[5] = SRC as u32; // BP
    m.cpu.regs[AX] = 0x1300;
    m.cpu.regs[BX] = 0x0A00; // 明るい緑
    m.cpu.regs[CX] = 2;
    m.cpu.regs[DX] = 0x0105; // 1行目 5桁目
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(cell(&m, 1, 5), (b'H', 0x0A));
    assert_eq!(cell(&m, 1, 6), (b'I', 0x0A));
}
