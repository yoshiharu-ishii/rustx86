//! INT 15h E820: メモリマップ。gfxboot (DSL 2024 の isolinux メニュー) が高位メモリを
//! 取るのに要る。1MB〜RAM 末尾が「使える」で出て、続きの番号が 0 で終わること
use rustx86_core::{cpu, Machine, MachineProfile};

#[test]
fn e820_lists_low_and_high_ram() {
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(64));
    let mut ebx = 0u32;
    let mut entries = Vec::new();
    loop {
        m.cpu.regs[cpu::AX] = 0xE820;
        m.cpu.regs[cpu::BX] = ebx;
        m.cpu.regs[cpu::CX] = 24;
        m.cpu.regs[cpu::DX] = 0x534D_4150;
        m.cpu.sregs[cpu::ES] = 0x1000;
        m.cpu.regs[cpu::DI] = 0;
        m.bios_interrupt(0x15);
        assert!(
            !m.cpu.flag(cpu::CF),
            "CF が立った (entry {})",
            entries.len()
        );
        assert_eq!(m.cpu.regs[cpu::AX], 0x534D_4150);
        let at = 0x10000u32;
        let base = m.read32(at) as u64 | (m.read32(at + 4) as u64) << 32;
        let len = m.read32(at + 8) as u64 | (m.read32(at + 12) as u64) << 32;
        let kind = m.read32(at + 16);
        entries.push((base, len, kind));
        ebx = m.cpu.regs[cpu::BX];
        if ebx == 0 {
            break;
        }
    }
    assert_eq!(entries[0], (0, 0x9FC00, 1));
    let high = entries
        .iter()
        .find(|e| e.0 == 0x10_0000)
        .expect("1MB からの項");
    assert_eq!(high.2, 1);
    assert_eq!(high.1, 64 * 1024 * 1024 - 0x10_0000);
    // 知らない番号は CF
    m.cpu.regs[cpu::AX] = 0xE820;
    m.cpu.regs[cpu::BX] = 99;
    m.cpu.regs[cpu::CX] = 24;
    m.cpu.regs[cpu::DX] = 0x534D_4150;
    m.bios_interrupt(0x15);
    assert!(m.cpu.flag(cpu::CF));
}
