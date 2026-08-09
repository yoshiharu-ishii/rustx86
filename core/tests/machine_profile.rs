//! マシンプロファイル — RAMサイズがマシンごとに変わる。
//!
//! Linuxは今の1MBには収まらない (bzImage + initrd + 作業領域で数MB要る)。
//! `Machine::new()` の決め打ちをプロファイル駆動にする第一歩。

use rustx86_core::{Machine, MachineProfile};

#[test]
fn 既定は16bit機で1mb() {
    let m = Machine::new();
    assert_eq!(m.ram_bytes(), 1 << 20, "既定のRAMが1MBでない");
}

#[test]
fn プロファイルでramを増やせる() {
    let m = Machine::with_profile(MachineProfile::pc_32bit(16)); // 16MB
    assert_eq!(m.ram_bytes(), 16 << 20);
}

#[test]
fn 大きいramは物理1mb超を素通しする() {
    // 16MB機。物理15MB (0x00F0_0000) に書いて読み戻せる = 折り返していない
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(16));
    m.write_phys32(0x00F0_0000, 0xDEAD_BEEF);
    assert_eq!(m.read_phys32(0x00F0_0000), 0xDEAD_BEEF, "15MBが折り返した");
}

#[test]
fn ram1mb機の1mb超は未マップ() {
    // 物理層は折り返さない。RAMを超えた番地は未マップ = 0xFF が返る
    // (実機でRAMを超えた番地がチップセットに落ちるのと同じ)。
    // 8086の1MBラップは cpu::lin 側にあり、これとは別
    let mut m = Machine::new();
    m.write_phys32(0x0010_0000, 0xDEAD_BEEF); // 1MB (範囲外)。書きは捨てられる
    assert_eq!(
        m.read_phys32(0x0010_0000),
        0xFFFF_FFFF,
        "未マップが0xFFでない"
    );
    assert_eq!(
        m.read_phys32(0x0000_0000),
        0,
        "低位に漏れて書かれた (折り返した)"
    );
}

#[test]
fn スナップショットはramサイズを往復する() {
    // 大きい機械の状態を、大きい機械で復元できる (サイズが記録されている)
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(16));
    m.write_phys32(0x00F0_0000, 0xCAFE_F00D);
    let saved = m.save_state();

    // 既定 (1MB) の機械で load しても、保存側のサイズに合わせて広がる
    let mut n = Machine::new();
    n.load_state(&saved).unwrap();
    assert_eq!(n.ram_bytes(), 16 << 20, "復元でRAMサイズが合っていない");
    assert_eq!(n.read_phys32(0x00F0_0000), 0xCAFE_F00D);
}
