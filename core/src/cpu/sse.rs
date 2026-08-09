//! SSE / SSE2 — 128bitのXMMレジスタと、その上の演算。
//!
//! ## なぜ要るのか
//!
//! Pentium III (1999) のSSE、Pentium 4 (2000) のSSE2で入った拡張だが、
//! 現代の主用途は浮動小数点ではなく**メモリ操作**である。gccは memcpy も
//! strlen も XMM で16バイトずつ束ねて回すコードを吐き、Alpineのユーザーランド
//! (i686ビルド) はCPUIDを見ずにこれを使う。つまり**現代のLinuxユーザーランドを
//! 走らせる = SSEを持つ**ことと同義になっている。
//!
//! ## デコードの仕掛け: 「同じオペコード、プレフィクスで別命令」
//!
//! SSEはオペコード空間を増やさず、**既存のプレフィクスを味変に使った**:
//!
//! ```text
//!   0F 10        movups   (プレフィクスなし = packed single)
//!   66 0F 10     movupd   (66 = packed double)
//!   F3 0F 10     movss    (F3 = scalar single)
//!   F2 0F 10     movsd    (F2 = scalar double)
//! ```
//!
//! 66 は「オペランドサイズ」、F2/F3 は「REP」だったものが、0F の直後では
//! 命令選択子に化ける。デコーダの [`Decoder`] が既に両方を記録しているので、
//! ここではそれを読み替えるだけでよい。
//!
//! ## 実装の割り切り
//!
//! - アラインメント検査 (movapsの16バイト境界#GP) はしない — 検査で得るものが無い
//! - 浮動小数点はホストの f32/f64 で計算する (丸めモードはMXCSRを保持するだけ)。
//!   カーネル起動とbusyboxに必要なのは値の器としてのXMMで、FPの厳密さではない
//! - 例外 (#XM) は起こさない

use super::operand::{fetch8, modrm, Operand};
use super::{Decoder, AX, CF, PF, ZF};
use crate::Machine;

/// プレフィクスの読み替え結果
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pfx {
    /// なし: packed single (ps)
    None,
    /// 66: packed double (pd) / 整数128bit
    P66,
    /// F3: scalar single (ss)
    F3,
    /// F2: scalar double (sd)
    F2,
}

pub(crate) fn pfx(d: &Decoder) -> Pfx {
    // 32bitコードで 66 が付くと opsize32 が反転して false になっている
    match d.rep {
        Some(0xF3) => Pfx::F3,
        Some(0xF2) => Pfx::F2,
        _ if !d.opsize32 => Pfx::P66,
        _ => Pfx::None,
    }
}

// ---- XMMとメモリの出し入れ ----

fn read_m128(m: &Machine, a: u32) -> u128 {
    m.read32(a) as u128
        | (m.read32(a.wrapping_add(4)) as u128) << 32
        | (m.read32(a.wrapping_add(8)) as u128) << 64
        | (m.read32(a.wrapping_add(12)) as u128) << 96
}

fn write_m128(m: &mut Machine, a: u32, v: u128) {
    m.write32(a, v as u32);
    m.write32(a.wrapping_add(4), (v >> 32) as u32);
    m.write32(a.wrapping_add(8), (v >> 64) as u32);
    m.write32(a.wrapping_add(12), (v >> 96) as u32);
}

fn read_m64(m: &Machine, a: u32) -> u64 {
    m.read32(a) as u64 | (m.read32(a.wrapping_add(4)) as u64) << 32
}

fn write_m64(m: &mut Machine, a: u32, v: u64) {
    m.write32(a, v as u32);
    m.write32(a.wrapping_add(4), (v >> 32) as u32);
}

/// r/m の128bit読み (レジスタかメモリか)
fn rm128(m: &Machine, rm: &Operand) -> u128 {
    match rm {
        Operand::Reg(r) => m.cpu.xmm[*r],
        Operand::Mem { addr, .. } => read_m128(m, *addr),
    }
}

fn rm64(m: &Machine, rm: &Operand) -> u64 {
    match rm {
        Operand::Reg(r) => m.cpu.xmm[*r] as u64,
        Operand::Mem { addr, .. } => read_m64(m, *addr),
    }
}

fn rm32(m: &Machine, rm: &Operand) -> u32 {
    match rm {
        Operand::Reg(r) => m.cpu.xmm[*r] as u32,
        Operand::Mem { addr, .. } => m.read32(*addr),
    }
}

// ---- レーン分解 (128bit ⇄ 要素の配列) ----

fn to8(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}
fn from8(a: [u8; 16]) -> u128 {
    u128::from_le_bytes(a)
}
fn to16(v: u128) -> [u16; 8] {
    core::array::from_fn(|i| (v >> (i * 16)) as u16)
}
fn from16(a: [u16; 8]) -> u128 {
    a.iter()
        .enumerate()
        .fold(0u128, |acc, (i, &x)| acc | (x as u128) << (i * 16))
}
fn to32(v: u128) -> [u32; 4] {
    core::array::from_fn(|i| (v >> (i * 32)) as u32)
}
fn from32(a: [u32; 4]) -> u128 {
    a.iter()
        .enumerate()
        .fold(0u128, |acc, (i, &x)| acc | (x as u128) << (i * 32))
}
fn to64(v: u128) -> [u64; 2] {
    [v as u64, (v >> 64) as u64]
}
fn from64(a: [u64; 2]) -> u128 {
    a[0] as u128 | (a[1] as u128) << 64
}

fn tof32(v: u128) -> [f32; 4] {
    to32(v).map(f32::from_bits)
}
fn fromf32(a: [f32; 4]) -> u128 {
    from32(a.map(f32::to_bits))
}
fn tof64(v: u128) -> [f64; 2] {
    to64(v).map(f64::from_bits)
}
fn fromf64(a: [f64; 2]) -> u128 {
    from64(a.map(f64::to_bits))
}

/// 浮動小数点の2項演算をプレフィクスに従って適用する。
/// packed は全レーン、scalar は最下位レーンだけ (残りはdst据え置き)
fn fp_binop(
    m: &mut Machine,
    d: &Decoder,
    f32op: impl Fn(f32, f32) -> f32,
    f64op: impl Fn(f64, f64) -> f64,
) {
    let p = pfx(d);
    let (reg, rm) = modrm(m, d);
    let dst = m.cpu.xmm[reg];
    let v = match p {
        Pfx::None => {
            let (a, b) = (tof32(dst), tof32(rm128(m, &rm)));
            fromf32(core::array::from_fn(|i| f32op(a[i], b[i])))
        }
        Pfx::P66 => {
            let (a, b) = (tof64(dst), tof64(rm128(m, &rm)));
            fromf64(core::array::from_fn(|i| f64op(a[i], b[i])))
        }
        Pfx::F3 => {
            let a = f32::from_bits(dst as u32);
            let b = f32::from_bits(rm32(m, &rm));
            (dst & !0xFFFF_FFFF) | f32op(a, b).to_bits() as u128
        }
        Pfx::F2 => {
            let a = f64::from_bits(dst as u64);
            let b = f64::from_bits(rm64(m, &rm));
            (dst & !0xFFFF_FFFF_FFFF_FFFF) | f64op(a, b).to_bits() as u128
        }
    };
    m.cpu.xmm[reg] = v;
}

/// 整数のレーン別2項演算 (8/16/32/64bitはクロージャ側で選ぶ)
fn int_binop(m: &mut Machine, d: &Decoder, f: impl Fn(u128, u128) -> u128) {
    let (reg, rm) = modrm(m, d);
    let b = rm128(m, &rm);
    m.cpu.xmm[reg] = f(m.cpu.xmm[reg], b);
}

fn lanes8(f: impl Fn(u8, u8) -> u8) -> impl Fn(u128, u128) -> u128 {
    move |a, b| {
        let (x, y) = (to8(a), to8(b));
        from8(core::array::from_fn(|i| f(x[i], y[i])))
    }
}
fn lanes16(f: impl Fn(u16, u16) -> u16) -> impl Fn(u128, u128) -> u128 {
    move |a, b| {
        let (x, y) = (to16(a), to16(b));
        from16(core::array::from_fn(|i| f(x[i], y[i])))
    }
}
fn lanes32(f: impl Fn(u32, u32) -> u32) -> impl Fn(u128, u128) -> u128 {
    move |a, b| {
        let (x, y) = (to32(a), to32(b));
        from32(core::array::from_fn(|i| f(x[i], y[i])))
    }
}
fn lanes64(f: impl Fn(u64, u64) -> u64) -> impl Fn(u128, u128) -> u128 {
    move |a, b| {
        let (x, y) = (to64(a), to64(b));
        from64([f(x[0], y[0]), f(x[1], y[1])])
    }
}

/// COMISS/UCOMISS/COMISD: 比較結果をEFLAGSへ (ZF/PF/CF、他は0)
fn comis(m: &mut Machine, a: f64, b: f64) {
    let (zf, pf, cf) = if a.is_nan() || b.is_nan() {
        (true, true, true) // unordered
    } else if a > b {
        (false, false, false)
    } else if a < b {
        (false, false, true)
    } else {
        (true, false, false)
    };
    m.cpu.set_flag(ZF, zf);
    m.cpu.set_flag(PF, pf);
    m.cpu.set_flag(CF, cf);
    m.cpu.set_flag(super::SF, false);
    m.cpu.set_flag(super::OF, false);
    m.cpu.set_flag(super::AF, false);
}

/// `0F op2` のSSE命令を実行する。扱わないオペコードなら false (呼び出し側がtrapする)
pub(crate) fn step_sse(m: &mut Machine, d: &Decoder, op2: u8) -> bool {
    let p = pfx(d);
    match op2 {
        // ---- データ移動 ----
        // movups/movupd/movss/movsd: 読み (10) と書き (11)
        0x10 => {
            let (reg, rm) = modrm(m, d);
            m.cpu.xmm[reg] = match p {
                Pfx::None | Pfx::P66 => rm128(m, &rm),
                Pfx::F3 => match rm {
                    // メモリからのmovssは上位96bitをゼロに、レジスタ間は下位だけ
                    Operand::Mem { addr, .. } => m.read32(addr) as u128,
                    Operand::Reg(r) => {
                        (m.cpu.xmm[reg] & !0xFFFF_FFFF) | (m.cpu.xmm[r] & 0xFFFF_FFFF)
                    }
                },
                Pfx::F2 => match rm {
                    Operand::Mem { addr, .. } => read_m64(m, addr) as u128,
                    Operand::Reg(r) => {
                        (m.cpu.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | (m.cpu.xmm[r] as u64 as u128)
                    }
                },
            };
        }
        0x11 => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.xmm[reg];
            match (p, rm) {
                (Pfx::None | Pfx::P66, Operand::Mem { addr, .. }) => write_m128(m, addr, v),
                (Pfx::None | Pfx::P66, Operand::Reg(r)) => m.cpu.xmm[r] = v,
                (Pfx::F3, Operand::Mem { addr, .. }) => m.write32(addr, v as u32),
                (Pfx::F3, Operand::Reg(r)) => {
                    m.cpu.xmm[r] = (m.cpu.xmm[r] & !0xFFFF_FFFF) | (v & 0xFFFF_FFFF)
                }
                (Pfx::F2, Operand::Mem { addr, .. }) => write_m64(m, addr, v as u64),
                (Pfx::F2, Operand::Reg(r)) => {
                    m.cpu.xmm[r] = (m.cpu.xmm[r] & !0xFFFF_FFFF_FFFF_FFFF) | (v as u64 as u128)
                }
            }
        }
        // movlps/movlpd (12/13): 下位64bitの出し入れ。movhps/movhpd (16/17): 上位64bit
        // F2 0F 12 = movddup / F3 0F 12 = movsldup はSSE3なので来たらtrapに落とす
        0x12 if matches!(p, Pfx::None | Pfx::P66) => {
            let (reg, rm) = modrm(m, d);
            let v = rm64(m, &rm);
            m.cpu.xmm[reg] = (m.cpu.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | v as u128;
        }
        0x13 if matches!(p, Pfx::None | Pfx::P66) => {
            let (reg, rm) = modrm(m, d);
            if let Operand::Mem { addr, .. } = rm {
                let v = m.cpu.xmm[reg] as u64;
                write_m64(m, addr, v);
            }
        }
        0x16 if matches!(p, Pfx::None | Pfx::P66) => {
            let (reg, rm) = modrm(m, d);
            let v = rm64(m, &rm);
            m.cpu.xmm[reg] = (m.cpu.xmm[reg] & 0xFFFF_FFFF_FFFF_FFFF) | (v as u128) << 64;
        }
        0x17 if matches!(p, Pfx::None | Pfx::P66) => {
            let (reg, rm) = modrm(m, d);
            if let Operand::Mem { addr, .. } = rm {
                let v = (m.cpu.xmm[reg] >> 64) as u64;
                write_m64(m, addr, v);
            }
        }
        // movaps/movapd (28/29)。アラインメント検査はしない (冒頭のdoc参照)
        0x28 => {
            let (reg, rm) = modrm(m, d);
            m.cpu.xmm[reg] = rm128(m, &rm);
        }
        0x29 => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.xmm[reg];
            match rm {
                Operand::Mem { addr, .. } => write_m128(m, addr, v),
                Operand::Reg(r) => m.cpu.xmm[r] = v,
            }
        }
        // movntps/movntdq (2B/E7): 「キャッシュを汚さない書き込み」。
        // キャッシュが無いので、ただの書き込み
        0x2B | 0xE7 => {
            let (reg, rm) = modrm(m, d);
            if let Operand::Mem { addr, .. } = rm {
                let v = m.cpu.xmm[reg];
                write_m128(m, addr, v);
            }
        }
        // movd/movq (6E): r/m32 → xmm (ゼロ拡張)
        0x6E if p == Pfx::P66 => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.regs[r],
                Operand::Mem { addr, .. } => m.read32(addr),
            };
            m.cpu.xmm[reg] = v as u128;
        }
        // movdqa/movdqu (6F/7F)
        0x6F if matches!(p, Pfx::P66 | Pfx::F3) => {
            let (reg, rm) = modrm(m, d);
            m.cpu.xmm[reg] = rm128(m, &rm);
        }
        0x7F if matches!(p, Pfx::P66 | Pfx::F3) => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.xmm[reg];
            match rm {
                Operand::Mem { addr, .. } => write_m128(m, addr, v),
                Operand::Reg(r) => m.cpu.xmm[r] = v,
            }
        }
        // 66 0F 7E: movd r/m32 ← xmm。F3 0F 7E: movq xmm ← xmm/m64 (上位ゼロ)
        0x7E if p == Pfx::P66 => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.xmm[reg] as u32;
            match rm {
                Operand::Reg(r) => m.cpu.regs[r] = v,
                Operand::Mem { addr, .. } => m.write32(addr, v),
            }
        }
        0x7E if p == Pfx::F3 => {
            let (reg, rm) = modrm(m, d);
            m.cpu.xmm[reg] = rm64(m, &rm) as u128;
        }
        // 66 0F D6: movq xmm/m64 ← xmm下位
        0xD6 if p == Pfx::P66 => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.xmm[reg] as u64;
            match rm {
                Operand::Mem { addr, .. } => write_m64(m, addr, v),
                Operand::Reg(r) => m.cpu.xmm[r] = v as u128,
            }
        }

        // ---- ビット演算 (ps/pd/整数で同じ動き) ----
        0x54 => int_binop(m, d, |a, b| a & b), // andps/pand相当
        0x55 => int_binop(m, d, |a, b| !a & b), // andnps
        0x56 => int_binop(m, d, |a, b| a | b), // orps
        0x57 => int_binop(m, d, |a, b| a ^ b), // xorps
        0xDB => int_binop(m, d, |a, b| a & b), // pand
        0xDF => int_binop(m, d, |a, b| !a & b), // pandn
        0xEB => int_binop(m, d, |a, b| a | b), // por
        0xEF => int_binop(m, d, |a, b| a ^ b), // pxor

        // ---- 整数の加減算 ----
        0xFC => int_binop(m, d, lanes8(u8::wrapping_add)),
        0xFD => int_binop(m, d, lanes16(u16::wrapping_add)),
        0xFE => int_binop(m, d, lanes32(u32::wrapping_add)),
        0xD4 => int_binop(m, d, lanes64(u64::wrapping_add)),
        0xF8 => int_binop(m, d, lanes8(u8::wrapping_sub)),
        0xF9 => int_binop(m, d, lanes16(u16::wrapping_sub)),
        0xFA => int_binop(m, d, lanes32(u32::wrapping_sub)),
        0xFB => int_binop(m, d, lanes64(u64::wrapping_sub)),
        // 飽和演算 (パディングや画像で使う)
        0xDC => int_binop(m, d, lanes8(u8::saturating_add)),
        0xD8 => int_binop(m, d, lanes8(u8::saturating_sub)),
        0xDA => int_binop(m, d, lanes8(u8::min)), // pminub
        0xDE => int_binop(m, d, lanes8(u8::max)), // pmaxub

        // ---- 比較 (等しいレーンが全1になる) — strlen/memchrの心臓部 ----
        0x74 => int_binop(m, d, lanes8(|a, b| if a == b { 0xFF } else { 0 })),
        0x75 => int_binop(m, d, lanes16(|a, b| if a == b { 0xFFFF } else { 0 })),
        0x76 => int_binop(m, d, lanes32(|a, b| if a == b { 0xFFFF_FFFF } else { 0 })),
        0x64 => int_binop(
            m,
            d,
            lanes8(|a, b| if (a as i8) > b as i8 { 0xFF } else { 0 }),
        ),
        0x65 => int_binop(
            m,
            d,
            lanes16(|a, b| if (a as i16) > b as i16 { 0xFFFF } else { 0 }),
        ),
        0x66 => int_binop(
            m,
            d,
            lanes32(|a, b| {
                if (a as i32) > b as i32 {
                    0xFFFF_FFFF
                } else {
                    0
                }
            }),
        ),
        // pmovmskb: 各バイトの符号ビットを16bitに束ねて汎用レジスタへ。
        // 「どのレーンが一致したか」を分岐で使える形にする、比較の相棒
        0xD7 if p == Pfx::P66 => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.xmm[r],
                Operand::Mem { .. } => return false, // レジスタ専用
            };
            let mask = to8(v)
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | ((b >> 7) as u32) << i);
            m.cpu.regs[reg] = mask;
        }
        // movmskps/movmskpd (50)
        0x50 => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.xmm[r],
                Operand::Mem { .. } => return false,
            };
            m.cpu.regs[reg] = if p == Pfx::P66 {
                (((v >> 63) & 1) | ((v >> 126) & 2)) as u32
            } else {
                to32(v)
                    .iter()
                    .enumerate()
                    .fold(0u32, |acc, (i, &x)| acc | (x >> 31) << i)
            };
        }

        // ---- 並べ替え ----
        // punpckl/h: 2本のレジスタのレーンを互い違いに混ぜる
        0x60 => int_binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            from8(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[i / 2]
                } else {
                    y[i / 2]
                }
            }))
        }),
        0x61 => int_binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            from16(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[i / 2]
                } else {
                    y[i / 2]
                }
            }))
        }),
        0x62 => int_binop(m, d, |a, b| {
            let (x, y) = (to32(a), to32(b));
            from32([x[0], y[0], x[1], y[1]])
        }),
        0x6C => int_binop(m, d, |a, b| from64([to64(a)[0], to64(b)[0]])),
        0x68 => int_binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            from8(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[8 + i / 2]
                } else {
                    y[8 + i / 2]
                }
            }))
        }),
        0x69 => int_binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            from16(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[4 + i / 2]
                } else {
                    y[4 + i / 2]
                }
            }))
        }),
        0x6A => int_binop(m, d, |a, b| {
            let (x, y) = (to32(a), to32(b));
            from32([x[2], y[2], x[3], y[3]])
        }),
        0x6D => int_binop(m, d, |a, b| from64([to64(a)[1], to64(b)[1]])),
        // pshufd/pshuflw/pshufhw (70 + imm8)
        0x70 => {
            let (reg, rm) = modrm(m, d);
            let src = rm128(m, &rm);
            let imm = fetch8(m) as usize;
            m.cpu.xmm[reg] = match p {
                Pfx::P66 => {
                    let s = to32(src);
                    from32(core::array::from_fn(|i| s[(imm >> (i * 2)) & 3]))
                }
                Pfx::F2 => {
                    // pshuflw: 下位4ワードを並べ替え、上位64bitは素通し
                    let s = to16(src);
                    let mut o = s;
                    for (i, slot) in o.iter_mut().take(4).enumerate() {
                        *slot = s[(imm >> (i * 2)) & 3];
                    }
                    from16(o)
                }
                Pfx::F3 => {
                    let s = to16(src);
                    let mut o = s;
                    for i in 0..4 {
                        o[4 + i] = s[4 + ((imm >> (i * 2)) & 3)];
                    }
                    from16(o)
                }
                Pfx::None => return false, // pshufw はMMX (未対応)
            };
        }
        // shufps (C6 + imm8)
        0xC6 if p == Pfx::None => {
            let (reg, rm) = modrm(m, d);
            let b = rm128(m, &rm);
            let imm = fetch8(m) as usize;
            let (x, y) = (to32(m.cpu.xmm[reg]), to32(b));
            m.cpu.xmm[reg] = from32([
                x[imm & 3],
                x[(imm >> 2) & 3],
                y[(imm >> 4) & 3],
                y[(imm >> 6) & 3],
            ]);
        }

        // ---- シフト ----
        // 即値形 (71/72/73 はModRMのreg欄が演算選択)
        0x71..=0x73 if p == Pfx::P66 => {
            let (kind, rm) = modrm(m, d);
            let n = fetch8(m) as u32;
            let r = match rm {
                Operand::Reg(r) => r,
                Operand::Mem { .. } => return false,
            };
            let v = m.cpu.xmm[r];
            m.cpu.xmm[r] = match (op2, kind) {
                (0x71, 2) => from16(to16(v).map(|x| if n < 16 { x >> n } else { 0 })),
                (0x71, 4) => from16(to16(v).map(|x| ((x as i16) >> n.min(15)) as u16)),
                (0x71, 6) => from16(to16(v).map(|x| if n < 16 { x << n } else { 0 })),
                (0x72, 2) => from32(to32(v).map(|x| if n < 32 { x >> n } else { 0 })),
                (0x72, 4) => from32(to32(v).map(|x| ((x as i32) >> n.min(31)) as u32)),
                (0x72, 6) => from32(to32(v).map(|x| if n < 32 { x << n } else { 0 })),
                (0x73, 2) => from64(to64(v).map(|x| if n < 64 { x >> n } else { 0 })),
                (0x73, 6) => from64(to64(v).map(|x| if n < 64 { x << n } else { 0 })),
                (0x73, 3) => {
                    // psrldq: バイト単位の右シフト (128bit全体)
                    if n >= 16 {
                        0
                    } else {
                        v >> (n * 8)
                    }
                }
                (0x73, 7) => {
                    // pslldq
                    if n >= 16 {
                        0
                    } else {
                        v << (n * 8)
                    }
                }
                _ => return false,
            };
        }
        // レジスタ (下位64bitがカウント) 形
        0xD1 => int_binop(m, d, |a, b| {
            let n = (b as u64).min(63) as u32;
            if b as u64 >= 16 {
                0
            } else {
                from16(to16(a).map(|x| x >> n))
            }
        }),
        0xD2 => int_binop(m, d, |a, b| {
            if b as u64 >= 32 {
                0
            } else {
                from32(to32(a).map(|x| x >> (b as u32)))
            }
        }),
        0xD3 => int_binop(m, d, |a, b| {
            if b as u64 >= 64 {
                0
            } else {
                from64(to64(a).map(|x| x >> (b as u32)))
            }
        }),
        0xF1 => int_binop(m, d, |a, b| {
            if b as u64 >= 16 {
                0
            } else {
                from16(to16(a).map(|x| x << (b as u32)))
            }
        }),
        0xF2 => int_binop(m, d, |a, b| {
            if b as u64 >= 32 {
                0
            } else {
                from32(to32(a).map(|x| x << (b as u32)))
            }
        }),
        0xF3 => int_binop(m, d, |a, b| {
            if b as u64 >= 64 {
                0
            } else {
                from64(to64(a).map(|x| x << (b as u32)))
            }
        }),

        // psadbw: バイト差の絶対値の和。memchr系の最適化で出てくる
        0xF6 if p == Pfx::P66 => int_binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            let lo: u64 = (0..8)
                .map(|i| (x[i] as i16 - y[i] as i16).unsigned_abs() as u64)
                .sum();
            let hi: u64 = (8..16)
                .map(|i| (x[i] as i16 - y[i] as i16).unsigned_abs() as u64)
                .sum();
            from64([lo, hi])
        }),
        // pack系: 幅を半分に潰して2本を1本に (飽和つき)
        0x63 => int_binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            let sat = |v: u16| (v as i16).clamp(-128, 127) as i8 as u8;
            from8(core::array::from_fn(|i| {
                if i < 8 {
                    sat(x[i])
                } else {
                    sat(y[i - 8])
                }
            }))
        }),
        0x67 => int_binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            let sat = |v: u16| (v as i16).clamp(0, 255) as u8;
            from8(core::array::from_fn(|i| {
                if i < 8 {
                    sat(x[i])
                } else {
                    sat(y[i - 8])
                }
            }))
        }),
        0x6B => int_binop(m, d, |a, b| {
            let (x, y) = (to32(a), to32(b));
            let sat = |v: u32| (v as i32).clamp(-32768, 32767) as i16 as u16;
            from16(core::array::from_fn(|i| {
                if i < 4 {
                    sat(x[i])
                } else {
                    sat(y[i - 4])
                }
            }))
        }),

        // ---- 浮動小数点 ----
        0x51 => fp_binop(m, d, |_, b| b.sqrt(), |_, b| b.sqrt()),
        0x58 => fp_binop(m, d, |a, b| a + b, |a, b| a + b),
        0x59 => fp_binop(m, d, |a, b| a * b, |a, b| a * b),
        0x5C => fp_binop(m, d, |a, b| a - b, |a, b| a - b),
        0x5D => fp_binop(m, d, f32::min, f64::min),
        0x5E => fp_binop(m, d, |a, b| a / b, |a, b| a / b),
        0x5F => fp_binop(m, d, f32::max, f64::max),
        // 変換: 整数 → 浮動 (2A)、浮動 → 整数 (2C=切り捨て, 2D=丸め)
        0x2A => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.regs[r] as i32,
                Operand::Mem { addr, .. } => m.read32(addr) as i32,
            };
            match p {
                Pfx::F3 => {
                    m.cpu.xmm[reg] = (m.cpu.xmm[reg] & !0xFFFF_FFFF) | (v as f32).to_bits() as u128
                }
                Pfx::F2 => {
                    m.cpu.xmm[reg] =
                        (m.cpu.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | (v as f64).to_bits() as u128
                }
                _ => return false, // cvtpi2ps はMMX絡み (未対応)
            }
        }
        0x2C | 0x2D => {
            let (reg, rm) = modrm(m, d);
            let val = match p {
                Pfx::F3 => f32::from_bits(rm32(m, &rm)) as f64,
                Pfx::F2 => f64::from_bits(rm64(m, &rm)),
                _ => return false,
            };
            // 2C (cvtt〜) は0方向へ切り捨て。2D は丸め — どちらもtruncで代用
            // (MXCSRの丸めモードは既定の最近接だが、libcの主用途はtrunc)
            let out = if val.is_nan() {
                i32::MIN
            } else {
                val.trunc().clamp(i32::MIN as f64, i32::MAX as f64) as i32
            };
            m.cpu.regs[reg] = out as u32;
        }
        // cvtss2sd / cvtsd2ss / cvtps2pd / cvtpd2ps (5A)
        0x5A => {
            let (reg, rm) = modrm(m, d);
            match p {
                Pfx::F3 => {
                    let v = f32::from_bits(rm32(m, &rm)) as f64;
                    m.cpu.xmm[reg] =
                        (m.cpu.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | v.to_bits() as u128;
                }
                Pfx::F2 => {
                    let v = f64::from_bits(rm64(m, &rm)) as f32;
                    m.cpu.xmm[reg] = (m.cpu.xmm[reg] & !0xFFFF_FFFF) | v.to_bits() as u128;
                }
                Pfx::None => {
                    let s = tof32(rm128(m, &rm));
                    m.cpu.xmm[reg] = fromf64([s[0] as f64, s[1] as f64]);
                }
                Pfx::P66 => {
                    let s = tof64(rm128(m, &rm));
                    m.cpu.xmm[reg] =
                        fromf32([s[0] as f32, s[1] as f32, 0.0, 0.0]) & 0xFFFF_FFFF_FFFF_FFFF;
                }
            }
        }
        // cvtdq2ps (5B) / cvtps2dq (66 5B)
        0x5B => {
            let (reg, rm) = modrm(m, d);
            match p {
                Pfx::None => {
                    let s = to32(rm128(m, &rm));
                    m.cpu.xmm[reg] = fromf32(s.map(|x| x as i32 as f32));
                }
                Pfx::P66 | Pfx::F3 => {
                    let s = tof32(rm128(m, &rm));
                    m.cpu.xmm[reg] = from32(s.map(|x| {
                        if x.is_nan() {
                            i32::MIN as u32
                        } else {
                            x.trunc() as i32 as u32
                        }
                    }));
                }
                _ => return false,
            }
        }
        // ucomiss/comiss (2E/2F)。66ならsd
        0x2E | 0x2F => {
            let (reg, rm) = modrm(m, d);
            let (a, b) = if p == Pfx::P66 {
                (
                    f64::from_bits(m.cpu.xmm[reg] as u64),
                    f64::from_bits(rm64(m, &rm)),
                )
            } else {
                (
                    f32::from_bits(m.cpu.xmm[reg] as u32) as f64,
                    f32::from_bits(rm32(m, &rm)) as f64,
                )
            };
            comis(m, a, b);
        }
        // cmpps/cmpss等 (C2 + imm8): 比較して全1/全0を書く
        0xC2 => {
            let (reg, rm) = modrm(m, d);
            let b128 = rm128(m, &rm);
            let b32 = rm32(m, &rm);
            let b64 = rm64(m, &rm);
            let imm = fetch8(m) & 7;
            // NLT (5) / NLE (6) は**否定で定義される** — !(a<b) は a>=b と
            // NaNの扱いが違い、それがSSEの述語の仕様そのもの
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let cmp64 = |a: f64, b: f64| -> bool {
                match imm {
                    0 => a == b,
                    1 => a < b,
                    2 => a <= b,
                    3 => a.is_nan() || b.is_nan(),
                    4 => a != b,
                    5 => !(a < b),
                    6 => !(a <= b),
                    _ => !(a.is_nan() || b.is_nan()),
                }
            };
            let dst = m.cpu.xmm[reg];
            m.cpu.xmm[reg] = match p {
                Pfx::None => {
                    let (x, y) = (tof32(dst), tof32(b128));
                    from32(core::array::from_fn(|i| {
                        if cmp64(x[i] as f64, y[i] as f64) {
                            !0u32
                        } else {
                            0
                        }
                    }))
                }
                Pfx::P66 => {
                    let (x, y) = (tof64(dst), tof64(b128));
                    from64([
                        if cmp64(x[0], y[0]) { !0u64 } else { 0 },
                        if cmp64(x[1], y[1]) { !0u64 } else { 0 },
                    ])
                }
                Pfx::F3 => {
                    let ok = cmp64(
                        f32::from_bits(dst as u32) as f64,
                        f32::from_bits(b32) as f64,
                    );
                    (dst & !0xFFFF_FFFF) | if ok { 0xFFFF_FFFF } else { 0 }
                }
                Pfx::F2 => {
                    let ok = cmp64(f64::from_bits(dst as u64), f64::from_bits(b64));
                    (dst & !0xFFFF_FFFF_FFFF_FFFF) | if ok { 0xFFFF_FFFF_FFFF_FFFF } else { 0 }
                }
            };
        }

        _ => return false,
    }
    true
}

/// 0F AE グループ: FXSAVE/FXRSTOR/LDMXCSR/STMXCSR。
///
/// FXSAVE/FXRSTOR は**カーネルがコンテキストスイッチでXMMを退避する**ための命令。
/// CPUIDでFXSRを名乗った以上、これが動かないとプロセス間でXMMが混線する。
/// 512バイトの決められた配置 (FCW@0, MXCSR@24, ST@32.., XMM@160..) に書く
pub(crate) fn grp_0fae(m: &mut Machine, d: &Decoder) -> bool {
    let (kind, rm) = modrm(m, d);
    match (kind, rm) {
        // FXSAVE
        (0, Operand::Mem { addr, .. }) => {
            for i in 0..512u32 {
                m.write8(addr.wrapping_add(i), 0);
            }
            let cw = m.cpu.fpu_cw;
            m.write16(addr, cw);
            m.write16(addr.wrapping_add(2), 0); // FSW
            m.write8(addr.wrapping_add(4), 0); // FTW (空)
            let mx = m.cpu.mxcsr;
            m.write32(addr.wrapping_add(24), mx);
            m.write32(addr.wrapping_add(28), 0xFFFF); // MXCSR_MASK
            for i in 0..8 {
                let v = m.cpu.xmm[i];
                write_m128(m, addr.wrapping_add(160 + i as u32 * 16), v);
            }
            true
        }
        // FXRSTOR
        (1, Operand::Mem { addr, .. }) => {
            m.cpu.fpu_cw = m.read16(addr);
            m.cpu.mxcsr = m.read32(addr.wrapping_add(24));
            for i in 0..8 {
                m.cpu.xmm[i] = read_m128(m, addr.wrapping_add(160 + i as u32 * 16));
            }
            true
        }
        // LDMXCSR / STMXCSR
        (2, Operand::Mem { addr, .. }) => {
            m.cpu.mxcsr = m.read32(addr);
            true
        }
        (3, Operand::Mem { addr, .. }) => {
            let v = m.cpu.mxcsr;
            m.write32(addr, v);
            true
        }
        // SFENCE/LFENCE/MFENCE (mod=3): 逐次実行なので順序は常に守られている
        (5..=7, Operand::Reg(_)) => true,
        _ => {
            let _ = d;
            false
        }
    }
}

// AXは使わないが、既存のimportパターンに合わせて明示しておく
#[allow(unused_imports)]
use super::DX as _KEEP;
#[allow(dead_code)]
const _: usize = AX;
