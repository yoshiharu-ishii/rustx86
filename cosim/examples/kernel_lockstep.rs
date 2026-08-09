//! bzImage ブートを rustx86 と Unicorn の両方で走らせ、1命令ずつ突き合わせる。
//!
//! ランダム単発照合 (cosim本体) と違い、**実カーネルの実行列そのもの**を
//! オラクルと比較する。デコンプレッサのような「アルゴリズムが出力を検証する」
//! コードは、CPUのわずかな噛み違いを何十万命令も先で「uncompression error」
//! としてしか教えてくれない — 突き合わせなら食い違った命令がその場で出る。
//!
//! 実行: cargo run -p rustx86-cosim --example kernel_lockstep --release

use rustx86_core::{cpu, Machine, MachineProfile};
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

const RAM_MB: u64 = 128;

fn main() {
    let img = std::fs::read("images/vmlinuz-lts").expect("images/vmlinuz-lts");

    // --- rustx86 側 ---
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(RAM_MB as usize));
    m.boot_bzimage(&img, "console=ttyS0").unwrap();

    // --- Unicorn 側: rustx86 が作った起動直後のメモリとレジスタを丸写し ---
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_32).expect("unicorn init");
    uc.mem_map(0, RAM_MB << 20, Prot::ALL).expect("map");
    // メモリ全体をコピー (カーネル・zero page・GDT が含まれる)
    let ram: Vec<u8> = (0..(RAM_MB << 20) as u32).map(|a| m.read_phys8(a)).collect();
    uc.mem_write(0, &ram).expect("ram copy");

    let gpr = [
        (RegisterX86::EAX, cpu::AX),
        (RegisterX86::ECX, cpu::CX),
        (RegisterX86::EDX, cpu::DX),
        (RegisterX86::EBX, cpu::BX),
        (RegisterX86::ESP, cpu::SP),
        (RegisterX86::EBP, cpu::BP),
        (RegisterX86::ESI, cpu::SI),
        (RegisterX86::EDI, cpu::DI),
    ];
    for (ur, i) in gpr {
        uc.reg_write(ur, m.cpu.regs[i] as u64).unwrap();
    }
    uc.reg_write(RegisterX86::EFLAGS, m.cpu.flags as u64 | 2)
        .unwrap();

    // GDTR は64bit超の特殊レジスタなので reg_write_long で
    // uc_x86_mmr のレイアウト {selector:u16, pad, base:u64, limit:u32, flags:u32}
    // をそのままバイト列で書く
    let mut mmr = [0u8; 24];
    mmr[8..16].copy_from_slice(&0x800u64.to_le_bytes()); // base
    mmr[16..20].copy_from_slice(&31u32.to_le_bytes()); // limit
    uc.reg_write_long(RegisterX86::GDTR, &mmr).expect("gdtr");
    // CR0.PE
    uc.reg_write(RegisterX86::CR0, 0x1).expect("cr0");
    for (r, sel) in [
        (RegisterX86::CS, 0x10u64),
        (RegisterX86::DS, 0x18),
        (RegisterX86::ES, 0x18),
        (RegisterX86::FS, 0x18),
        (RegisterX86::GS, 0x18),
        (RegisterX86::SS, 0x18),
    ] {
        uc.reg_write(r, sel).expect("sreg");
    }
    uc.reg_write(RegisterX86::EIP, m.cpu.ip as u64).unwrap();

    // --- ロックステップ ---
    let mut last = String::new();
    for i in 0u64..2_000_000 {
        if m.halted || m.trap.is_some() {
            println!("rustx86 stopped @ {i}: halted={} trap={:?}", m.halted, m.trap);
            break;
        }
        let ip = m.cpu.ip;
        // CPUID / RDTSC は「マシンの個性」— 石が違えば答えも違うのが正しい。
        // うちの答えを正としてUnicornへ写し、比較は続ける
        let lin = m.cpu.lin(cpu::CS, ip);
        let identity_op = matches!(
            (m.read_phys8(lin), m.read_phys8(lin + 1)),
            (0x0F, 0xA2) | (0x0F, 0x31)
        );
        // 1命令ずつ。until は使わず count=1
        if let Err(e) = uc.emu_start(uc.reg_read(RegisterX86::EIP).unwrap(), 0, 0, 1) {
            println!("unicorn error @ {i}: {e:?} (eip={ip:08x})");
            break;
        }
        m.step();

        // REP系はうちが1ステップで完走するのに対し、Unicornは1反復ずつ
        // 止まる。同じIPに留まっている間は回して足並みを揃える
        let mut u_eip = uc.reg_read(RegisterX86::EIP).unwrap() as u32;
        let mut guard = 0u64;
        while u_eip == ip && m.cpu.ip != ip {
            if let Err(e) = uc.emu_start(u_eip as u64, 0, 0, 1) {
                println!("unicorn error in rep @ {i}: {e:?}");
                break;
            }
            u_eip = uc.reg_read(RegisterX86::EIP).unwrap() as u32;
            guard += 1;
            if guard > 40_000_000 {
                println!("rep sync runaway @ {i}");
                break;
            }
        }
        if m.cpu.ip != u_eip {
            println!("DIVERGE @ {i}: EIP ours={:08x} unicorn={u_eip:08x}", m.cpu.ip);
            println!("  last executed: {last}");
            dump(&m, &mut uc);
            break;
        }
        if identity_op {
            for (ur, gi) in gpr {
                uc.reg_write(ur, m.cpu.regs[gi] as u64).unwrap();
            }
            last = format!("{ip:08x}");
            continue;
        }
        let mut bad = Vec::new();
        for (ur, gi) in gpr {
            let uv = uc.reg_read(ur).unwrap() as u32;
            if m.cpu.regs[gi] != uv {
                bad.push(format!(
                    "{:?} ours={:08x} uc={uv:08x}",
                    ur, m.cpu.regs[gi]
                ));
            }
        }
        if !bad.is_empty() {
            println!("DIVERGE @ {i} at instr ip={ip:08x} (next eip={:08x}):", m.cpu.ip);
            for b in &bad {
                println!("  {b}");
            }
            break;
        }
        last = format!("{ip:08x}");
    }
    println!("done");
}

fn dump(m: &Machine, uc: &mut Unicorn<()>) {
    let names = ["EAX", "ECX", "EDX", "EBX", "ESP", "EBP", "ESI", "EDI"];
    let urs = [
        RegisterX86::EAX,
        RegisterX86::ECX,
        RegisterX86::EDX,
        RegisterX86::EBX,
        RegisterX86::ESP,
        RegisterX86::EBP,
        RegisterX86::ESI,
        RegisterX86::EDI,
    ];
    for i in 0..8 {
        println!(
            "  {} ours={:08x} uc={:08x}",
            names[i],
            m.cpu.regs[i],
            uc.reg_read(urs[i]).unwrap() as u32
        );
    }
}
