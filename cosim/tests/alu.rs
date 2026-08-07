//! ALU・GRP1-5・シフト・十進補正・ストリング命令のco-simテスト。
//!
//! ハーネス (Unicornオラクル、テンプレート、乱数生成) は `rustx86_cosim` 本体にある。

use rustx86_cosim::*;

/// ALUグリッド全8演算 x 主要6形式
#[test]
fn alu_grid() {
    // ADD OR ADC SBB AND SUB XOR CMP を (演算<<3) で並べる
    macro_rules! alu_templates {
        ($($name:literal => $base:expr),* $(,)?) => {
            vec![
                $(
                    Template { name: concat!($name, " r/m8,r8"),  undefined: 0, fixup: nofix, build: |r| vec![$base + 0x00, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " r/m16,r16"), undefined: 0, fixup: nofix, build: |r| vec![$base + 0x01, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " r8,r/m8"),   undefined: 0, fixup: nofix, build: |r| vec![$base + 0x02, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " AL,imm8"),   undefined: 0, fixup: nofix, build: |r| vec![$base + 0x04, r.interesting_u8()] },
                    Template { name: concat!($name, " AX,imm16"),  undefined: 0, fixup: nofix, build: |r| { let v = r.interesting_u16(); vec![$base + 0x05, v as u8, (v >> 8) as u8] } },
                )*
            ]
        };
    }
    let templates: Vec<Template> = alu_templates![
        "ADD" => 0x00u8,
        "OR"  => 0x08u8,
        "ADC" => 0x10u8,
        "SBB" => 0x18u8,
        "AND" => 0x20u8,
        "SUB" => 0x28u8,
        "XOR" => 0x30u8,
        "CMP" => 0x38u8,
    ];
    check(&templates, 200, 0xC0DE_1234);
}

/// メモリオペランド経由のALU (ModRMのアドレッシング検証を兼ねる)
#[test]
fn alu_memory_operands() {
    let templates = vec![
        // ADD [BX+SI], AL / ADD AL, [BX+SI] など。BX,SIはケース生成時にランダムなので
        // 実効アドレスがDATA_ADDR近傍に来るようdisp16形式を使う
        Template {
            name: "ADD [disp16],AL",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 16);
                vec![0x00, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "SUB AX,[disp16]",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 15);
                vec![0x2B, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "MOV [disp16],AX",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 15);
                vec![0x89, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "CMP [disp16],imm8",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 16);
                vec![0x80, 0x3E, off as u8, (off >> 8) as u8, r.interesting_u8()]
            },
        },
    ];
    check(&templates, 200, 0xBEEF_0001);
}

/// GRP1 (r/m, imm) と INC/DEC
#[test]
fn grp1_and_incdec() {
    let templates = vec![
        Template {
            name: "GRP1 r/m8,imm8",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                vec![0x80, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), r.interesting_u8()]
            },
        },
        Template {
            name: "GRP1 r/m16,imm16",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                let v = r.interesting_u16();
                vec![0x81, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), v as u8, (v >> 8) as u8]
            },
        },
        Template {
            name: "GRP1 r/m16,imm8 (符号拡張)",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                vec![0x83, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), r.interesting_u8()]
            },
        },
        Template {
            name: "INC r16",
            undefined: 0,
            fixup: nofix,
            build: |r| vec![0x40 | (r.next_u16() as u8 & 7)],
        },
        Template {
            name: "DEC r16",
            undefined: 0,
            fixup: nofix,
            build: |r| vec![0x48 | (r.next_u16() as u8 & 7)],
        },
    ];
    check(&templates, 300, 0x5EED_0007);
}


/// GRP2: シフトと回転。AFは常に未定義、OFはカウント1のときだけ定義される
#[test]
fn shifts_and_rotates() {
    fn modrm_byte(r: &mut Rng) -> u8 {
        0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)
    }
    let templates = vec![
        Template { name: "GRP2 r/m8,1", undefined: UD_AF, fixup: nofix,
            build: |r| vec![0xD0, modrm_byte(r)] },
        Template { name: "GRP2 r/m16,1", undefined: UD_AF, fixup: nofix,
            build: |r| vec![0xD1, modrm_byte(r)] },
        Template { name: "GRP2 r/m8,imm8", undefined: UD_AF | UD_OF, fixup: nofix,
            build: |r| vec![0xC0, modrm_byte(r), (r.next_u16() as u8) & 0x1F] },
        Template { name: "GRP2 r/m16,imm8", undefined: UD_AF | UD_OF, fixup: nofix,
            build: |r| vec![0xC1, modrm_byte(r), (r.next_u16() as u8) & 0x1F] },
        Template { name: "GRP2 r/m8,CL", undefined: UD_AF | UD_OF, fixup: nofix,
            build: |r| vec![0xD2, modrm_byte(r)] },
        Template { name: "GRP2 r/m16,CL", undefined: UD_AF | UD_OF, fixup: nofix,
            build: |r| vec![0xD3, modrm_byte(r)] },
    ];
    check(&templates, 400, 0x5417_0000);
}

/// GRP3: TEST/NOT/NEG/MUL/IMUL/DIV/IDIV
#[test]
fn grp3_mul_div() {
    // MUL/IMULはCFとOFのみ定義。DIV/IDIVは全フラグ未定義 (レジスタ結果で検証する)
    const UD_MUL: u16 = UD_SF | UD_ZF | UD_AF | UD_PF;
    const UD_DIV: u16 = UD_CF | UD_PF | UD_AF | UD_ZF | UD_SF | UD_OF;
    let templates = vec![
        Template { name: "TEST r/m8,imm8", undefined: 0, fixup: nofix,
            build: |r| vec![0xF6, 0xC0 | (r.next_u16() as u8 & 7), r.interesting_u8()] },
        Template { name: "NOT r/m16", undefined: 0, fixup: nofix,
            build: |r| vec![0xF7, 0xD0 | (r.next_u16() as u8 & 7)] },
        Template { name: "NEG r/m8", undefined: 0, fixup: nofix,
            build: |r| vec![0xF6, 0xD8 | (r.next_u16() as u8 & 7)] },
        Template { name: "NEG r/m16", undefined: 0, fixup: nofix,
            build: |r| vec![0xF7, 0xD8 | (r.next_u16() as u8 & 7)] },
        Template { name: "MUL r/m8", undefined: UD_MUL, fixup: nofix,
            build: |r| vec![0xF6, 0xE0 | (r.next_u16() as u8 & 7)] },
        Template { name: "IMUL r/m8", undefined: UD_MUL, fixup: nofix,
            build: |r| vec![0xF6, 0xE8 | (r.next_u16() as u8 & 7)] },
        Template { name: "MUL r/m16", undefined: UD_MUL, fixup: nofix,
            build: |r| vec![0xF7, 0xE0 | (r.next_u16() as u8 & 7)] },
        Template { name: "IMUL r/m16", undefined: UD_MUL, fixup: nofix,
            build: |r| vec![0xF7, 0xE8 | (r.next_u16() as u8 & 7)] },
        // DIVは除数CL/CXを非ゼロにし、商が収まる範囲に被除数を絞る
        Template { name: "DIV r/m8 (CL)", undefined: UD_DIV,
            fixup: |regs| { regs[0] &= 0x00FF; regs[1] |= 0x0001; },
            build: |_| vec![0xF6, 0xF1] },
        Template { name: "IDIV r/m8 (CL)", undefined: UD_DIV,
            fixup: |regs| { regs[0] &= 0x007F; regs[1] = (regs[1] & 0x007F) | 1; },
            build: |_| vec![0xF6, 0xF9] },
        Template { name: "DIV r/m16 (CX)", undefined: UD_DIV,
            fixup: |regs| { regs[2] = 0; regs[1] |= 0x0001; },
            build: |_| vec![0xF7, 0xF1] },
        Template { name: "IDIV r/m16 (CX)", undefined: UD_DIV,
            fixup: |regs| { regs[2] = 0; regs[0] &= 0x7FFF; regs[1] = (regs[1] & 0x7FFF) | 1; },
            build: |_| vec![0xF7, 0xF9] },
    ];
    check(&templates, 300, 0x3D1F_0042);
}

/// TEST/XCHG/LEA/CBW/CWD/SAHF/LAHF/PUSHF/POPF と GRP4/5
#[test]
fn misc_instructions() {
    fn modrm_byte(r: &mut Rng) -> u8 {
        0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)
    }
    let templates = vec![
        Template { name: "TEST r/m8,r8", undefined: 0, fixup: nofix,
            build: |r| vec![0x84, modrm_byte(r)] },
        Template { name: "TEST r/m16,r16", undefined: 0, fixup: nofix,
            build: |r| vec![0x85, modrm_byte(r)] },
        Template { name: "XCHG r/m8,r8", undefined: 0, fixup: nofix,
            build: |r| vec![0x86, modrm_byte(r)] },
        Template { name: "XCHG r/m16,r16", undefined: 0, fixup: nofix,
            build: |r| vec![0x87, modrm_byte(r)] },
        Template { name: "XCHG AX,r16", undefined: 0, fixup: nofix,
            build: |r| vec![0x90 | (r.next_u16() as u8 & 7)] },
        Template { name: "LEA r16,[disp16]", undefined: 0, fixup: nofix,
            build: |r| { let off = r.interesting_u16();
                vec![0x8D, 0x06 | ((r.next_u16() as u8 & 7) << 3), off as u8, (off >> 8) as u8] } },
        Template { name: "CBW", undefined: 0, fixup: nofix, build: |_| vec![0x98] },
        Template { name: "CWD", undefined: 0, fixup: nofix, build: |_| vec![0x99] },
        Template { name: "SAHF", undefined: 0, fixup: nofix, build: |_| vec![0x9E] },
        Template { name: "LAHF", undefined: 0, fixup: nofix, build: |_| vec![0x9F] },
        Template { name: "PUSHF", undefined: 0, fixup: nofix, build: |_| vec![0x9C] },
        Template { name: "POPF", undefined: 0, fixup: nofix, build: |_| vec![0x9D] },
        Template { name: "INC/DEC r/m8 (GRP4)", undefined: 0, fixup: nofix,
            build: |r| vec![0xFE, 0xC0 | ((r.next_u16() as u8 & 1) << 3) | (r.next_u16() as u8 & 7)] },
        Template { name: "INC/DEC r/m16 (GRP5)", undefined: 0, fixup: nofix,
            build: |r| vec![0xFF, 0xC0 | ((r.next_u16() as u8 & 1) << 3) | (r.next_u16() as u8 & 7)] },
        Template { name: "PUSH r/m16 (GRP5 /6)", undefined: 0, fixup: nofix,
            build: |r| vec![0xFF, 0xF0 | (r.next_u16() as u8 & 7)] },
        Template { name: "POP r/m16", undefined: 0, fixup: nofix,
            build: |r| vec![0x8F, 0xC0 | (r.next_u16() as u8 & 7)] },
    ];
    check(&templates, 300, 0x9111_0007);
}

/// 十進補正 (DAA/DAS/AAA/AAS/AAM/AAD)。BCD演算のための歴史的命令。
///
/// これらはALの値とCF/AFだけで分岐が決まる。状態空間が 256 x 3 x 4 と小さいので
/// ランダムではなく総当たりで検証する。ランダムだと 0x9A のような特定の境界値を
/// 踏み損ねて、バグを見逃すことが実際にあった。
#[test]
fn decimal_adjust() {
    check_cases("DAA", UD_OF, sweep_al(vec![0x27]));
    check_cases("DAS", UD_OF, sweep_al(vec![0x2F]));
    check_cases("AAA", UD_OF | UD_SF | UD_ZF | UD_PF, sweep_al(vec![0x37]));
    check_cases("AAS", UD_OF | UD_SF | UD_ZF | UD_PF, sweep_al(vec![0x3F]));
    for base in [1u8, 8, 10, 16, 100, 255] {
        check_cases("AAM imm8", UD_OF | UD_AF | UD_CF, sweep_al(vec![0xD4, base]));
        check_cases("AAD imm8", UD_OF | UD_AF | UD_CF, sweep_al(vec![0xD5, base]));
    }
}

/// ストリング命令 (REPなし1回分)。DFによる方向とSI/DI更新を検証する
///
/// SI/DIはデータ窓に収める。ランダムなままだとコード領域に書き込むケースが出て、
/// QEMU (Unicorn) が自己書き換えコードとみなして命令を中断し、IPが進まない
/// 「実行されなかった」状態と比較してしまう (自作CPU側のバグではない)。
#[test]
fn string_instructions() {
    fn in_data_window(regs: &mut [u16; 8]) {
        regs[6] = DATA_ADDR + (regs[6] % 14); // SI
        regs[7] = DATA_ADDR + (regs[7] % 14); // DI
    }
    let templates = vec![
        Template { name: "MOVSB", undefined: 0, fixup: in_data_window, build: |_| vec![0xA4] },
        Template { name: "MOVSW", undefined: 0, fixup: in_data_window, build: |_| vec![0xA5] },
        Template { name: "CMPSB", undefined: 0, fixup: in_data_window, build: |_| vec![0xA6] },
        Template { name: "CMPSW", undefined: 0, fixup: in_data_window, build: |_| vec![0xA7] },
        Template { name: "STOSB", undefined: 0, fixup: in_data_window, build: |_| vec![0xAA] },
        Template { name: "STOSW", undefined: 0, fixup: in_data_window, build: |_| vec![0xAB] },
        Template { name: "LODSB", undefined: 0, fixup: in_data_window, build: |_| vec![0xAC] },
        Template { name: "LODSW", undefined: 0, fixup: in_data_window, build: |_| vec![0xAD] },
        Template { name: "SCASB", undefined: 0, fixup: in_data_window, build: |_| vec![0xAE] },
        Template { name: "SCASW", undefined: 0, fixup: in_data_window, build: |_| vec![0xAF] },
    ];
    check(&templates, 300, 0x5712_0000);
}

