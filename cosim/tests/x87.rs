//! x87 を Unicorn と突き合わせる。**glibc が壊れて musl が動く**の切り分けで生まれた —
//! glibc の printf/strtod は 80bit (FLD/FSTP m80) の**ビット**を見る (mpn で分解) ので、
//! x87 の格納形式・変換・比較が 1bit でもずれると "+nan" / "inf" になる (DSL 2024、2026-08-23)。
//! musl は FP 演算だけで済ませるのでずれが見えない。
//!
//! 1 命令ずつではなく**短い列**を走らせ、メモリの窓 (data 16B / stack 32B) と AX・flags を比べる
use rustx86_core::{cpu, Machine, MachineProfile};
use rustx86_cosim::*;
use unicorn_engine::{Arch, Mode, Prot, RegisterX86, Unicorn};

fn ours(code: &[u8], data: &[u8; 16], stack: &[u8; STACK_WINDOW]) -> State {
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(2));
    for (i, b) in code.iter().enumerate() {
        m.write8(CODE_ADDR as u32 + i as u32, *b);
    }
    for (i, b) in data.iter().enumerate() {
        m.write8(DATA_ADDR as u32 + i as u32, *b);
    }
    for (i, b) in stack.iter().enumerate() {
        m.write8(STACK_BASE as u32 + i as u32, *b);
    }
    m.cpu.regs[cpu::BX] = DATA_ADDR as u32;
    m.cpu.regs[cpu::DI] = STACK_BASE as u32;
    m.cpu.regs[cpu::SP] = STACK_INIT_SP as u32;
    m.cpu.set_eflags(0x0002);
    m.cpu.set_cs_ip(0, CODE_ADDR);
    let end = CODE_ADDR as u32 + code.len() as u32;
    for _ in 0..200 {
        if m.cpu.ip == end {
            break;
        }
        m.step();
    }
    assert_eq!(m.cpu.ip, end, "うちが最後まで進まない (ip={:#x})", m.cpu.ip);
    let mut d = [0u8; 16];
    for (i, x) in d.iter_mut().enumerate() {
        *x = m.read8(DATA_ADDR as u32 + i as u32);
    }
    let mut s = [0u8; STACK_WINDOW];
    for (i, x) in s.iter_mut().enumerate() {
        *x = m.read8(STACK_BASE as u32 + i as u32);
    }
    State {
        regs: std::array::from_fn(|i| m.cpu.regs[i] as u16),
        sregs: std::array::from_fn(|i| m.cpu.sregs[i]),
        flags: m.cpu.eflags() as u16 & FLAG_MASK_ALL,
        ip: m.cpu.ip as u16,
        data: d,
        stack: s,
    }
}

fn oracle(code: &[u8], data: &[u8; 16], stack: &[u8; STACK_WINDOW]) -> State {
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_16).unwrap();
    uc.mem_map(0, 0x100000, Prot::ALL).unwrap();
    uc.mem_write(CODE_ADDR as u64, code).unwrap();
    uc.mem_write(DATA_ADDR as u64, data).unwrap();
    uc.mem_write(STACK_BASE as u64, stack).unwrap();
    uc.reg_write(RegisterX86::BX, DATA_ADDR as u64).unwrap();
    uc.reg_write(RegisterX86::DI, STACK_BASE as u64).unwrap();
    uc.reg_write(RegisterX86::SP, STACK_INIT_SP as u64).unwrap();
    uc.reg_write(RegisterX86::EFLAGS, 0x0002).unwrap();
    let end = CODE_ADDR as u64 + code.len() as u64;
    uc.emu_start(CODE_ADDR as u64, end, 0, 0).unwrap();
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
    let mut d = [0u8; 16];
    uc.mem_read(DATA_ADDR as u64, &mut d).unwrap();
    let mut s = [0u8; STACK_WINDOW];
    uc.mem_read(STACK_BASE as u64, &mut s).unwrap();
    State {
        regs: std::array::from_fn(|i| uc.reg_read(regs[i]).unwrap() as u16),
        sregs: [0; 4],
        flags: uc.reg_read(RegisterX86::EFLAGS).unwrap() as u16 & FLAG_MASK_ALL,
        ip: uc.reg_read(RegisterX86::IP).unwrap() as u16,
        data: d,
        stack: s,
    }
}

fn doubles() -> Vec<(&'static str, f64)> {
    vec![
        ("1.5", 1.5),
        ("3.0", 3.0),
        ("0.8", 0.8),
        ("2.5", 2.5),
        ("-0.0", -0.0),
        ("0.0", 0.0),
        ("1e-310 (非正規)", 1e-310),
        ("1e300", 1e300),
        ("1234567.125", 1234567.125),
        ("-7.25", -7.25),
        ("NaN", f64::NAN),
        ("inf", f64::INFINITY),
        ("1e-5", 1e-5),
        ("123456789012345678", 123456789012345678.0),
    ]
}

/// data の先頭 8 バイトに v、残りは 0。stack は 0
fn with_double(v: f64) -> ([u8; 16], [u8; STACK_WINDOW]) {
    let mut d = [0u8; 16];
    d[..8].copy_from_slice(&v.to_le_bytes());
    (d, [0u8; STACK_WINDOW])
}

fn run(
    name: &str,
    code: &[u8],
    data: &[u8; 16],
    stack: &[u8; STACK_WINDOW],
    cmp_ax: bool,
    flag_mask: u16,
) -> Option<String> {
    run2(name, code, data, stack, cmp_ax, flag_mask, false)
}

/// m80 の仮数の下位 11bit (f64 に無い精度) を落としてから比べる — 台帳「64bit 精度は持っていない」
fn mask_lo11(s: &mut [u8; STACK_WINDOW]) {
    for off in [0usize, 10, 20] {
        s[off] = 0;
        s[off + 1] &= 0xF8;
    }
}

fn run2(
    name: &str,
    code: &[u8],
    data: &[u8; 16],
    stack: &[u8; STACK_WINDOW],
    cmp_ax: bool,
    flag_mask: u16,
    lo11: bool,
) -> Option<String> {
    let mut a = ours(code, data, stack);
    let mut b = oracle(code, data, stack);
    if lo11 {
        mask_lo11(&mut a.stack);
        mask_lo11(&mut b.stack);
    }
    let mut out = Vec::new();
    if a.data != b.data {
        out.push(format!(
            "data: ours={:02x?}\n        oracle={:02x?}",
            a.data, b.data
        ));
    }
    if a.stack != b.stack {
        out.push(format!(
            "stack: ours={:02x?}\n         oracle={:02x?}",
            a.stack, b.stack
        ));
    }
    if cmp_ax && a.regs[0] != b.regs[0] {
        out.push(format!(
            "AX: ours={:04x} oracle={:04x}",
            a.regs[0], b.regs[0]
        ));
    }
    if (a.flags & flag_mask) != (b.flags & flag_mask) {
        out.push(format!(
            "flags: ours={} oracle={}",
            flag_names(a.flags & flag_mask),
            flag_names(b.flags & flag_mask)
        ));
    }
    if out.is_empty() {
        None
    } else {
        Some(format!("[{name}]\n  {}", out.join("\n  ")))
    }
}

#[test]
fn x87_load_store_and_convert() {
    // ModRM: [bx]=07 / [di]=05 (16bit)。入力は data ([bx])、出力は stack の窓 ([di])
    let seqs: Vec<(&str, Vec<u8>)> = vec![
        // fld qword [bx]; fstp tword [di]  — glibc が見る 80bit の格納形式
        ("fld m64 → fstp m80", vec![0xDD, 0x07, 0xDB, 0x3D]),
        // fld qword [bx]; fstp dword [di]
        ("fld m64 → fstp m32", vec![0xDD, 0x07, 0xD9, 0x1D]),
        // fld qword [bx]; fstp qword [di]
        ("fld m64 → fstp m64", vec![0xDD, 0x07, 0xDD, 0x1D]),
        // fld qword [bx]; fistp dword [di]
        ("fld m64 → fistp m32", vec![0xDD, 0x07, 0xDB, 0x1D]),
        // fld qword [bx]; fistp qword [di]
        ("fld m64 → fistp m64", vec![0xDD, 0x07, 0xDF, 0x3D]),
        // fld qword [bx]; frndint; fstp qword [di]
        ("frndint", vec![0xDD, 0x07, 0xD9, 0xFC, 0xDD, 0x1D]),
        // fld qword [bx]; fxtract; fstp qword [di]; fstp qword [di+8]
        (
            "fxtract",
            vec![0xDD, 0x07, 0xD9, 0xF4, 0xDD, 0x1D, 0xDD, 0x5D, 0x08],
        ),
        // fld qword [bx]; fabs; fchs; fstp qword [di]
        (
            "fabs/fchs",
            vec![0xDD, 0x07, 0xD9, 0xE1, 0xD9, 0xE0, 0xDD, 0x1D],
        ),
        // fld qword [bx]; fsqrt; fstp qword [di]
        ("fsqrt", vec![0xDD, 0x07, 0xD9, 0xFA, 0xDD, 0x1D]),
        // fld1; fld qword [bx]; fscale; fstp qword [di]; fstp st0  (2^v の位取り)
        (
            "fscale",
            vec![0xD9, 0xE8, 0xDD, 0x07, 0xD9, 0xFD, 0xDD, 0x1D, 0xDD, 0xD8],
        ),
        // fld qword [bx]; fld1; fprem; fstp qword [di]; fstp st0
        (
            "fprem",
            vec![0xDD, 0x07, 0xD9, 0xE8, 0xD9, 0xF8, 0xDD, 0x1D, 0xDD, 0xD8],
        ),
        // fnstcw [di]
        ("fnstcw", vec![0xD9, 0x3D]),
        // fld qword [bx]; fld qword [bx+8]; fxsave [0x3000]; fninit; fxrstor [0x3000];
        // fxam; fnstsw ax; fstp qword [di]; fstp qword [di+8]  — TOP≠0 での往復
        (
            "fxsave/fxrstor",
            vec![
                0xDD, 0x07, 0xDD, 0x47, 0x08, 0x0F, 0xAE, 0x06, 0x00, 0x30, 0xDB, 0xE3, 0x0F, 0xAE,
                0x0E, 0x00, 0x30, 0xD9, 0xE5, 0xDF, 0xE0, 0xDD, 0x1D, 0xDD, 0x5D, 0x08,
            ],
        ),
        // (台帳) fnstenv の 16bit 版 (14 バイト) は未実装 — 32bit の 28 バイトだけ。DOS の古い数値ソフト向け
    ];
    let mut fails = Vec::new();
    for (name, code) in &seqs {
        for (dn, v) in doubles() {
            // (台帳) NaN/inf の FXTRACT・FPREM は Unicorn (QEMU) が特異な値を返す (1.0/inf 等) —
            // 実機の「不定値」と食い違う部分で、glibc/musl はどちらも踏まない。比べない
            if (name.starts_with("fxtract") || name.starts_with("fprem")) && !v.is_finite() {
                continue;
            }
            // (台帳) 非正規の FPREM も同じく Unicorn が -inf を返す (softfloat の癖)。答えは入力そのもの
            if name.starts_with("fprem") && v.is_subnormal() {
                continue;
            }
            // (台帳) Unicorn の版で答えが割れるもの: FSQRT(負) の NaN の符号 (実機は負の不定値、
            // CI の Unicorn は正)、|x|<1 の非整数の FPREM の下位 bit (実機は x そのもの、CI の
            // Unicorn は a - q*b の丸め)。手元 (arm64) と CI (x86_64) で違ったので比べない
            if name.starts_with("fsqrt") && v < 0.0 {
                continue;
            }
            if name.starts_with("fprem") && v.fract() != 0.0 && v.abs() < 1.0 {
                continue;
            }
            let (mut d, s) = with_double(v);
            d[8..16].copy_from_slice(&0.8f64.to_le_bytes());
            // 先頭に fninit: Unicorn の初期 FPU 状態は実機のリセットと違う (CW=0)
            let mut code2 = vec![0xDB, 0xE3];
            code2.extend_from_slice(code);
            let cmp_ax = name.starts_with("fxsave");
            if let Some(e) = run(&format!("{name} / {dn}"), &code2, &d, &s, cmp_ax, 0) {
                fails.push(e);
            }
        }
    }
    assert!(
        fails.is_empty(),
        "{} 件の不一致:\n{}",
        fails.len(),
        fails.join("\n")
    );
}

#[test]
fn x87_compare_and_classify() {
    let seqs: Vec<(&str, Vec<u8>, bool, u16)> = vec![
        // fld qword [bx]; fld qword [bx+8]; fucomip st0,st1; fstp st0 → flags
        (
            "fucomip",
            vec![0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDF, 0xE9, 0xDD, 0xD8],
            false,
            (cpu::ZF | cpu::PF | cpu::CF) as u16,
        ),
        // **直前に ALU 命令** (遅延フラグの材料が生きた状態) → fucomip の結果が残るか。
        // add ax,1 / sub ax,1 / and ax,0xff を先頭に
        (
            "add → fucomip",
            vec![
                0x05, 0x01, 0x00, 0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDF, 0xE9, 0xDD, 0xD8,
            ],
            false,
            (cpu::ZF | cpu::PF | cpu::CF) as u16,
        ),
        (
            "sub → fcomip",
            vec![
                0x2D, 0x01, 0x00, 0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDF, 0xF1, 0xDD, 0xD8,
            ],
            false,
            (cpu::ZF | cpu::PF | cpu::CF) as u16,
        ),
        (
            "and → fucomi",
            vec![
                0x25, 0xFF, 0x00, 0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDB, 0xE9, 0xDD, 0xD8, 0xDD, 0xD8,
            ],
            false,
            (cpu::ZF | cpu::PF | cpu::CF) as u16,
        ),
        (
            "fcomip",
            vec![0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDF, 0xF1, 0xDD, 0xD8],
            false,
            (cpu::ZF | cpu::PF | cpu::CF) as u16,
        ),
        // fld qword [bx]; fxam; fnstsw ax; fstp st0 → AX (C3C2C0, C1)
        (
            "fxam",
            vec![0xDD, 0x07, 0xD9, 0xE5, 0xDF, 0xE0, 0xDD, 0xD8],
            true,
            0,
        ),
        // fld qword [bx]; fld qword [bx+8]; fucomp st1; fnstsw ax; fstp st0 → AX
        (
            "fucomp+fnstsw",
            vec![
                0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDD, 0xE9, 0xDF, 0xE0, 0xDD, 0xD8,
            ],
            true,
            0,
        ),
        // fld qword [bx]; fld qword [bx+8]; fcomp st1; fnstsw ax; fstp st0 → AX
        (
            "fcomp+fnstsw",
            vec![
                0xDD, 0x07, 0xDD, 0x47, 0x08, 0xD8, 0xD9, 0xDF, 0xE0, 0xDD, 0xD8,
            ],
            true,
            0,
        ),
        // 空のスタックで fnstsw ax (初期状態の SW)
        ("fnstsw (初期)", vec![0xDF, 0xE0], true, 0),
        // fld qword [bx]; fld qword [bx+8]; fnstsw ax; fstp st0; fstp st0 (TOP の位置)
        (
            "fnstsw TOP",
            vec![
                0xDD, 0x07, 0xDD, 0x47, 0x08, 0xDF, 0xE0, 0xDD, 0xD8, 0xDD, 0xD8,
            ],
            true,
            0,
        ),
    ];
    let pairs: Vec<(f64, f64)> = {
        let ds = doubles();
        let mut v = Vec::new();
        for (_, a) in &ds {
            for (_, b) in ds.iter().take(6) {
                v.push((*a, *b));
            }
        }
        v
    };
    let mut fails = Vec::new();
    for (name, code, cmp_ax, mask) in &seqs {
        for (a, b) in &pairs {
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&a.to_le_bytes());
            d[8..].copy_from_slice(&b.to_le_bytes());
            let s = [0u8; STACK_WINDOW];
            let mut code2 = vec![0xDB, 0xE3];
            code2.extend_from_slice(code);
            if let Some(e) = run(
                &format!("{name} / {a} vs {b}"),
                &code2,
                &d,
                &s,
                *cmp_ax,
                *mask,
            ) {
                fails.push(e);
            }
        }
    }
    // 1 命令ごとの重複を畳む (同じ名前の最初の数件だけ)
    let mut seen = std::collections::HashMap::new();
    let shown: Vec<String> = fails
        .iter()
        .filter(|e| {
            let k = e.split(" / ").next().unwrap().to_string();
            let c = seen.entry(k).or_insert(0);
            *c += 1;
            *c <= 3
        })
        .cloned()
        .collect();
    assert!(
        fails.is_empty(),
        "{} 件の不一致 (抜粋):\n{}",
        fails.len(),
        shown.join("\n")
    );
}

#[test]
fn x87_control_word_precision() {
    // fldcw [bx+8] (PC=24/53/64) の後で fld qword [bx]; fmul st0,st0; fstp qword [di]
    let mut fails = Vec::new();
    for (pcn, cw) in [
        ("PC=24", 0x007Fu16),
        ("PC=53", 0x027F),
        ("PC=64", 0x037F),
        ("RC=down", 0x077F),
        ("RC=up", 0x0B7F),
        ("RC=zero", 0x0F7F),
    ] {
        for (dn, v) in doubles() {
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&v.to_le_bytes());
            d[8..10].copy_from_slice(&cw.to_le_bytes());
            let s = [0u8; STACK_WINDOW];
            // fninit; fldcw [bx+8]; fld qword [bx]; fmul st0,st0; fstp qword [di]; fnstcw [di+8]
            let code = vec![
                0xDB, 0xE3, 0xD9, 0x6F, 0x08, 0xDD, 0x07, 0xD8, 0xC8, 0xDD, 0x1D, 0xD9, 0x7D, 0x08,
            ];
            if let Some(e) = run(&format!("{pcn} fmul / {dn}"), &code, &d, &s, false, 0) {
                fails.push(e);
            }
            // 同じ CW で fld qword [bx]; fld qword [bx] (=v); fdivrp (v / v'?) は退屈なので
            // v / 3.0 と v + 0.8、frndint、fistp を見る
            let mut d2 = d;
            d2[8..16].copy_from_slice(&3.0f64.to_le_bytes());
            // fninit; fldcw [di+16] (CW は stack 窓の 16 に置く); fld qword [bx+8]; fld qword [bx]; fdivrp → st0 = v/3
            // fstp qword [di]; fld qword [bx]; frndint; fstp qword [di+8]
            let mut s2 = s;
            s2[16..18].copy_from_slice(&cw.to_le_bytes());
            let code = vec![
                0xDB, 0xE3, 0xD9, 0x6D, 0x10, 0xDD, 0x47, 0x08, 0xDD, 0x07, 0xDE, 0xF9, 0xDD, 0x1D,
                0xDD, 0x07, 0xD9, 0xFC, 0xDD, 0x5D, 0x08,
            ];
            if let Some(e) = run(
                &format!("{pcn} fdiv/frndint / {dn}"),
                &code,
                &d2,
                &s2,
                false,
                0,
            ) {
                fails.push(e);
            }
        }
    }
    assert!(
        fails.is_empty(),
        "{} 件の不一致:\n{}",
        fails.len(),
        fails.join("\n")
    );
}

/// 整数ロード (FILD) と定数・入れ替え — glibc の strtod は小さい整数を FILD で作る
#[test]
fn x87_integer_loads_and_constants() {
    let ints: Vec<i64> = vec![
        3,
        0,
        -7,
        1,
        65535,
        -32768,
        2147483647,
        -2147483648,
        123456789012,
        // (台帳) 2^53 を超える整数は f64 裏打ちで丸まる (i64::MAX → 2^63)。64bit 精度は持っていない
    ];
    let seqs: Vec<(&str, Vec<u8>)> = vec![
        // fninit; fild qword [bx]; fstp tword [di]
        ("fild m64 → m80", vec![0xDB, 0xE3, 0xDF, 0x2F, 0xDB, 0x3D]),
        // fninit; fild qword [bx]; fstp qword [di]
        ("fild m64 → m64", vec![0xDB, 0xE3, 0xDF, 0x2F, 0xDD, 0x1D]),
        // fninit; fild dword [bx]; fstp tword [di]
        ("fild m32 → m80", vec![0xDB, 0xE3, 0xDB, 0x07, 0xDB, 0x3D]),
        // fninit; fild word [bx]; fstp tword [di]
        ("fild m16 → m80", vec![0xDB, 0xE3, 0xDF, 0x07, 0xDB, 0x3D]),
        // fninit; fild qword [bx]; fist dword [di]; fistp qword [di+8]
        (
            "fild m64 → fist m32 / fistp m64",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xDB, 0x15, 0xDF, 0x7D, 0x08],
        ),
        // fninit; fild qword [bx]; fld st0; fxch st1; fstp qword [di]; fstp qword [di+8]
        (
            "fld st0 / fxch",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xC0, 0xD9, 0xC9, 0xDD, 0x1D, 0xDD, 0x5D, 0x08,
            ],
        ),
        // fninit; fild qword [bx]; fld st0; faddp (=2x); fstp tword [di]
        (
            "faddp",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xC0, 0xDE, 0xC1, 0xDB, 0x3D],
        ),
        // fninit; fild qword [bx]; fld1; fdivp (=1/x?) — DE F9 は FDIVP st1 = st1/st0 → x/1; fstp tword [di]
        (
            "fdivp",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDE, 0xF9, 0xDB, 0x3D],
        ),
        // fninit; fild qword [bx]; fld1; fdivrp (DE F1: st1 = st0/st1 = 1/x); fstp tword [di]
        (
            "fdivrp",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDE, 0xF1, 0xDB, 0x3D],
        ),
        // fninit; fild qword [bx]; fld1; fsubp (DE E9: st1 = st1 - st0 = x-1); fstp tword [di]
        (
            "fsubp",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDE, 0xE9, 0xDB, 0x3D],
        ),
        // fninit; fild qword [bx]; fld1; fsubrp (DE E1: st1 = st0 - st1 = 1-x); fstp tword [di]
        (
            "fsubrp",
            vec![0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDE, 0xE1, 0xDB, 0x3D],
        ),
        // fninit; fild qword [bx]; fld1; fsub st0,st1 (D8 E1: st0 = st0 - st1 = 1-x); fstp tword [di]; fstp st0
        (
            "fsub st0,st1",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xD8, 0xE1, 0xDB, 0x3D, 0xDD, 0xD8,
            ],
        ),
        // fninit; fild qword [bx]; fld1; fsubr st0,st1 (D8 E9: st0 = st1 - st0 = x-1)
        (
            "fsubr st0,st1",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xD8, 0xE9, 0xDB, 0x3D, 0xDD, 0xD8,
            ],
        ),
        // fninit; fild qword [bx]; fld1; fsub st1,st0 (DC E9: st1 = st1 - st0 = x-1); fstp st0; fstp tword [di]
        (
            "fsub st1,st0",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDC, 0xE9, 0xDD, 0xD8, 0xDB, 0x3D,
            ],
        ),
        // fninit; fild qword [bx]; fld1; fsubr st1,st0 (DC E1: st1 = st0 - st1 = 1-x)
        (
            "fsubr st1,st0",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDC, 0xE1, 0xDD, 0xD8, 0xDB, 0x3D,
            ],
        ),
        // fninit; fild qword [bx]; fld1; fdiv st1,st0 (DC F9: st1 = st1/st0 = x/1); fstp st0; fstp tword
        (
            "fdiv st1,st0",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDC, 0xF9, 0xDD, 0xD8, 0xDB, 0x3D,
            ],
        ),
        // fninit; fild qword [bx]; fld1; fdivr st1,st0 (DC F1: st1 = st0/st1 = 1/x)
        (
            "fdivr st1,st0",
            vec![
                0xDB, 0xE3, 0xDF, 0x2F, 0xD9, 0xE8, 0xDC, 0xF1, 0xDD, 0xD8, 0xDB, 0x3D,
            ],
        ),
        // (台帳) fldpi/fldl2e/fldlg2 等の定数は実機が 64bit 仮数で持つ。f64 の丸めと
        // 実機の 64bit を 53bit に切った値が 1ulp ずれることがあるので比べない
    ];
    let mut fails = Vec::new();
    for (name, code) in &seqs {
        for v in &ints {
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&v.to_le_bytes());
            let s = [0u8; STACK_WINDOW];
            if let Some(e) = run2(&format!("{name} / {v}"), code, &d, &s, false, 0, true) {
                fails.push(e);
            }
        }
    }
    let mut seen = std::collections::HashMap::new();
    let shown: Vec<String> = fails
        .iter()
        .filter(|e| {
            let k = e.split(" / ").next().unwrap().to_string();
            let c = seen.entry(k).or_insert(0);
            *c += 1;
            *c <= 2
        })
        .cloned()
        .collect();
    assert!(
        fails.is_empty(),
        "{} 件の不一致 (抜粋):\n{}",
        fails.len(),
        shown.join("\n")
    );
}
