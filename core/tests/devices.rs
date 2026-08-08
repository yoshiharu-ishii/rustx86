//! 8259 PIC / 8254 PIT / UART 16550 のテスト。
//!
//! この3つは32bit Linuxでもそのまま使うので、捨てにならない順として先に作っている。
//! 最後の `timer_interrupt_reaches_the_cpu` が本命で、
//! **PITが挙手 → PICが交通整理 → CPUが受け取る**という一周を通す。

use rustx86_core::cpu::IF;
use rustx86_core::dev::{pit::CLOCK_HZ, Pic8259, Pit8254, Uart16550};
use rustx86_core::Machine;

// ---------- 8259 PIC ----------

/// OSはICW1〜ICW4を同じポートに順番に書く。
/// **同じ番地でも何番目かで意味が変わる**のがこのチップの流儀である
#[test]
fn pic_initialization_sequence_changes_the_meaning_of_the_same_port() {
    let mut p = Pic8259::new();
    p.write_command(0x11); // ICW1: 初期化開始 + ICW4を送る
    p.write_data(0x08); // ICW2: ベクタのベース
    p.write_data(0x04); // ICW3: カスケード
    p.write_data(0x01); // ICW4: 8086モード
    assert_eq!(p.vector_base, 0x08);

    // 初期化が終わると、同じ 0x21 が割り込みマスクになる
    p.write_data(0xFE);
    assert_eq!(p.imr, 0xFE);
}

/// ICW2で決めたベースから割り込みベクタが決まる。
///
/// BIOSはIRQ0をベクタ0x08に置くが、プロテクトモードではCPUの例外番号と
/// 衝突するため、Linuxは起動時に0x20へ付け替える
#[test]
fn pic_vector_base_decides_the_interrupt_number() {
    for (base, irq, want) in [(0x08u8, 0u8, 0x08u8), (0x08, 4, 0x0C), (0x20, 0, 0x20)] {
        let mut p = Pic8259::new();
        p.write_command(0x11);
        p.write_data(base);
        p.write_data(0x04);
        p.write_data(0x01);
        p.write_data(0x00); // マスク解除
        p.raise(irq);
        assert_eq!(p.acknowledge(), Some(want), "base={base:#04x} irq={irq}");
    }
}

/// マスクされた線は挙手しても通らない
#[test]
fn pic_masked_lines_are_ignored() {
    let mut p = Pic8259::new();
    p.write_command(0x11);
    p.write_data(0x08);
    p.write_data(0x04);
    p.write_data(0x01);
    p.write_data(0xFE); // IRQ0 だけ開ける

    p.raise(1);
    assert_eq!(p.acknowledge(), None, "IRQ1はマスクされている");
    p.raise(0);
    assert_eq!(p.acknowledge(), Some(0x08), "IRQ0は通る");
}

/// 優先順位は線の番号が若いほど高い。
/// **処理中(ISR)より優先度の低い線は、EOIが来るまで待たされる**
#[test]
fn pic_lower_priority_waits_until_eoi() {
    let mut p = Pic8259::new();
    p.write_command(0x11);
    p.write_data(0x08);
    p.write_data(0x04);
    p.write_data(0x01);
    p.write_data(0x00);

    p.raise(0);
    p.raise(3);
    assert_eq!(p.acknowledge(), Some(0x08), "若い番号が先");
    assert_eq!(p.acknowledge(), None, "IRQ0を処理中はIRQ3が待たされる");

    p.write_command(0x20); // 非特定EOI
    assert_eq!(p.acknowledge(), Some(0x0B), "EOIで解けるとIRQ3が通る");
}

// ---------- 8254 PIT ----------

/// 分周値で割り込み周波数が決まる。
///
/// 入力 1.193182 MHz は設計値ではなく、初代IBM PCがNTSCカラーサブキャリアの
/// 水晶を流用した残りである。テレビ用の部品が安かった、というだけの理由が
/// 40年生き残っている
#[test]
fn pit_divisor_sets_the_interrupt_frequency() {
    // 分周値0 = 65536 → DOSの 18.2 Hz
    let mut p = Pit8254::new();
    p.write_control(0x36); // カウンタ0、LoHi、モード3
    p.write_counter(0, 0x00);
    p.write_counter(0, 0x00);
    assert!((p.irq0_hz() - 18.2).abs() < 0.1, "{}", p.irq0_hz());

    // Linuxの 100 Hz
    let mut p = Pit8254::new();
    p.write_control(0x36);
    let div = (CLOCK_HZ / 100) as u16; // 11931
    p.write_counter(0, div as u8);
    p.write_counter(0, (div >> 8) as u8);
    assert!((p.irq0_hz() - 100.0).abs() < 0.1, "{}", p.irq0_hz());
}

/// カウンタが0を跨いだ回数だけ出力パルスが出る
#[test]
fn pit_emits_one_pulse_per_period() {
    let mut p = Pit8254::new();
    p.write_control(0x36);
    p.write_counter(0, 100);
    p.write_counter(0, 0);

    assert_eq!(p.tick(99), 0, "まだ0に達しない");
    assert_eq!(p.tick(1), 1, "ちょうど1周");
    assert_eq!(p.tick(250), 2, "2周と半分 → パルスは2発");
}

/// 16bitの値を8bitポートで読むと、途中で桁が繰り下がって壊れる。
/// **ラッチはそれを防ぐための写し取り**である
#[test]
fn pit_latch_freezes_the_value_across_two_reads() {
    let mut p = Pit8254::new();
    p.write_control(0x36);
    p.write_counter(0, 0x00);
    p.write_counter(0, 0x10); // 0x1000

    p.write_control(0x00); // カウンタ0をラッチ
    let lo = p.read_counter(0);
    p.tick(0x500); // 読んでいる間にカウンタは進む
    let hi = p.read_counter(0);
    assert_eq!(
        u16::from(hi) << 8 | u16::from(lo),
        0x1000,
        "ラッチした瞬間の値"
    );
}

// ---------- UART 16550 ----------

/// 書いたバイトがそのまま外へ出る。ELKSのコンソールはこの1本で成立する
#[test]
fn uart_transmits_what_is_written() {
    let mut u = Uart16550::new();
    for b in b"hi" {
        u.write(0, *b);
    }
    assert_eq!(u.tx_string(), "hi");
}

/// DLABを立てると**先頭2ポートの意味が分周値に化ける**。
/// 8本しかないポートに通信速度を置く余裕が無かった時代の節約術
#[test]
fn uart_dlab_repurposes_the_first_two_ports() {
    let mut u = Uart16550::new();
    u.write(3, 0x80); // LCR: DLABを立てる
    u.write(0, 0x01); // 分周値の下位
    u.write(1, 0x00); // 分周値の上位
    assert_eq!(u.divisor, 1);
    assert_eq!(u.baud(), 115_200);

    u.write(3, 0x03); // DLABを下ろす (8N1)
    u.write(0, b'Z'); // 同じポートが送信に戻る
    assert_eq!(u.tx_string(), "Z");
}

/// 受信データの有無はLSRのbit0で見る。OSはここをポーリングする
#[test]
fn uart_line_status_reports_received_data() {
    let mut u = Uart16550::new();
    assert_eq!(u.read(5) & 1, 0, "受信データなし");
    u.feed(b"A");
    assert_eq!(u.read(5) & 1, 1, "受信データあり");
    assert_eq!(u.read(0), b'A');
    assert_eq!(u.read(5) & 1, 0, "読んだら下がる");
}

// ---------- 一周 ----------

/// **これが本命**。PITが挙手 → PICが交通整理 → CPUが命令境界で受け取る。
/// この経路のどこか1つでも欠けるとOSのスケジューラが動かない。
#[test]
fn timer_interrupt_reaches_the_cpu() {
    let mut m = Machine::new();
    let sector = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../asm/hello.bin")).unwrap();
    m.load_boot_sector(&sector).unwrap();

    // OSがやる初期化をそのまま行う: PICを初期化し、IRQ0だけ開ける
    m.io_write8(0x20, 0x11); // ICW1
    m.io_write8(0x21, 0x08); // ICW2: IRQ0 → ベクタ 0x08
    m.io_write8(0x21, 0x04); // ICW3
    m.io_write8(0x21, 0x01); // ICW4
    m.io_write8(0x21, 0xFE); // IRQ0 以外をマスク

    // PITを 100 Hz に設定
    m.io_write8(0x43, 0x36);
    m.io_write8(0x40, 0x9B);
    m.io_write8(0x40, 0x2E); // 0x2E9B = 11931

    // 割り込みハンドラを IVT[0x08] に置く
    m.write16(0x08 * 4, 0x9000);
    m.write16(0x08 * 4 + 2, 0x0000);
    m.cpu.set_flag(IF, true);

    // ハンドラに到達するまで回す
    let mut reached = false;
    for _ in 0..2_000_000 {
        m.step();
        if m.cpu.ip == 0x9000 {
            reached = true;
            break;
        }
    }
    assert!(reached, "タイマ割り込みがCPUに届かなかった");
    assert_eq!(
        m.devices.pic[0].isr & 1,
        1,
        "PICは処理中の印 (ISR) を立てたまま。EOIが来るまで下りない"
    );
}

// ---------- キーボード配列 ----------

/// ASCII → スキャンコードの対応 (US配列)。
///
/// ゲスト (ELKS) はUS配列の対応表しか持たない。JIS配列の実機から使うときは
/// **位置ではなく文字**を渡して辻褄を合わせるので、ここが正しくないと
/// 記号がまったく入らなくなる (実際 `@` が落ちていた)。
#[test]
fn ascii_maps_to_us_layout_scancodes() {
    use rustx86_core::dev::kbd::scancode_shift;

    // 数字段の記号は数字のShift。タイプライタからの引き継ぎで、
    // キーの位置に意味があるわけではない
    assert_eq!(scancode_shift('2'), Some((0x03, false)));
    assert_eq!(scancode_shift('@'), Some((0x03, true)), "@ は Shift+2");
    assert_eq!(scancode_shift('1'), Some((0x02, false)));
    assert_eq!(scancode_shift('!'), Some((0x02, true)));

    // 英字は位置。大文字はShift付き
    assert_eq!(scancode_shift('q'), Some((0x10, false)));
    assert_eq!(scancode_shift('Q'), Some((0x10, true)));
    assert_eq!(scancode_shift('a'), Some((0x1E, false)));
    assert_eq!(scancode_shift('z'), Some((0x2C, false)));

    // 記号
    assert_eq!(scancode_shift(':'), Some((0x27, true)), ": は Shift+;");
    assert_eq!(scancode_shift('/'), Some((0x35, false)));
    assert_eq!(scancode_shift('?'), Some((0x35, true)));
    assert_eq!(scancode_shift('_'), Some((0x0C, true)));
    assert_eq!(scancode_shift('|'), Some((0x2B, true)));

    // 制御
    assert_eq!(scancode_shift('\n'), Some((0x1C, false)));
    assert_eq!(scancode_shift(' '), Some((0x39, false)));
    assert_eq!(scancode_shift('\x1b'), Some((0x01, false)));

    // 表せない文字は落とす (勝手に別の文字にしない)
    assert_eq!(scancode_shift('あ'), None);
}

/// 印字できるASCIIがすべて打てること。取りこぼしがあると
/// 「その記号だけ入らない」という分かりにくい症状になる
#[test]
fn every_printable_ascii_can_be_typed() {
    use rustx86_core::dev::kbd::scancode_shift;
    for c in 0x20u8..0x7F {
        let ch = c as char;
        assert!(scancode_shift(ch).is_some(), "{ch:?} ({c:#04x}) が打てない");
    }
}

/// Shiftが要る文字は、Shiftの上げ下げで挟んで送られる
#[test]
fn shifted_characters_are_wrapped_with_shift() {
    let mut k = rustx86_core::dev::Kbd8042::new();
    k.type_ascii("@");
    let mut got = Vec::new();
    while k.has_data() {
        got.push(k.read_data());
    }
    assert_eq!(
        got,
        vec![0x2A, 0x03, 0x83, 0xAA],
        "Shift押下, 2押下, 2離す, Shift離す"
    );
}
