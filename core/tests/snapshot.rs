//! 状態の保存と復元のテスト。
//!
//! 「割り込みの直前で止めて保存し、何度でもそこから始める」が成立するには、
//! **保存した状態から再開した機械が、保存しなかった機械とまったく同じ道を
//! たどる**必要がある。ここではそれを直接確かめる。

use rustx86_core::Machine;

fn boot(name: &str) -> Option<Machine> {
    let path = format!("{}/../{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).ok()?;
    let mut m = Machine::new();
    if name.ends_with(".img") {
        m.boot_from_disk(data).ok()?;
    } else {
        m.load_boot_sector(&data).ok()?;
    }
    Some(m)
}

/// 保存して戻すと、同じ状態から同じ結果になる
#[test]
fn restored_machine_follows_the_same_path() {
    let mut a = boot("asm/vram.bin").expect("asm/vram.bin");
    a.run(3_000); // 途中まで走らせる

    let snap = a.save_state();
    let mut b = Machine::new();
    b.load_state(&snap).expect("復元");

    // 同じ地点から同じだけ走らせる
    a.run(10_000);
    b.run(10_000);

    assert_eq!(a.text_screen_string(), b.text_screen_string(), "画面が違う");
    assert_eq!(a.cpu.regs, b.cpu.regs, "レジスタが違う");
    assert_eq!(a.cpu.ip, b.cpu.ip, "IPが違う");
    assert_eq!(a.cpu.eflags(), b.cpu.eflags(), "フラグが違う");
    assert_eq!(a.halted, b.halted);
}

/// **装置の状態も戻ること。** CPUとメモリだけでは再開できない
#[test]
fn devices_survive_the_round_trip() {
    let mut m = boot("asm/hello.bin").expect("asm/hello.bin");
    // OSがやるようにPICとPITを設定する
    m.io_write8(0x20, 0x11);
    m.io_write8(0x21, 0x08);
    m.io_write8(0x21, 0x04);
    m.io_write8(0x21, 0x01);
    m.io_write8(0x21, 0xFE);
    m.io_write8(0x43, 0x36);
    m.io_write8(0x40, 0x9B);
    m.io_write8(0x40, 0x2E);
    m.devices.keyboard.type_ascii("hi");
    m.devices.uart.feed(b"z");

    let snap = m.save_state();
    let mut b = Machine::new();
    b.load_state(&snap).expect("復元");

    assert_eq!(b.devices.pic[0].vector_base, 0x08, "PICのベクタベース");
    assert_eq!(b.devices.pic[0].imr, 0xFE, "PICのマスク");
    assert_eq!(b.devices.pit.counters[0].reload, 0x2E9B, "PITの分周値");
    assert!(b.devices.pit.counters[0].running, "PITが動いている");
    assert!((b.devices.pit.irq0_hz() - 100.0).abs() < 0.1);
    assert!(b.devices.keyboard.has_data(), "打ち込んだキーが残っている");
    assert_eq!(b.devices.uart.rx.len(), 1, "受信待ちが残っている");
}

/// 壊れたデータを黙って受け入れない
#[test]
fn rejects_garbage() {
    let mut m = Machine::new();
    assert!(m.load_state(b"").is_err(), "空");
    assert!(m.load_state(b"NOTASNAP--------").is_err(), "印が違う");

    let good = boot("asm/hello.bin").expect("asm/hello.bin").save_state();
    assert!(
        m.load_state(&good[..good.len() / 2]).is_err(),
        "途中で切れている"
    );
    // 版が違うもの
    let mut bad = good.clone();
    bad[8] = 0xFF;
    assert!(m.load_state(&bad).is_err(), "版が違う");
}

/// 失敗しても元の機械を壊さない
#[test]
fn failed_load_leaves_the_machine_alone() {
    let mut m = boot("asm/vram.bin").expect("asm/vram.bin");
    m.run(3_000);
    let before = (m.cpu.regs, m.cpu.ip, m.text_screen_string());

    assert!(m.load_state(b"broken").is_err());

    assert_eq!(m.cpu.regs, before.0, "失敗したのにレジスタが変わっている");
    assert_eq!(m.cpu.ip, before.1);
    assert_eq!(m.text_screen_string(), before.2);
}

/// ゼロの海が潰れて、実用的な大きさに収まること
#[test]
fn snapshot_is_small_enough_to_carry() {
    let m = boot("asm/hello.bin").expect("asm/hello.bin");
    let snap = m.save_state();
    assert!(
        snap.len() < 64 * 1024,
        "1MBのメモリを積んで {} バイト (連長圧縮が効いていない)",
        snap.len()
    );
}
