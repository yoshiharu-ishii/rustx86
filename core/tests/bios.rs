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
                            // **割り込みを開けておく。** キーは 8042 → IRQ1 → INT 09h → BDAの待ち行列 →
                            // INT 16h という順で届く。実BIOSと同じ経路にしたので、割り込みを止めたままだと
                            // INT 09h が走らず、待ち行列に一文字も積まれない
    m.cpu.set_flag(rustx86_core::cpu::IF, true);
    m
}

/// キーを打ち、**割り込みが処理されて待ち行列に届くまで**進める。
///
/// 押した瞬間に読めるわけではない。IRQ1が上がり、INT 09h が走って初めて
/// BIOSの待ち行列に載る — 実機と同じ順序である
fn type_key(m: &mut Machine, s: &str) {
    m.devices.keyboard.type_ascii(s);
    for _ in 0..20_000 {
        m.step();
    }
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
    type_key(&mut m, "A");
    m.cpu.regs[AX] = 0x0000; // AH=00: 待って1つ取る
    call_int(&mut m, 0x16, 100_000);
    assert_eq!(m.cpu.regs[AX] as u8, b'A', "AL に ASCII");
    assert_eq!(
        (m.cpu.regs[AX] >> 8) as u8,
        0x1E,
        "AH にスキャンコード (Aの位置)"
    );
}

/// 記号もShiftの上げ下げを解釈して組み立てる
#[test]
fn int16_handles_shifted_symbols() {
    for (ch, want) in [('@', b'@'), (':', b':'), ('!', b'!'), ('a', b'a')] {
        let mut m = machine();
        type_key(&mut m, &ch.to_string());
        m.cpu.regs[AX] = 0x0000;
        call_int(&mut m, 0x16, 100_000);
        assert_eq!(m.cpu.regs[AX] as u8, want, "{ch:?} が取れない");
    }
}

/// **キーが無ければ待つ。** IRETせずに戻ることでINTがやり直され、
/// 実BIOSが割り込みを待って回っているのと同じ状態になる。
///
/// 判定にHLTを使わないのは、**BIOSがPITを動かすようにしてから
/// HLTが「永久に止まった」を意味しなくなった**ためである。
/// タイマ割り込みが18.2回/秒で起こしにくる。見るべきは
/// 「キーが取れたか」であって「止まったか」ではない。
#[test]
fn int16_blocks_until_a_key_arrives() {
    let mut m = machine();
    m.cpu.regs[AX] = 0x0000;
    call_int(&mut m, 0x16, 5_000);
    assert_eq!(m.cpu.regs[AX], 0, "キーが無いのに何か取れている");
    assert_eq!(
        m.cpu.sregs[rustx86_core::cpu::CS],
        rustx86_core::BIOS_SEG,
        "キーが無いのにBIOSの入口から先へ進んでしまった"
    );

    // 後からキーを入れると取れる
    m.devices.keyboard.type_ascii("z");
    let mut got = false;
    for _ in 0..200_000 {
        m.step();
        if m.cpu.regs[AX] as u8 == b'z' {
            got = true;
            break;
        }
    }
    assert!(got, "キーが来ても取れない");
}

/// **Ctrl を押しながらだと制御文字になる。**
///
/// Ctrl+A〜Z が 0x01〜0x1A なのは、ASCIIの英大文字が 0x41〜0x5A に並んでいて
/// 上位3ビットを落とすと 1〜26 になるからで、端末が Ctrl+C で止まり
/// Ctrl+D で終わるのはこの引き算の名残でしかない。
///
/// ここを通していなかったので、Ctrlは8042まで届いていたのに文字にする段で
/// 捨てられ、**Ctrl+C がただの `c`** になっていた。
#[test]
fn int16_translates_control_combinations() {
    for (keys, want) in [
        ("\u{3}", 0x03u8), // Ctrl+C
        ("\u{4}", 0x04),   // Ctrl+D
        ("\u{1a}", 0x1a),  // Ctrl+Z
        ("\u{1b}", 0x1b),  // Ctrl+[ は Esc と同じ
    ] {
        let mut m = machine();
        // 8042へは「Ctrlを押す → キーを押す → 離す」の順で流れる
        let (sc, _) = rustx86_core::dev::isa::kbd::scancode_shift(match want {
            0x03 => 'c',
            0x04 => 'd',
            0x1a => 'z',
            _ => '[',
        })
        .expect("スキャンコードがある");
        m.devices.keyboard.feed(&[0x1D, sc, sc | 0x80, 0x9D]);
        for _ in 0..20_000 {
            m.step();
        }
        m.cpu.regs[AX] = 0x0000;
        call_int(&mut m, 0x16, 100_000);
        assert_eq!(
            m.cpu.regs[AX] as u8, want,
            "{keys:?} が {:#04x} になっていない",
            m.cpu.regs[AX] as u8
        );
    }
}

/// Ctrlの状態が **BIOSデータエリアに出る** (AH=02 で読める)
#[test]
fn int16_reports_ctrl_in_the_shift_state() {
    let mut m = machine();
    m.devices.keyboard.feed(&[0x1D]); // Ctrl 押しっぱなし
    for _ in 0..20_000 {
        m.step();
    }
    m.cpu.regs[AX] = 0x0200;
    call_int(&mut m, 0x16, 10_000);
    assert_eq!(
        m.cpu.regs[AX] as u8 & 0x04,
        0x04,
        "Ctrlのビットが立っていない"
    );
}

/// AH=01 は取らずに覗く。無ければ ZF=1
#[test]
fn int16_peek_does_not_consume() {
    let mut m = machine();
    m.cpu.regs[AX] = 0x0100;
    call_int(&mut m, 0x16, 10_000);
    assert!(m.cpu.flag(ZF), "空なのに ZF が立っていない");

    type_key(&mut m, "k");
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

/// **タブは8桁ごとの停留所まで進む。**
///
/// 扱っていなかったので、タブ文字そのもの (CP437では ○) を画面に書いていた
#[test]
fn int10_teletype_advances_to_the_next_tab_stop() {
    let mut m = machine();
    m.set_cursor_pos(0, 3);
    m.cpu.regs[AX] = 0x0E09; // AH=0E AL=タブ
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(m.cursor_pos(), (0, 8), "次の停留所へ行っていない");
    assert_ne!(cell(&m, 0, 3).0, 0x09, "タブ文字そのものを書いている");

    // ちょうど停留所の上なら、次の停留所まで進む
    m.set_cursor_pos(0, 8);
    m.cpu.regs[AX] = 0x0E09;
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(m.cursor_pos(), (0, 16));
}

/// **カーソル位置はBIOSデータエリアにも載る。**
///
/// 実機では CRTC と BDA (0x450) の両方に同じ位置がある。画面はCRTCを見るが、
/// **ソフトはBDAの方を直接読むことがある**。ここを更新していなかったので、
/// FreeDOSからは常に「行0桁0」に見えていて、キーを打つたびに画面の先頭へ
/// カーソルが飛んでいた。
#[test]
fn cursor_position_is_mirrored_in_the_bios_data_area() {
    let mut m = machine();
    m.set_cursor_pos(7, 13);
    assert_eq!(m.read16(0x450), (7 << 8) | 13, "BDAに載っていない");

    // BIOS越しに動かしても同じ
    m.cpu.regs[DX] = (3 << 8) | 21;
    m.cpu.regs[AX] = 0x0200; // AH=02: カーソル移動
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(m.read16(0x450), (3 << 8) | 21);
    assert_eq!(m.cursor_pos(), (3, 21), "CRTC側とずれている");
}

/// **ハードウェアスクロール。**
///
/// テキストVRAMの窓は32KBあり、80x25の1画面はそのうち4000バイトでしかない。
/// どこから表示するかはCRTCのレジスタ 0x0C/0x0D が決めていて、**ここを動かすと
/// メモリを1バイトも書き換えずに画面がスクロールする**。80年代の機械が
/// 遅いCPUで滑らかにスクロールできたのはこの仕組みによる。
///
/// 描く側がこれを見ておらず常に先頭を返していたため、CGA向けにこの手で描く
/// ソフト (zmiy など) は**画面の下が永久に出てこなかった**。CRTCは実装済みで、
/// 説明にも「ここを動かすとスクロールできる」と書いてあったのに、
/// **使う側が読んでいなかった**。
#[test]
fn crtc_start_address_scrolls_the_screen_without_touching_memory() {
    let mut m = machine();
    // 1画面ぶん先 (25行目) に目印を置く
    put(&mut m, 0, 0, b'A', 0x07);
    let below = VRAM_TEXT_BASE + (TEXT_COLS * 25 * 2) as u32;
    m.write8(below, b'B');
    m.write8(below + 1, 0x07);

    assert_eq!(m.text_screen_string().chars().next(), Some('A'));

    // CRTCの開始位置を1画面ぶん (2000文字) 進める。**メモリは触らない**
    m.io_write8(0x3D4, 0x0C);
    m.io_write8(0x3D5, (2000u16 >> 8) as u8);
    m.io_write8(0x3D4, 0x0D);
    m.io_write8(0x3D5, 2000u16 as u8);

    assert_eq!(
        m.text_screen_string().chars().next(),
        Some('B'),
        "開始位置を動かしても画面が変わらない"
    );
    assert!(
        m.take_vram_dirty(),
        "画面が変わったのに描き直しの合図が出ていない"
    );
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
    m.cpu.regs[5] = SRC; // BP
    m.cpu.regs[AX] = 0x1300;
    m.cpu.regs[BX] = 0x0A00; // 明るい緑
    m.cpu.regs[CX] = 2;
    m.cpu.regs[DX] = 0x0105; // 1行目 5桁目
    call_int(&mut m, 0x10, 10_000);
    assert_eq!(cell(&m, 1, 5), (b'H', 0x0A));
    assert_eq!(cell(&m, 1, 6), (b'I', 0x0A));
}
