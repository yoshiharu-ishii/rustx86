//! 32bit オペランドの整数命令を Unicorn と突き合わせる (0x66 プレフィクス付きの 16bit モード)。
//! 既存の alu.rs は 16bit の網で、**32bit の上位半分は比べていなかった** — glibc の mpn
//! (printf/strtod の多倍長) は shld/shrd/bsr/mul/div の 32bit を叩く (DSL 2024、2026-08-23)
use rustx86_core::{cpu, Machine, MachineProfile};
use rustx86_cosim::*;
use unicorn_engine::{Arch, Mode, Prot, RegisterX86, Unicorn};

#[derive(Clone, PartialEq, Eq, Debug)]
struct St32 {
    regs: [u32; 8],
    flags: u16,
    ip: u16,
    data: [u8; 16],
    stack: [u8; STACK_WINDOW],
}

fn ours(tc: &TestCase) -> St32 {
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(2));
    for (i, b) in tc.code.iter().enumerate() {
        m.write8(CODE_ADDR as u32 + i as u32, *b);
    }
    for (i, b) in tc.data.iter().enumerate() {
        m.write8(DATA_ADDR as u32 + i as u32, *b);
    }
    for (i, b) in tc.stack.iter().enumerate() {
        m.write8(STACK_BASE as u32 + i as u32, *b);
    }
    // 上位 16bit にも値を置く (乱数の下位を鏡写し)
    // 上位 16bit にも値を置く (EAX/ECX だけ。EDX は div の被除数の上位なので 0 のまま、
    // 残りは番地に使うので 16bit のまま)
    for i in 0..8 {
        m.cpu.regs[i] = tc.regs[i] as u32;
    }
    for i in 0..2 {
        m.cpu.regs[i] |= (tc.regs[(i + 3) & 7] as u32) << 16;
    }
    m.cpu.sregs[..4].copy_from_slice(&tc.sregs);
    m.cpu.set_eflags(tc.flags as u32 | 0x0002);
    m.cpu.set_cs_ip(0, CODE_ADDR);
    m.step();
    let mut data = [0u8; 16];
    for (i, d) in data.iter_mut().enumerate() {
        *d = m.read8(DATA_ADDR as u32 + i as u32);
    }
    let mut stack = [0u8; STACK_WINDOW];
    for (i, d) in stack.iter_mut().enumerate() {
        *d = m.read8(STACK_BASE as u32 + i as u32);
    }
    St32 {
        regs: std::array::from_fn(|i| m.cpu.regs[i]),
        flags: m.cpu.eflags() as u16 & FLAG_MASK_ALL,
        ip: m.cpu.ip as u16,
        data,
        stack,
    }
}

fn oracle(tc: &TestCase) -> St32 {
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_16).unwrap();
    uc.mem_map(0, 0x100000, Prot::ALL).unwrap();
    uc.mem_write(CODE_ADDR as u64, &tc.code).unwrap();
    uc.mem_write(DATA_ADDR as u64, &tc.data).unwrap();
    uc.mem_write(STACK_BASE as u64, &tc.stack).unwrap();
    let regs = [
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
        let mut v = tc.regs[i] as u32;
        if i < 2 {
            v |= (tc.regs[(i + 3) & 7] as u32) << 16;
        }
        uc.reg_write(regs[i], v as u64).unwrap();
    }
    let sregs = [
        RegisterX86::ES,
        RegisterX86::CS,
        RegisterX86::SS,
        RegisterX86::DS,
    ];
    for (i, s) in sregs.iter().enumerate() {
        uc.reg_write(*s, tc.sregs[i] as u64).unwrap();
    }
    uc.reg_write(RegisterX86::EFLAGS, tc.flags as u64 | 0x0002)
        .unwrap();
    uc.emu_start(CODE_ADDR as u64, 0xFFFF, 0, 1).unwrap();
    let mut data = [0u8; 16];
    uc.mem_read(DATA_ADDR as u64, &mut data).unwrap();
    let mut stack = [0u8; STACK_WINDOW];
    uc.mem_read(STACK_BASE as u64, &mut stack).unwrap();
    St32 {
        regs: std::array::from_fn(|i| uc.reg_read(regs[i]).unwrap() as u32),
        flags: uc.reg_read(RegisterX86::EFLAGS).unwrap() as u16 & FLAG_MASK_ALL,
        ip: uc.reg_read(RegisterX86::IP).unwrap() as u16,
        data,
        stack,
    }
}

fn check32(templates: &[Template], n: usize, seed: u64) {
    let mut failures = Vec::new();
    for t in templates {
        let mut rng = Rng::new(seed ^ (t.name.len() as u64 * 0x9E37_79B9));
        for _ in 0..n {
            let tc = random_case(&mut rng, t);
            let a = ours(&tc);
            let Ok(b) = std::panic::catch_unwind(|| oracle(&tc)) else {
                failures.push(format!(
                    "[{}] code={:02x?} regs={:04x?}: Unicorn が例外 (テンプレートの入力が悪い)",
                    t.name, tc.code, tc.regs
                ));
                break;
            };
            let mask = FLAG_MASK_ALL & !t.undefined;
            let mut d = Vec::new();
            for i in 0..8 {
                if a.regs[i] != b.regs[i] {
                    d.push(format!(
                        "{}: ours={:08x} oracle={:08x}",
                        REG_NAMES[i], a.regs[i], b.regs[i]
                    ));
                }
            }
            if a.flags & mask != b.flags & mask {
                d.push(format!(
                    "flags: ours={} oracle={}",
                    flag_names(a.flags & mask),
                    flag_names(b.flags & mask)
                ));
            }
            if a.data != b.data {
                d.push(format!("data: ours={:02x?} oracle={:02x?}", a.data, b.data));
            }
            if a.stack != b.stack {
                d.push("stack differs".into());
            }
            if a.ip != b.ip {
                d.push(format!("ip: ours={:04x} oracle={:04x}", a.ip, b.ip));
            }
            if !d.is_empty() {
                failures.push(format!(
                    "[{}] code={:02x?} regs={:04x?} flags_in={}\n  {}",
                    t.name,
                    tc.code,
                    tc.regs,
                    flag_names(tc.flags),
                    d.join("\n  ")
                ));
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} テンプレートで不一致:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn reg_modrm(r: &mut Rng) -> u8 {
    // mod=11、reg と rm は SP/BP/SI/DI を避けない (レジスタ同士なので番地に使わない)
    0xC0 | (r.next_u16() as u8 & 0x3F)
}

fn mem_modrm(r: &mut Rng) -> u8 {
    // mod=00、rm=07 ([bx])、reg は乱数
    (r.next_u16() as u8 & 0x38) | 0x07
}

#[test]
fn alu32_shifts_bits_mul_div() {
    let templates = [
        Template {
            name: "shld r32,r32,imm8",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xA4, reg_modrm(r), r.next_u16() as u8 & 0x3F],
            fixup: fix_xlat,
        },
        Template {
            name: "shld r32,r32,cl",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xA5, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "shrd r32,r32,imm8",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xAC, reg_modrm(r), r.next_u16() as u8 & 0x3F],
            fixup: fix_xlat,
        },
        Template {
            name: "shrd r32,r32,cl",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xAD, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "shld m32,r32,imm8",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xA4, mem_modrm(r), r.next_u16() as u8 & 0x3F],
            fixup: fix_xlat,
        },
        Template {
            name: "shrd m32,r32,cl",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0x0F, 0xAD, mem_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "bsf r32,r32",
            undefined: (cpu::CF | cpu::OF | cpu::SF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0x0F, 0xBC, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "bsr r32,r32",
            undefined: (cpu::CF | cpu::OF | cpu::SF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0x0F, 0xBD, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "bsr r32,m32",
            undefined: (cpu::CF | cpu::OF | cpu::SF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0x0F, 0xBD, mem_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "bt/bts/btr/btc r32,r32",
            undefined: (cpu::OF | cpu::SF | cpu::AF | cpu::PF | cpu::ZF) as u16,
            build: |r| {
                vec![
                    0x66,
                    0x0F,
                    0xA3 | ((r.next_u16() as u8 & 3) << 3),
                    reg_modrm(r),
                ]
            },
            fixup: fix_xlat,
        },
        Template {
            name: "bt* r32,imm8 (grp8)",
            undefined: (cpu::OF | cpu::SF | cpu::AF | cpu::PF | cpu::ZF) as u16,
            build: |r| {
                vec![
                    0x66,
                    0x0F,
                    0xBA,
                    0xE0 | ((r.next_u16() as u8 & 3) << 3) | (r.next_u16() as u8 & 7),
                    r.next_u16() as u8,
                ]
            },
            fixup: fix_xlat,
        },
        Template {
            name: "mul r32",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0xF7, 0xE0 | (r.next_u16() as u8 & 7)],
            fixup: fix_xlat,
        },
        Template {
            name: "imul r32",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0xF7, 0xE8 | (r.next_u16() as u8 & 7)],
            fixup: fix_xlat,
        },
        Template {
            name: "mul m32",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |_| vec![0x66, 0xF7, 0x27],
            fixup: fix_xlat,
        },
        Template {
            name: "imul r32,r32",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0x0F, 0xAF, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "imul r32,r32,imm8",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |r| vec![0x66, 0x6B, reg_modrm(r), r.next_u16() as u8],
            fixup: fix_xlat,
        },
        Template {
            name: "imul r32,r32,imm32",
            undefined: (cpu::SF | cpu::ZF | cpu::AF | cpu::PF) as u16,
            build: |r| {
                let v = r.next_u64() as u32;
                let mut c = vec![0x66, 0x69, reg_modrm(r)];
                c.extend_from_slice(&v.to_le_bytes());
                c
            },
            fixup: fix_xlat,
        },
        // div/idiv は 0 除算と溢れを避ける: 除数は CX (≠0) に固定して EDX を小さく
        Template {
            name: "div ecx",
            undefined: FLAG_MASK_ALL,
            build: |_| vec![0x66, 0xF7, 0xF1],
            fixup: |r| {
                // ECX の上位 16bit は r[4] (SP) の鏡写し — そこを立てて ECX ≥ 2^31 > EDX (16bit) に
                r[4] |= 0x8000;
                fix_xlat(r)
            },
        },
        Template {
            name: "idiv ecx",
            undefined: FLAG_MASK_ALL,
            build: |_| vec![0x66, 0xF7, 0xF9],
            fixup: |r| {
                r[4] = (r[4] & 0x7FFF) | 0x0100; // ECX は正で 2^24 以上 (上位は r[4] の鏡写し)。-1 で割ると溢れる
                r[2] &= 0x00FF; // |EDX:EAX| < 2^40 → 商は 2^16 未満で溢れない
                fix_xlat(r)
            },
        },
        Template {
            name: "bswap",
            undefined: 0,
            build: |r| vec![0x66, 0x0F, 0xC8 | (r.next_u16() as u8 & 7)],
            fixup: fix_xlat,
        },
        Template {
            name: "adc r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x13, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "sbb r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x1B, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "add/sub/xor/or/and/cmp r32,m32",
            undefined: 0,
            build: |r| vec![0x66, 0x03 | ((r.next_u16() as u8 & 7) << 3), mem_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "shift/rot r32,cl (grp2)",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0xD3, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "shift/rot r32,imm8 (grp2)",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0xC1, reg_modrm(r), r.next_u16() as u8 & 0x3F],
            fixup: fix_xlat,
        },
        Template {
            name: "shift/rot m32,1",
            undefined: (cpu::AF | cpu::OF) as u16,
            build: |r| vec![0x66, 0xD1, mem_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "neg/not r32 (grp3)",
            undefined: 0,
            build: |r| {
                vec![
                    0x66,
                    0xF7,
                    0xD0 | ((r.next_u16() as u8 & 1) << 3) | (r.next_u16() as u8 & 7),
                ]
            },
            fixup: fix_xlat,
        },
        Template {
            name: "inc/dec r32",
            undefined: 0,
            build: |r| vec![0x66, 0x40 | (r.next_u16() as u8 & 0xF)],
            fixup: fix_xlat,
        },
        Template {
            name: "movzx/movsx r32,r/m8|16",
            undefined: 0,
            build: |r| {
                vec![
                    0x66,
                    0x0F,
                    0xB6 | (r.next_u16() as u8 & 1) | ((r.next_u16() as u8 & 1) << 3),
                    reg_modrm(r),
                ]
            },
            fixup: fix_xlat,
        },
        Template {
            name: "cdq",
            undefined: 0,
            build: |_| vec![0x66, 0x99],
            fixup: fix_xlat,
        },
        Template {
            name: "cwde",
            undefined: 0,
            build: |_| vec![0x66, 0x98],
            fixup: fix_xlat,
        },
        Template {
            name: "xadd r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x0F, 0xC1, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "cmpxchg r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x0F, 0xB1, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "cmovcc r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x0F, 0x40 | (r.next_u16() as u8 & 0xF), reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "setcc r/m8",
            undefined: 0,
            build: |r| vec![0x0F, 0x90 | (r.next_u16() as u8 & 0xF), reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "test r32,r32",
            undefined: 0,
            build: |r| vec![0x66, 0x85, reg_modrm(r)],
            fixup: fix_xlat,
        },
        Template {
            name: "lea r32,[bx+disp]",
            undefined: 0,
            build: |r| {
                vec![
                    0x66,
                    0x8D,
                    0x47 | ((r.next_u16() as u8 & 7) << 3),
                    r.next_u16() as u8,
                ]
            },
            fixup: fix_xlat,
        },
    ];
    check32(&templates, 300, 0xA1B2_C3D4);
}
