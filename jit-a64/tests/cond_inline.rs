//! 条件インライン化 (g2、ADR-0023) の全数照合。
//!
//! 「材料→条件」のARM写像は意味論の二重実装に半歩踏み込む — その守りが
//! このテスト。全kind (add/or/and/sub/xor/cmp) × 全16cc × 両幅 × 境界値
//! オペランドで `alu; setcc` を **JIT on/off両方で実行し、setccの結果と
//! 6フラグが完全一致**することを確かめる。cc死储の省略 (g1) も
//! `alu; alu; setcc` の3連で踏む。
//!
//! 8bitの符号境界 (0x7F/0x80) とキャリー境界 (0/0xFF)、32bitの同型
//! (0x7FFFFFFF/0x80000000) を必ず含める — lsl 24の桁合わせとlo/lt系の
//! 写像ミスはここで赤くなる。

use rustx86_core::{cpu, Machine};

const PM_IMAGE: &[u8] = include_bytes!("../../asm/pm_hello.bin");
const CODE: u32 = 0x2000;

/// 32bitモード到達済みのマシンを作る
fn pm_machine() -> Box<Machine> {
    let mut m = Box::new(Machine::new());
    m.load_boot_sector(PM_IMAGE).unwrap();
    for _ in 0..10_000 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted && m.cpu.seg_is32(cpu::CS), "32bitコード未到達");
    m
}

/// コードを置いて実行し、(CL, 6フラグ) を返す
fn run_case(m: &mut Machine, code: &[u8], a: u32, b: u32) -> (u8, [bool; 6]) {
    for (i, &byte) in code.iter().enumerate() {
        m.write_phys8(CODE + i as u32, byte);
    }
    m.cpu.regs[cpu::AX] = a;
    m.cpu.regs[cpu::BX] = b;
    m.cpu.regs[cpu::CX] = 0xAAAA_AAAA; // 前の値が残ればすぐ分かる
    m.cpu.ip = CODE;
    m.halted = false;
    m.run(1_000);
    assert!(m.trap.is_none(), "trap: {:?}", m.trap);
    assert!(m.halted, "HLT未到達");
    let flags = [
        m.cpu.flag(cpu::CF),
        m.cpu.flag(cpu::PF),
        m.cpu.flag(cpu::AF),
        m.cpu.flag(cpu::ZF),
        m.cpu.flag(cpu::SF),
        m.cpu.flag(cpu::OF),
    ];
    ((m.cpu.regs[cpu::CX] & 0xFF) as u8, flags)
}

/// `alu(kind) eax,ebx (or al,bl); setcc cl; hlt` を組む。
/// 先頭のmov edx×2は最小ブロック長 (4op) を満たすためのパディング
fn case_code(kind: u8, cc: u8, wide: bool) -> Vec<u8> {
    let alu_op = (kind << 3) | if wide { 0x01 } else { 0x00 };
    vec![
        0xBA,
        0x11,
        0x11,
        0x11,
        0x11, // mov edx, imm
        0xBA,
        0x22,
        0x22,
        0x22,
        0x22, // mov edx, imm
        alu_op,
        0xD8, // alu rm=eAX/AL, reg=eBX/BL
        0x0F,
        0x90 + cc,
        0xC1, // setcc cl
        0xF4, // hlt
    ]
}

/// g1も踏む3連: `alu1 eax,ebx; alu2 eax,ebx; setcc cl; hlt`
fn chain_code(kind1: u8, kind2: u8, cc: u8) -> Vec<u8> {
    vec![
        0xBA,
        0x11,
        0x11,
        0x11,
        0x11, // mov edx, imm (パディング)
        (kind1 << 3) | 0x01,
        0xD8,
        (kind2 << 3) | 0x01,
        0xD8,
        0x0F,
        0x90 + cc,
        0xC1,
        0xF4,
    ]
}

const OPERANDS32: [u32; 8] = [
    0,
    1,
    2,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0xFFFF_FFFE,
    0x1234_5678,
];
const OPERANDS8: [u32; 8] = [0, 1, 2, 0x7F, 0x80, 0x81, 0xFE, 0xFF];

#[test]
fn 条件インラインは全kind全ccで遅延評価器とビット一致() {
    let mut m_ref = pm_machine();
    let mut m_jit = pm_machine();
    unsafe { rustx86_jit_a64::attach(&mut m_jit) };

    // ADC/SBB (2,3) はインライン対象外だがフォールバック経路の検証として含める
    for kind in 0..8u8 {
        for cc in 0..16u8 {
            for wide in [true, false] {
                let code = case_code(kind, cc, wide);
                let opers = if wide { &OPERANDS32 } else { &OPERANDS8 };
                for &a in opers {
                    for &b in opers {
                        let r = run_case(&mut m_ref, &code, a, b);
                        let j = run_case(&mut m_jit, &code, a, b);
                        assert_eq!(
                            r, j,
                            "kind={kind} cc={cc} wide={wide} a={a:#x} b={b:#x}: interp={r:?} jit={j:?}"
                        );
                    }
                }
            }
        }
    }
    assert!(
        m_jit.jit_instrs > 0,
        "JITが一度も入っていない — テストが空回り"
    );
}

#[test]
fn cc死储の省略はフラグの観測値を変えない() {
    let mut m_ref = pm_machine();
    let mut m_jit = pm_machine();
    unsafe { rustx86_jit_a64::attach(&mut m_jit) };

    for kind1 in [0u8, 1, 4, 5, 6] {
        for kind2 in [4u8, 5, 7] {
            for cc in 0..16u8 {
                let code = chain_code(kind1, kind2, cc);
                for &a in &[0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
                    for &b in &[0u32, 1, 0x8000_0000, 0xFFFF_FFFF] {
                        let r = run_case(&mut m_ref, &code, a, b);
                        let j = run_case(&mut m_jit, &code, a, b);
                        assert_eq!(
                            r, j,
                            "kind1={kind1} kind2={kind2} cc={cc} a={a:#x} b={b:#x}"
                        );
                    }
                }
            }
        }
    }
    assert!(
        m_jit.jit_instrs > 0,
        "JITが一度も入っていない — テストが空回り"
    );
}
