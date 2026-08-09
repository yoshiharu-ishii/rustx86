use rustx86_core::{cpu, Machine, MachineProfile};
fn main() {
    let data = std::fs::read("images/vmlinuz-lts").unwrap();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_bzimage(&data, "console=ttyS0 earlyprintk=serial,ttyS0,115200 debug")
        .unwrap();
    let mut printed = 0usize;
    let mut i = 0u64;
    let mut next_report = 500_000_000u64;
    while i < 6_000_000_000 {
        if i == next_report {
            eprintln!("[progress {i}: EIP={:08x}]", m.cpu.ip);
            next_report += 500_000_000;
        }
        if m.halted && !m.cpu.flag(cpu::IF) {
            // IFなしのHLT = 二度と起きない死の停止。IF付きは割り込みで
            // 起きるアイドルなので回し続ける (実機のCPUと同じ)
            eprintln!("[DEAD HALT @ {i} at {:04x}:{:08x}]",
                m.cpu.sregs[cpu::CS], m.cpu.ip);
            break;
        }
        if let Some(t) = &m.trap {
            let lin = m.cpu.lin(cpu::CS, t.ip);
            let bytes: Vec<u8> = (0..12)
                .map(|k| m.read_phys8(m.translate(lin + k)))
                .collect();
            let asm = rustx86_disasm::one(&bytes, 32, t.ip as u64);
            eprintln!("[TRAP @ {i}: {} at {:04x}:{:08x}  {asm}]", t.reason, t.cs, t.ip);
            break;
        }
        m.step();
        i += 1;
        if m.devices.uart.tx.len() > printed {
            let out: String = m.devices.uart.tx[printed..].iter().map(|&b| b as char).collect();
            print!("{out}");
            printed = m.devices.uart.tx.len();
        }
    }
    eprintln!("[done: {i} instrs, serial {} bytes, EIP={:08x}]", m.devices.uart.tx.len(), m.cpu.ip);
    // VGAテキストも覗く
    for row in 0..25 {
        let mut line = String::new();
        for col in 0..80 {
            let ch = m.read_phys8(0xB8000 + (row * 80 + col) * 2);
            line.push(if (0x20..0x7F).contains(&ch) { ch as char } else { ' ' });
        }
        let line = line.trim_end().to_string();
        if !line.is_empty() { eprintln!("VGA {row:2}| {line}"); }
    }
}
