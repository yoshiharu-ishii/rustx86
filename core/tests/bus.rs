//! バス (アドレスの振り分け) のテスト。
//!
//! x86にはアドレス空間が2つある。その**どちらも「番地から宛先を決める」だけ**で、
//! 間に立って経路を決めるブリッジは要らない — 16bit時代はアドレスが定数として
//! 焼かれているためである。ここではその振り分けが正しいことを確かめる。

use rustx86_core::bus::{
    decode_io, decode_mem, IoTarget, MemRegion, TEXT_COLS, TEXT_LEN, VRAM_TEXT_BASE,
};
use rustx86_core::Machine;

fn load(name: &str) -> Machine {
    let path = format!("{}/../asm/{name}", env!("CARGO_MANIFEST_DIR"));
    let sector = std::fs::read(&path).unwrap_or_else(|e| panic!("{path} ({e})"));
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    m
}

/// IBM PCは1MBを「下位640KBはRAM、上位384KBは装置とROMの窓」と区切った。
/// この線引きが後年の「640KBの壁」になる
#[test]
fn memory_space_is_split_at_640kb() {
    assert_eq!(decode_mem(0x00000), MemRegion::Ram);
    assert_eq!(decode_mem(0x7C00), MemRegion::Ram, "ブートセクタはRAM側");
    assert_eq!(decode_mem(0x9FFFF), MemRegion::Ram, "640KBの末尾まではRAM");
    assert_eq!(decode_mem(0xA0000), MemRegion::VideoGraphics, "ここから装置の窓");
    assert_eq!(decode_mem(0xB0000), MemRegion::VideoMono);
    assert_eq!(decode_mem(0xB8000), MemRegion::VideoText, "カラーテキスト画面");
    assert_eq!(decode_mem(0xBFFFF), MemRegion::VideoText);
    assert_eq!(decode_mem(0xC0000), MemRegion::Rom);
    assert_eq!(decode_mem(0xF0000), MemRegion::Rom, "システムBIOSの居場所");
}

/// リアルモードのアドレスは20bitで折り返す (A20が無かった時代の挙動)
#[test]
fn memory_decode_wraps_at_one_megabyte() {
    assert_eq!(decode_mem(0x10_0000), MemRegion::Ram, "1MBちょうどは0番地へ折り返す");
    assert_eq!(decode_mem(0x10_B800), MemRegion::Ram, "0x0B800 へ折り返すのでRAM側");
    assert_eq!(decode_mem(0x1B_8000), MemRegion::VideoText, "0xB8000 へ折り返す");
}

/// ポート番号がばらばらなのは設計ではなく履歴。
/// IBM PCが装置を足していった順に、空いている番地を割り当てた結果である
#[test]
fn io_ports_route_to_their_devices() {
    assert_eq!(decode_io(0x20), IoTarget::Pic { slave: false });
    assert_eq!(decode_io(0x21), IoTarget::Pic { slave: false });
    assert_eq!(decode_io(0xA0), IoTarget::Pic { slave: true }, "スレーブPIC");
    assert_eq!(decode_io(0x40), IoTarget::Pit);
    assert_eq!(decode_io(0x43), IoTarget::Pit);
    assert_eq!(decode_io(0x60), IoTarget::Keyboard);
    assert_eq!(decode_io(0x64), IoTarget::Keyboard);
    assert_eq!(decode_io(0x3F8), IoTarget::Uart, "COM1");
    assert_eq!(decode_io(0x3FF), IoTarget::Uart);
    assert_eq!(decode_io(0x0000), IoTarget::Unmapped);
    assert_eq!(decode_io(0xFFFF), IoTarget::Unmapped);
}

/// `OUT` が正しい装置に届く
#[test]
fn out_reaches_the_right_device() {
    let mut m = Machine::new();
    m.io_write8(0x21, 0xFE); // PICマスタの割り込みマスク
    m.io_write8(0xA1, 0xFD); // PICスレーブ
    m.io_write8(0x43, 0x36); // PITの制御 (カウンタ0、LoHi、モード3)
    m.io_write8(0x3F8, b'X'); // UARTの送信

    assert_eq!(m.devices.pic[0].imr, 0xFE);
    assert_eq!(m.devices.pic[1].imr, 0xFD, "スレーブは別のチップ");
    assert_eq!(m.devices.pit.counters[0].mode, 3);
    assert_eq!(m.devices.uart.tx, b"X");
}

/// 未接続のポートは 0xFF を返す。
///
/// 実機のISAバスは誰もドライブしないとプルアップで全ビットが立ち、
/// OSはこれを見て「装置が居ない」と判断する。**ここで panic すると
/// 装置探索の段階で止まってしまう**ので、値は返しつつ番号だけ覚えておく
#[test]
fn unmapped_ports_read_as_all_ones_and_are_recorded() {
    let mut m = Machine::new();
    assert_eq!(m.io_read8(0x1234), 0xFF);
    m.io_write8(0x5678, 0x42);

    assert!(m.unhandled_io.contains(&0x1234), "読みも記録される");
    assert!(m.unhandled_io.contains(&0x5678), "書きも記録される");
    assert!(!m.unhandled_io.contains(&0x21), "対応済みのポートは記録しない");
}

/// 16bitのI/Oは連続する2ポートに割れる (装置がまたがることもある)
#[test]
fn word_io_spans_two_ports() {
    let mut m = Machine::new();
    // 0x3F8 = UARTの送信、0x3F9 = 割り込み許可。1命令で別レジスタ2つに届く
    m.io_write16(0x3F8, 0x0341);
    assert_eq!(m.devices.uart.tx, b"A", "下位バイトが先のポートへ");
    assert_eq!(m.devices.uart.ier, 0x03, "上位バイトが次のポートへ");
}

/// テキストVRAMは**メモリ空間に居座る装置**である。
/// DOSやUNIXのコンソールはBIOSを呼ばず、ここへ直接書く
#[test]
fn guest_writes_straight_into_text_vram() {
    let mut m = load("vram.bin");
    let executed = m.run(10_000);
    assert!(m.halted, "HLT到達せず ({executed}命令)");

    assert_eq!(m.text_screen_string(), "BUS\nOK");

    // 生バイト列でも確かめる: 2バイトで1文字 (文字コード + 属性)
    let v = m.text_vram();
    assert_eq!(&v[0..6], b"B\x0FU\x0FS\x0F", "文字と属性が交互に並ぶ");
    // 2行目は 80桁 × 2バイト 先。行がメモリ上で連続している
    let row2 = TEXT_COLS * 2;
    assert_eq!(&v[row2..row2 + 4], b"O\x0FK\x0F");
    assert_eq!(v.len(), TEXT_LEN);
}

/// 描画側への合図。書き込みで立ち、読んだら下りる
#[test]
fn vram_writes_raise_the_dirty_flag() {
    let mut m = Machine::new();
    assert!(!m.take_vram_dirty(), "初期状態では立っていない");

    m.write8(0x1000, b'x');
    assert!(!m.take_vram_dirty(), "RAMへの書き込みでは立たない");

    m.write8(VRAM_TEXT_BASE, b'A');
    assert!(m.take_vram_dirty(), "VRAMへの書き込みで立つ");
    assert!(!m.take_vram_dirty(), "読んだら下りる");
}
