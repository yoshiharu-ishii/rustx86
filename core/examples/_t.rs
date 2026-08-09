use rustx86_core::{cpu, Machine, MachineProfile};
fn main() {
    let data = std::fs::read("images/vmlinuz-lts").unwrap();
    let initrd = std::fs::read("images/initramfs-lts").ok();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_bzimage_with_initrd(&data, "console=ttyS0", initrd.as_deref()).unwrap();
    let mut timer_at_10b = 0u32;
    for i in 0u64..12_000_000_000u64 {
        if m.trap.is_some() { println!("trap @{i}: {:?}", m.trap); break; }
        m.step();
        if i == 10_000_000_000 {
            timer_at_10b = m.int_counts[0x30..0x40].iter().sum();
        }
    }
    let ip = m.cpu.ip;
    let bytes: Vec<u8> = (0..12).map(|k| m.read_phys8(m.translate(m.cpu.lin(cpu::CS, ip + k)))).collect();
    let asm = rustx86_disasm::one(&bytes, 32, ip as u64);
    println!("EIP={ip:08x} {asm} IF={} halted={}", m.cpu.flag(cpu::IF), m.halted);
    let timer_now: u32 = m.int_counts[0x30..0x40].iter().sum();
    println!("timer ints (0x30-0x3f): at10B={timer_at_10b} at12B={timer_now}");
    println!("pic0: {:?}", m.devices.pic[0]);
    println!("pit c0: reload={} count={} mode={} running={}",
        m.devices.pit.counters[0].reload, m.devices.pit.counters[0].count,
        m.devices.pit.counters[0].mode, m.devices.pit.counters[0].running);
    println!("unhandled_io: {:04x?}", m.unhandled_io);
    println!("ud_user: {:?}", m.ud_user);
    // 直近の割り込み
    println!("int_recent: {:?}", m.int_recent.iter().rev().take(6).collect::<Vec<_>>());
}
