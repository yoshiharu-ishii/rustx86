//! Unicornをオラクルにした比較実行テスト。
//!
//! 命令テンプレート (オペコード + オペランドの形) を定義し、レジスタ初期値と
//! 即値だけをランダムに振ってケースを大量生成する。ランダムなバイト列を撒くより
//! 有効な命令に当たる確率がはるかに高い。

use rustx86_cosim::*;
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

/// Unicornで1命令実行してオラクルの状態を得る
fn run_oracle(tc: &TestCase) -> State {
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_16).expect("unicorn init");
    uc.mem_map(0, 0x10000, Prot::ALL).expect("map");
    uc.mem_write(CODE_ADDR as u64, &tc.code).expect("write code");
    uc.mem_write(DATA_ADDR as u64, &tc.data).expect("write data");

    let regs = [
        RegisterX86::AX,
        RegisterX86::CX,
        RegisterX86::DX,
        RegisterX86::BX,
        RegisterX86::SP,
        RegisterX86::BP,
        RegisterX86::SI,
        RegisterX86::DI,
    ];
    for (i, r) in regs.iter().enumerate() {
        uc.reg_write(*r, tc.regs[i] as u64).expect("reg");
    }
    for s in [RegisterX86::CS, RegisterX86::DS, RegisterX86::ES, RegisterX86::SS] {
        uc.reg_write(s, 0).expect("sreg");
    }
    uc.reg_write(RegisterX86::EFLAGS, tc.flags as u64 | 0x0002)
        .expect("flags");

    // 1命令だけ実行 (until=0で無効化し、count=1で止める)
    uc.emu_start(CODE_ADDR as u64, 0xFFFF, 0, 1).expect("emu");

    let mut out_regs = [0u16; 8];
    for (i, r) in regs.iter().enumerate() {
        out_regs[i] = uc.reg_read(*r).expect("read reg") as u16;
    }
    let flags = uc.reg_read(RegisterX86::EFLAGS).expect("read flags") as u16;
    let ip = uc.reg_read(RegisterX86::IP).expect("read ip") as u16;
    let mut data = [0u8; 16];
    uc.mem_read(DATA_ADDR as u64, &mut data).expect("read data");

    State {
        regs: out_regs,
        flags: flags & FLAG_MASK_ALL,
        ip,
        data,
    }
}

/// 命令テンプレート: ランダム値から命令バイト列を組み立てる
struct Template {
    name: &'static str,
    /// 比較から除外するフラグ (x86が未定義と定めるもの)
    undefined: u16,
    build: fn(&mut Rng) -> Vec<u8>,
}

fn random_case(rng: &mut Rng, t: &Template) -> TestCase {
    TestCase {
        code: (t.build)(rng),
        regs: std::array::from_fn(|_| rng.interesting_u16()),
        flags: (rng.next_u16() & FLAG_MASK_ALL),
        data: std::array::from_fn(|_| rng.interesting_u8()),
    }
}

fn check(templates: &[Template], cases_per_template: usize, seed: u64) {
    let mut failures = Vec::new();
    for t in templates {
        let mut rng = Rng::new(seed ^ t.name.len() as u64 * 0x9E37_79B9);
        let mut checked = 0;
        for _ in 0..cases_per_template {
            let tc = random_case(&mut rng, t);
            let ours = run_ours(&tc);
            let oracle = run_oracle(&tc);
            checked += 1;
            if let Some(d) = diff(&ours, &oracle, FLAG_MASK_ALL & !t.undefined) {
                failures.push(format!(
                    "[{}] code={:02x?} regs={:04x?} flags_in={}\n  {}",
                    t.name,
                    tc.code,
                    tc.regs,
                    flag_names(tc.flags),
                    d
                ));
                break; // テンプレートごとに最初の1件だけ報告
            }
        }
        eprintln!("{}: {checked} cases", t.name);
    }
    assert!(
        failures.is_empty(),
        "co-sim mismatch ({} template(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// ALUグリッド全8演算 x 主要6形式
#[test]
fn alu_grid() {
    // ADD OR ADC SBB AND SUB XOR CMP を (演算<<3) で並べる
    macro_rules! alu_templates {
        ($($name:literal => $base:expr),* $(,)?) => {
            vec![
                $(
                    Template { name: concat!($name, " r/m8,r8"),  undefined: 0, build: |r| vec![$base + 0x00, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " r/m16,r16"), undefined: 0, build: |r| vec![$base + 0x01, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " r8,r/m8"),   undefined: 0, build: |r| vec![$base + 0x02, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7)] },
                    Template { name: concat!($name, " AL,imm8"),   undefined: 0, build: |r| vec![$base + 0x04, r.interesting_u8()] },
                    Template { name: concat!($name, " AX,imm16"),  undefined: 0, build: |r| { let v = r.interesting_u16(); vec![$base + 0x05, v as u8, (v >> 8) as u8] } },
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
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 16);
                vec![0x00, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "SUB AX,[disp16]",
            undefined: 0,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 15);
                vec![0x2B, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "MOV [disp16],AX",
            undefined: 0,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 15);
                vec![0x89, 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "CMP [disp16],imm8",
            undefined: 0,
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
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                vec![0x80, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), r.interesting_u8()]
            },
        },
        Template {
            name: "GRP1 r/m16,imm16",
            undefined: 0,
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                let v = r.interesting_u16();
                vec![0x81, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), v as u8, (v >> 8) as u8]
            },
        },
        Template {
            name: "GRP1 r/m16,imm8 (符号拡張)",
            undefined: 0,
            build: |r| {
                let kind = r.next_u16() as u8 & 7;
                vec![0x83, 0xC0 | (kind << 3) | (r.next_u16() as u8 & 7), r.interesting_u8()]
            },
        },
        Template {
            name: "INC r16",
            undefined: 0,
            build: |r| vec![0x40 | (r.next_u16() as u8 & 7)],
        },
        Template {
            name: "DEC r16",
            undefined: 0,
            build: |r| vec![0x48 | (r.next_u16() as u8 & 7)],
        },
    ];
    check(&templates, 300, 0x5EED_0007);
}
