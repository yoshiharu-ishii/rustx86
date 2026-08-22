//! DOS/16M (DOOM.EXE の DOS エクステンダ) の CPU 判定列 — 386 と答えること。
//! PUSHF の bit 12-15 (8086 は常に 1)、PUSH SP (186 以前は減算後を積む)、
//! POPF で IOPL/NT が立つか (286 のリアルモードでは立たない) の 3 段
use rustx86_core::{cpu, Machine, MachineProfile};
#[test]
fn dos16m_cpu_check_says_386() {
    // DOOM.EXE (DOS/16M) の CPU 判定ルーチンをそのまま (末尾に HLT)
    let code: &[u8] = &[
        0x9C, 0x33, 0xC0, 0x50, 0x9D, 0x9C, 0x58, 0x80, 0xE4, 0xF0, 0x80, 0xFC, 0xF0, 0x74, 0x24,
        0x54, 0x5B, 0x3B, 0xDC, 0x75, 0x19, 0xB8, 0x00, 0xF0, 0x50, 0x9D, 0x9C, 0x5B, 0x23, 0xD8,
        0x74, 0x09, 0xB8, 0x03, 0x00, 0x66, 0xA3, 0xF2, 0x10, 0xEB, 0x0C, 0xB8, 0x02, 0x00, 0xEB,
        0x07, 0xB8, 0x01, 0x00, 0xEB, 0x02, 0x33, 0xC0, 0x9D, 0xF4,
    ];
    let mut sector = vec![0u8; 1_474_560];
    sector[..code.len()].copy_from_slice(code);
    sector[510] = 0x55;
    sector[511] = 0xAA;
    let mut m = Machine::with_profile(MachineProfile::pc_floppy(4));
    m.boot_from_disk(sector).expect("boot");
    for _ in 0..10_000 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted);
    assert_eq!(
        m.cpu.regs[cpu::AX] & 0xFFFF,
        3,
        "DOS/16M の判定: 0=8086 1=186 2=286 3=386+"
    );
}
