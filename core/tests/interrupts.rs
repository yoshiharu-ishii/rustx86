//! 割り込み機構 (Tier 1d) のテスト。
//!
//! co-simは1命令単位の比較なので、**割り込みは守備範囲の外**にある。
//! IVTの内容・スタックの積み方・受付タイミングは複数命令にまたがる状態遷移で、
//! 「同じ初期状態から1命令」では表現できない。ここからは実プログラムを
//! 走らせること自体がテストになる (ELKS起動へ続く道でもある)。

use rustx86_core::cpu::{AX, CS, IF, TF};
use rustx86_core::{Machine, BIOS_SEG};

fn load(name: &str) -> Machine {
    let path = format!("{}/../asm/{name}", env!("CARGO_MANIFEST_DIR"));
    let sector = std::fs::read(&path).unwrap_or_else(|e| panic!("{path} ({e})"));
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    m
}

/// 実プログラムでの一連の流れ:
/// IVTを自分のハンドラで書き換える → INT で呼ばれる → IRETで戻る →
/// 書き換えていないベクタはBIOS HLEのまま → ゼロ除算がハンドラへ飛ぶ
#[test]
fn os_takes_over_the_vector_table() {
    let mut m = load("interrupt.bin");
    let executed = m.run(10_000);
    assert!(m.halted, "HLT到達せず ({executed}命令)");
    assert_eq!(
        m.console_string(),
        "ABD!",
        "A=自前ハンドラ B=BIOS HLE D=ゼロ除算ハンドラ !=除算の次から再開"
    );
}

/// 起動直後はIVTの全エントリがBIOS HLEの入口を指している。
/// 実BIOSが起動時にやることと同じで、OSはこの上から自分のハンドラを書く
#[test]
fn boot_installs_bios_vectors() {
    let m = load("hello.bin");
    for n in [0x00u32, 0x10, 0x13, 0xFF] {
        assert_eq!(m.read16(n * 4), n as u16, "INT {n:#04x} のオフセット");
        assert_eq!(m.read16(n * 4 + 2), BIOS_SEG, "INT {n:#04x} のセグメント");
    }
}

/// 割り込みは**命令の途中ではなく境界で**受け付ける。
/// IFが下りている間は保留され、STIで開いた次の境界で入る
#[test]
fn hardware_irq_waits_for_the_interrupt_flag() {
    let mut m = load("hello.bin");
    m.cpu.set_flag(IF, false);
    m.raise_irq(0x08);

    let before = m.cpu.ip;
    m.step();
    assert!(m.pending_irq.is_some(), "IFが下りている間は保留されたまま");
    assert_ne!(m.cpu.ip, before, "保留中でも命令自体は進む");

    m.cpu.set_flag(IF, true);
    m.step();
    assert!(m.pending_irq.is_none(), "IFが立ったら受け付ける");
    assert_eq!(m.cpu.sregs[CS], BIOS_SEG, "IVT経由でハンドラへ飛んでいる");
}

/// ハンドラに入るとIFが落ちる。多重割り込みを防ぐためで、
/// 必要ならハンドラ側がSTIで開け直す
#[test]
fn entering_a_handler_clears_the_interrupt_flag() {
    let mut m = load("hello.bin");
    m.cpu.set_flag(IF, true);
    m.raise_irq(0x08);
    m.step();
    assert!(!m.cpu.flag(IF), "ハンドラ実行中は割り込み禁止");
}

/// HLTは割り込みで目を覚ます。「何もすることが無いので寝て待つ」という
/// OSのアイドルループが成立するのはこのため
#[test]
fn halt_wakes_up_on_interrupt() {
    let mut m = load("hello.bin");
    m.halted = true;
    m.cpu.set_flag(IF, true);

    m.step();
    assert!(m.halted, "割り込みが無ければ寝たまま");

    m.raise_irq(0x08);
    m.step();
    assert!(!m.halted, "割り込みで起きる");
    assert_eq!(m.cpu.sregs[CS], BIOS_SEG);
}

/// IFが下りたままのHLTは起きない (実機も同じ。NMIしか起こせない)
#[test]
fn halt_stays_asleep_while_interrupts_are_disabled() {
    let mut m = load("hello.bin");
    m.halted = true;
    m.cpu.set_flag(IF, false);
    m.raise_irq(0x08);
    m.step();
    assert!(m.halted, "IFが下りていれば起きない");
}

/// トラップフラグ: 1命令実行してから INT 1。
/// 「実行してから止まる」のでデバッガが1命令ずつ進められる
#[test]
fn trap_flag_fires_after_the_instruction() {
    let mut m = load("hello.bin");
    // INT 1 に自前ハンドラを置き、そこへ来たことを確認する
    m.write16(1 * 4, 0x9000);
    m.write16(1 * 4 + 2, 0x0000);
    m.cpu.set_flag(TF, true);

    m.step();
    assert_eq!(m.cpu.ip, 0x9000, "命令の後にINT 1へ飛ぶ");
    assert!(!m.cpu.flag(TF), "ハンドラ内ではTFが落ちている (無限再帰を防ぐ)");
}

/// ゼロ除算はマシンを止めず #DE (INT 0) を上げる。
/// 積まれる戻り先は「失敗した命令の先頭」— フォールトなのでやり直せる
#[test]
fn divide_by_zero_pushes_the_faulting_address() {
    let mut m = Machine::new();
    // 0x0000-0x03FF はIVTなので、コードはその先に置く。
    // (ここに置いてIVTを潰し、自分の命令を書き換えて一度ハマった)
    const CODE: u32 = 0x0500;
    // XOR CX,CX / DIV CX
    for (i, b) in [0x31u8, 0xC9, 0xF7, 0xF1].iter().enumerate() {
        m.write8(CODE + i as u32, *b);
    }
    m.write16(0 * 4, 0x9000); // INT 0 のハンドラ
    m.write16(0 * 4 + 2, 0x0000);
    m.cpu.set_cs_ip(0, CODE as u16);
    m.cpu.regs[AX] = 100;
    m.cpu.regs[4] = 0x7C00; // SP

    m.step(); // XOR CX,CX
    m.step(); // DIV CX → #DE

    assert!(!m.halted, "マシンは止まらない");
    assert_eq!(m.cpu.ip, 0x9000, "ハンドラへ飛んでいる");
    // スタック: [SP]=IP [SP+2]=CS [SP+4]=FLAGS
    let sp = m.cpu.regs[4] as u32;
    assert_eq!(
        m.read16(sp),
        CODE as u16 + 2,
        "戻り先はDIVの先頭 (次の命令ではない)"
    );
}
