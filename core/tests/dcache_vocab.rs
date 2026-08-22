//! デコード済みキャッシュ (dcache) の語彙が従来経路と同じ意味論か — 両経路差分。
//!
//! dcache はデバッガOFFのときだけ働く (mod.rs の約束)。同じ命令列を
//! **dbg.on=false (dcache) と dbg.on=true (従来経路)** で走らせ、レジスタ・
//! EFLAGS・データ領域をバイト単位で突き合わせる。語彙を足すたびにここへ
//! 命令を足せば、「速い写し」が原本からずれた瞬間に割れる。
//!
//! 初出は C16 (ADR-0028): X窓の従来経路落ちの正体 — movsx / cmovcc /
//! test eAX,imm / imul 3オペランド / cdq / bsf,bsr / shld,shrd。

use rustx86_core::{cpu, Machine};

/// pm_hello で32bitプロテクトモードまで運ぶ (CS=0x08 flat 32bit、DS=0x10 flat)
const PM_IMAGE: &[u8] = include_bytes!("../../asm/pm_hello.bin");

const CODE: u32 = 0x2000;
const DATA: u32 = 0x3000;

fn run_pm(code: &[u8], legacy: bool) -> Machine {
    let mut m = Machine::new();
    m.load_boot_sector(PM_IMAGE).unwrap();
    for _ in 0..10_000 {
        if m.halted {
            break;
        }
        m.step();
    }
    assert!(m.halted, "pm_helloがHLTに着いていない");
    assert!(m.cpu.seg_is32(cpu::CS), "32bitコードに到達していない");

    for (i, b) in code.iter().enumerate() {
        m.write_phys8(CODE + i as u32, *b);
    }
    for i in 0..0x40 {
        m.write_phys8(DATA + i, 0);
    }
    for r in 0..8 {
        m.cpu.regs[r] = 0;
    }
    m.dbg.on = legacy;
    let fills0 = m.dcache.fills; // 足場 (pm_hello) の分は数えない
    m.cpu.ip = CODE;
    m.halted = false;
    m.run(10_000);
    assert!(m.trap.is_none(), "trapした: {:?}", m.trap);
    assert!(m.halted, "HLTに到達していない");
    // 土台の自己検査: dcache 側は本当に写しを使い、従来側は一度も使っていない
    let fills = m.dcache.fills - fills0;
    if legacy {
        assert_eq!(fills, 0, "dbg.on なのに dcache が働いた");
    } else {
        assert!(fills > 0, "dcache が一度も使われていない (テストが空回り)");
    }
    m
}

fn snapshot(m: &Machine) -> (Vec<u32>, u32, Vec<u8>) {
    let regs = (0..8).map(|r| m.cpu.regs[r]).collect();
    let mem = (0..0x40).map(|i| m.read_phys8(DATA + i)).collect();
    (regs, m.cpu.eflags(), mem)
}

/// 両経路で走らせて、見える状態が一致することを確かめる
fn assert_same(code: &[u8]) {
    let a = snapshot(&run_pm(code, false));
    let b = snapshot(&run_pm(code, true));
    assert_eq!(a.0, b.0, "レジスタが両経路で違う (dcache vs 従来)");
    assert_eq!(a.1, b.1, "EFLAGSが両経路で違う: {:08x} vs {:08x}", a.1, b.1);
    assert_eq!(a.2, b.2, "データ領域が両経路で違う");
}

fn d32(a: u32) -> [u8; 4] {
    a.to_le_bytes()
}

/// pushfd; pop dword [DATA+off] — その時点の EFLAGS をデータ領域に残す
fn save_flags(off: u32) -> Vec<u8> {
    let [a0, a1, a2, a3] = d32(DATA + off);
    vec![0x9C, 0x8F, 0x05, a0, a1, a2, a3]
}

#[test]
fn c16の語彙は従来経路と同じ意味論() {
    let [d0, d1, d2, d3] = d32(DATA);
    let [e0, e1, e2, e3] = d32(DATA + 4);
    let [z0, z1, z2, z3] = d32(DATA + 8); // ゼロのまま (bsf のソース0)
    #[rustfmt::skip]
    let mut code: Vec<u8> = vec![
        0xB8, 0x80, 0x56, 0x34, 0x12,             // mov eax, 0x12345680
        0xA3, d0, d1, d2, d3,                     // mov [DATA], eax
        0xBB, 0x85, 0xFF, 0xFF, 0xFF,             // mov ebx, 0xFFFFFF85
        0x89, 0x1D, e0, e1, e2, e3,               // mov [DATA+4], ebx
        // movsx — 8bit/16bit × reg/mem
        0x0F, 0xBE, 0x0D, d0, d1, d2, d3,         // movsx ecx, byte [DATA]  (0x80 → 負)
        0x0F, 0xBE, 0xD3,                         // movsx edx, bl
        0x0F, 0xBF, 0x35, d0, d1, d2, d3,         // movsx esi, word [DATA] (0x5680 → 正)
        0x0F, 0xBF, 0xFB,                         // movsx edi, bx
        // cmovcc — 条件の真偽両方、mem形
        0x39, 0xD8,                               // cmp eax, ebx
        0x0F, 0x4C, 0xE8,                         // cmovl ebp, eax (偽)
        0x0F, 0x4F, 0xEB,                         // cmovg ebp, ebx (真)
        0x0F, 0x44, 0x0D, e0, e1, e2, e3,         // cmovz ecx, [DATA+4] (偽、読みは行う)
        // test eAX, imm
        0xA8, 0x80,                               // test al, 0x80
    ];
    code.extend(save_flags(0x10));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0xA9, 0x00, 0x00, 0x40, 0x00,             // test eax, 0x00400000
    ]);
    code.extend(save_flags(0x14));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        // imul 3オペランド — imm32 / imm8符号拡張 / mem形
        0x69, 0xCB, 0x34, 0x12, 0x00, 0x00,       // imul ecx, ebx, 0x1234
        0x6B, 0x15, d0, d1, d2, d3, 0xFD,         // imul edx, [DATA], -3
    ]);
    code.extend(save_flags(0x18));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0x69, 0xC0, 0x00, 0x00, 0x01, 0x00,       // imul eax, eax, 0x10000 (溢れ → CF/OF)
    ]);
    code.extend(save_flags(0x1C));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0x99,                                     // cdq
        // bsf/bsr — reg/mem/ソース0
        0x0F, 0xBC, 0xF7,                         // bsf esi, edi
        0x0F, 0xBD, 0x3D, e0, e1, e2, e3,         // bsr edi, [DATA+4]
    ]);
    code.extend(save_flags(0x20));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0x0F, 0xBC, 0x2D, z0, z1, z2, z3,         // bsf ebp, [DATA+8] (=0 → ZF=1、ebp不変)
    ]);
    code.extend(save_flags(0x24));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        // shld/shrd — imm / CL / mem形 / count 0 / count 31
        0x0F, 0xA4, 0xD8, 0x05,                   // shld eax, ebx, 5
    ]);
    code.extend(save_flags(0x28));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0xB1, 0x0D,                               // mov cl, 13
        0x0F, 0xAD, 0x1D, d0, d1, d2, d3,         // shrd [DATA], ebx, cl
        0x0F, 0xA5, 0xC2,                         // shld edx, eax, cl
    ]);
    code.extend(save_flags(0x2C));
    #[rustfmt::skip]
    code.extend_from_slice(&[
        0x0F, 0xAC, 0xF3, 0x00,                   // shrd ebx, esi, 0 (何もしない・フラグ不変)
        0x0F, 0xAC, 0xF3, 0x3F,                   // shrd ebx, esi, 63 (&0x1F = 31)
    ]);
    code.extend(save_flags(0x30));
    code.push(0xF4); // hlt
    assert_same(&code);
}

/// ゼロ拡張側 (既存語彙) も同じ型で守る — 差分テストの土台が動いている証拠
#[test]
fn 既存語彙のmovzxも両経路で一致する() {
    let [d0, d1, d2, d3] = d32(DATA);
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0xB8, 0x80, 0xFF, 0x34, 0x12,             // mov eax, 0x1234FF80
        0xA3, d0, d1, d2, d3,                     // mov [DATA], eax
        0x0F, 0xB6, 0x0D, d0, d1, d2, d3,         // movzx ecx, byte [DATA]
        0x0F, 0xB7, 0xD0,                         // movzx edx, ax
        0xF4,
    ];
    assert_same(&code);
}
