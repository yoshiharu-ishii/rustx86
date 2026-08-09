use rustx86_core::{cpu, Machine, MachineProfile};
fn main() {
    let data = std::fs::read("images/vmlinuz-lts").unwrap();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_bzimage(&data, "console=ttyS0").unwrap();
    let mut i = 0u64;
    let mut logged = 0;
    let mut was_aligned = true;
    loop {
        if m.trap.is_some() || (m.halted && !m.cpu.flag(cpu::IF)) || logged > 8 {
            println!("end @ {i}");
            break;
        }
        let ip = m.cpu.ip;
        m.step();
        let esp = m.cpu.regs[4];
        let aligned = esp & 3 == 0;
        if was_aligned && !aligned && m.cpu.hidden[cpu::SS].big {
            let lin = m.cpu.lin(cpu::CS, ip);
            let bs: Vec<u8> = (0..10).map(|k| m.read_phys8(m.translate(lin + k))).collect();
            let asm = rustx86_disasm::one(&bs, 32, ip as u64);
            println!("@{i} ESP misaligned: {esp:08x} after EIP={ip:08x}  {asm}");
            logged += 1;
        }
        was_aligned = aligned;
        i += 1;
    }
}
