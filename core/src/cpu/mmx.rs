//! MMX — 64bitのmmレジスタと、その上のパック整数演算。
//!
//! ## なぜ要るのか
//!
//! CPUIDでSSE2を名乗った時点で、**MMXは名乗らなくても使われる**。
//! i686のABIでは SSE2 ⊃ MMX が常識で、AlpineのOpenSSL (libcrypto) は
//! ベンチ選択の際にCPUIDのMMXビットを見ずに `movq mm0, [eax]` (0F 6F) を
//! 踏む。未実装だと #UD で `wget https://…` が即死する — TLSの壁の1枚目。
//!
//! ## mmレジスタはFPUレジスタの別名
//!
//! mm(i) は x87 物理レジスタ R(i) の**仮数64bit**である (TOPは見ない)。
//! 実機ではMMX命令を1つ実行するたびに TOP=0・タグ全valid になり、
//! 書いたレジスタの指数部は全1になる。カーネルはFXSAVE/FXRSTORで
//! x87とMMXを区別せず退避するので、このエイリアスを正しくやらないと
//! **コンテキストスイッチのたびにMMXの計算が壊れる**。
//! [`Fpu`](super::fpu::Fpu) の80bit原本サイドバンド (`raw`) がこれを支える。
//!
//! ## デコードの位置
//!
//! SSEと同じ0F空間の**プレフィクス無し**の顔がMMXである
//! (66 が付けばXMM上のSSE2)。twobyte.rs は未知の0Fをまず本モジュールに
//! 見せ、MMXの管轄でなければ sse.rs へ回す。
//!
//! ## 割り切り
//!
//! - CR0.EM/TS の検査はしない (x87本体と同じ割り切り)

use super::operand::{fetch8, modrm, Operand};
use super::sse::{pfx, read_m64, write_m64, Pfx};
use super::Decoder;
use crate::Machine;

// ---- レーン分解 (64bit = 8×u8 / 4×u16 / 2×u32) ----

fn to8(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}
fn from8(a: [u8; 8]) -> u64 {
    u64::from_le_bytes(a)
}
fn to16(v: u64) -> [u16; 4] {
    core::array::from_fn(|i| (v >> (i * 16)) as u16)
}
fn from16(a: [u16; 4]) -> u64 {
    a.iter()
        .enumerate()
        .fold(0, |acc, (i, &x)| acc | (x as u64) << (i * 16))
}
fn to32(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}
fn from32(a: [u32; 2]) -> u64 {
    a[0] as u64 | (a[1] as u64) << 32
}

fn lanes8(f: impl Fn(u8, u8) -> u8) -> impl Fn(u64, u64) -> u64 {
    move |a, b| {
        let (x, y) = (to8(a), to8(b));
        from8(core::array::from_fn(|i| f(x[i], y[i])))
    }
}
fn lanes16(f: impl Fn(u16, u16) -> u16) -> impl Fn(u64, u64) -> u64 {
    move |a, b| {
        let (x, y) = (to16(a), to16(b));
        from16(core::array::from_fn(|i| f(x[i], y[i])))
    }
}
fn lanes32(f: impl Fn(u32, u32) -> u32) -> impl Fn(u64, u64) -> u64 {
    move |a, b| {
        let (x, y) = (to32(a), to32(b));
        from32(core::array::from_fn(|i| f(x[i], y[i])))
    }
}

// ---- オペランドの出し入れ ----

/// rm側 (mm または m64) を読む
fn rm_mm(m: &mut Machine, rm: &Operand) -> u64 {
    match rm {
        Operand::Reg(r) => m.cpu.fpu.mm(*r),
        Operand::Mem { addr, .. } => read_m64(m, *addr),
    }
}

/// mm[reg] = f(mm[reg], rm側) の形の命令
fn binop(m: &mut Machine, d: &Decoder, f: impl Fn(u64, u64) -> u64) {
    let (reg, rm) = modrm(m, d);
    let b = rm_mm(m, &rm);
    let a = m.cpu.fpu.mm(reg);
    m.cpu.fpu.set_mm(reg, f(a, b));
}

/// シフト量: 下位64bit全体がカウント (レーン幅を超えたら全ビットが流れ出る)
fn shift_count(b: u64, width: u32) -> Option<u32> {
    if b >= width as u64 {
        None
    } else {
        Some(b as u32)
    }
}

/// MMX命令の一歩。管轄外なら false (sse.rs に回る)。
/// cold + inline(never): 呼び手 (twobyte::step) は最ホットの関数で、
/// この大きなmatchが巻き込まれると**MMXを1命令も踏まない起動が遅くなる**
/// (交互A/Bで +4.9% を観測した)。ホットパスから隔離する
#[cold]
#[inline(never)]
pub(crate) fn step_mmx(m: &mut Machine, d: &Decoder, op2: u8) -> bool {
    // mmレジスタはFPUの別名 — FPUを挿していない16bit機にMMXは存在しない
    if !m.profile.has_fpu {
        return false;
    }
    // プレフィクス付きはSSEの顔 (66=XMM整数、F3/F2は別命令)
    if pfx(d) != Pfx::None {
        return false;
    }
    match op2 {
        // ---- データ移動 ----
        // movd mm ← r/m32 (ゼロ拡張)
        0x6E => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.regs[r],
                Operand::Mem { addr, .. } => m.read32(addr),
            };
            m.cpu.fpu.set_mm(reg, v as u64);
        }
        // movd r/m32 ← mm (下位32bit)
        0x7E => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.fpu.mm(reg) as u32;
            match rm {
                Operand::Reg(r) => m.cpu.regs[r] = v,
                Operand::Mem { addr, .. } => m.write32(addr, v),
            }
        }
        // movq mm ← mm/m64 — libcryptoが最初に踏んだ命令 (0F 6F)
        0x6F => {
            let (reg, rm) = modrm(m, d);
            let v = rm_mm(m, &rm);
            m.cpu.fpu.set_mm(reg, v);
        }
        // movq mm/m64 ← mm
        0x7F => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.fpu.mm(reg);
            match rm {
                Operand::Reg(r) => m.cpu.fpu.set_mm(r, v),
                Operand::Mem { addr, .. } => write_m64(m, addr, v),
            }
        }
        // movntq m64 ← mm: 「キャッシュを汚さない」— キャッシュが無いのでただの書き込み
        0xE7 => {
            let (reg, rm) = modrm(m, d);
            let Operand::Mem { addr, .. } = rm else {
                return false;
            };
            let v = m.cpu.fpu.mm(reg);
            write_m64(m, addr, v);
        }

        // ---- 並べ替え ----
        0x60 => binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            from8(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[i / 2]
                } else {
                    y[i / 2]
                }
            }))
        }),
        0x61 => binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            from16(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[i / 2]
                } else {
                    y[i / 2]
                }
            }))
        }),
        0x62 => binop(m, d, |a, b| from32([to32(a)[0], to32(b)[0]])),
        0x68 => binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            from8(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[4 + i / 2]
                } else {
                    y[4 + i / 2]
                }
            }))
        }),
        0x69 => binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            from16(core::array::from_fn(|i| {
                if i % 2 == 0 {
                    x[2 + i / 2]
                } else {
                    y[2 + i / 2]
                }
            }))
        }),
        0x6A => binop(m, d, |a, b| from32([to32(a)[1], to32(b)[1]])),
        // pack系: 幅を半分に潰して2本を1本に (飽和つき)
        0x63 => binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            let sat = |v: u16| (v as i16).clamp(-128, 127) as i8 as u8;
            from8(core::array::from_fn(|i| {
                if i < 4 {
                    sat(x[i])
                } else {
                    sat(y[i - 4])
                }
            }))
        }),
        0x67 => binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            let sat = |v: u16| (v as i16).clamp(0, 255) as u8;
            from8(core::array::from_fn(|i| {
                if i < 4 {
                    sat(x[i])
                } else {
                    sat(y[i - 4])
                }
            }))
        }),
        0x6B => binop(m, d, |a, b| {
            let (x, y) = (to32(a), to32(b));
            let sat = |v: u32| (v as i32).clamp(-32768, 32767) as i16 as u16;
            from16([sat(x[0]), sat(x[1]), sat(y[0]), sat(y[1])])
        }),
        // pinsrw mm, r32/m16, imm8 — 指定wordだけ差し替える (ghash-x86が使う)
        0xC4 => {
            let (reg, rm) = modrm(m, d);
            let v = match rm {
                Operand::Reg(r) => m.cpu.regs[r] as u16,
                Operand::Mem { addr, .. } => m.read16(addr),
            };
            let imm = (fetch8(m) & 3) as usize;
            let mut s = to16(m.cpu.fpu.mm(reg));
            s[imm] = v;
            m.cpu.fpu.set_mm(reg, from16(s));
        }
        // pextrw r32, mm, imm8 — 指定wordをゼロ拡張で取り出す (レジスタ専用)
        0xC5 => {
            let (reg, rm) = modrm(m, d);
            let Operand::Reg(r) = rm else {
                return false;
            };
            let imm = (fetch8(m) & 3) as usize;
            m.cpu.regs[reg] = to16(m.cpu.fpu.mm(r))[imm] as u32;
            m.cpu.fpu.mmx_touch();
        }
        // pshufw mm, mm/m64, imm8 (SSEがMMXレジスタに足した並べ替え)
        0x70 => {
            let (reg, rm) = modrm(m, d);
            let src = rm_mm(m, &rm);
            let imm = fetch8(m) as usize;
            let s = to16(src);
            let v = from16(core::array::from_fn(|i| s[(imm >> (i * 2)) & 3]));
            m.cpu.fpu.set_mm(reg, v);
        }

        // ---- 比較 ----
        0x74 => binop(m, d, lanes8(|a, b| if a == b { 0xFF } else { 0 })),
        0x75 => binop(m, d, lanes16(|a, b| if a == b { 0xFFFF } else { 0 })),
        0x76 => binop(m, d, lanes32(|a, b| if a == b { 0xFFFF_FFFF } else { 0 })),
        0x64 => binop(
            m,
            d,
            lanes8(|a, b| if (a as i8) > b as i8 { 0xFF } else { 0 }),
        ),
        0x65 => binop(
            m,
            d,
            lanes16(|a, b| if (a as i16) > b as i16 { 0xFFFF } else { 0 }),
        ),
        0x66 => binop(
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
        // pmovmskb: 各バイトの符号ビットを束ねて汎用レジスタへ
        0xD7 => {
            let (reg, rm) = modrm(m, d);
            let Operand::Reg(r) = rm else {
                return false; // レジスタ専用
            };
            let v = m.cpu.fpu.mm(r);
            let mask = to8(v)
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | ((b >> 7) as u32) << i);
            m.cpu.regs[reg] = mask;
            m.cpu.fpu.mmx_touch();
        }

        // ---- 加減算 ----
        0xFC => binop(m, d, lanes8(u8::wrapping_add)),
        0xFD => binop(m, d, lanes16(u16::wrapping_add)),
        0xFE => binop(m, d, lanes32(u32::wrapping_add)),
        0xD4 => binop(m, d, u64::wrapping_add), // paddq (SSE2がMMXに足した)
        0xF8 => binop(m, d, lanes8(u8::wrapping_sub)),
        0xF9 => binop(m, d, lanes16(u16::wrapping_sub)),
        0xFA => binop(m, d, lanes32(u32::wrapping_sub)),
        0xFB => binop(m, d, u64::wrapping_sub), // psubq
        // 飽和つき
        0xDC => binop(m, d, lanes8(u8::saturating_add)),
        0xDD => binop(m, d, lanes16(u16::saturating_add)),
        0xD8 => binop(m, d, lanes8(u8::saturating_sub)),
        0xD9 => binop(m, d, lanes16(u16::saturating_sub)),
        0xEC => binop(m, d, lanes8(|a, b| (a as i8).saturating_add(b as i8) as u8)),
        0xED => binop(
            m,
            d,
            lanes16(|a, b| (a as i16).saturating_add(b as i16) as u16),
        ),
        0xE8 => binop(m, d, lanes8(|a, b| (a as i8).saturating_sub(b as i8) as u8)),
        0xE9 => binop(
            m,
            d,
            lanes16(|a, b| (a as i16).saturating_sub(b as i16) as u16),
        ),
        // min/max/平均 (SSEがMMXに足した)
        0xDA => binop(m, d, lanes8(u8::min)),
        0xDE => binop(m, d, lanes8(u8::max)),
        0xEA => binop(m, d, lanes16(|a, b| (a as i16).min(b as i16) as u16)),
        0xEE => binop(m, d, lanes16(|a, b| (a as i16).max(b as i16) as u16)),
        0xE0 => binop(m, d, lanes8(|a, b| ((a as u16 + b as u16 + 1) >> 1) as u8)),
        0xE3 => binop(
            m,
            d,
            lanes16(|a, b| ((a as u32 + b as u32 + 1) >> 1) as u16),
        ),

        // ---- 乗算 ----
        0xD5 => binop(
            m,
            d,
            lanes16(|a, b| (a as i16 as i32).wrapping_mul(b as i16 as i32) as u16),
        ),
        0xE5 => binop(
            m,
            d,
            lanes16(|a, b| ((a as i16 as i32 * b as i16 as i32) >> 16) as u16),
        ),
        0xE4 => binop(m, d, lanes16(|a, b| ((a as u32 * b as u32) >> 16) as u16)),
        // pmuludq: 下位dword同士のフル積 (SSE2がMMXに足した。bn/montが使う)
        0xF4 => binop(m, d, |a, b| (a as u32 as u64) * (b as u32 as u64)),
        // pmaddwd: 隣り合うword積の対和
        0xF5 => binop(m, d, |a, b| {
            let (x, y) = (to16(a), to16(b));
            let p = |i: usize| x[i] as i16 as i32 * (y[i] as i16 as i32);
            let lo = p(0).wrapping_add(p(1)) as u32;
            let hi = p(2).wrapping_add(p(3)) as u32;
            from32([lo, hi])
        }),
        // psadbw: バイト差の絶対値の和 → 下位16bit
        0xF6 => binop(m, d, |a, b| {
            let (x, y) = (to8(a), to8(b));
            (0..8)
                .map(|i| (x[i] as i16 - y[i] as i16).unsigned_abs() as u64)
                .sum()
        }),

        // ---- ビット演算 ----
        0xDB => binop(m, d, |a, b| a & b),
        0xDF => binop(m, d, |a, b| !a & b),
        0xEB => binop(m, d, |a, b| a | b),
        0xEF => binop(m, d, |a, b| a ^ b),

        // ---- シフト (レジスタ形: 下位64bitがカウント) ----
        0xD1 => binop(m, d, |a, b| match shift_count(b, 16) {
            Some(n) => from16(to16(a).map(|x| x >> n)),
            None => 0,
        }),
        0xD2 => binop(m, d, |a, b| match shift_count(b, 32) {
            Some(n) => from32(to32(a).map(|x| x >> n)),
            None => 0,
        }),
        0xD3 => binop(m, d, |a, b| match shift_count(b, 64) {
            Some(n) => a >> n,
            None => 0,
        }),
        0xF1 => binop(m, d, |a, b| match shift_count(b, 16) {
            Some(n) => from16(to16(a).map(|x| x << n)),
            None => 0,
        }),
        0xF2 => binop(m, d, |a, b| match shift_count(b, 32) {
            Some(n) => from32(to32(a).map(|x| x << n)),
            None => 0,
        }),
        0xF3 => binop(m, d, |a, b| match shift_count(b, 64) {
            Some(n) => a << n,
            None => 0,
        }),
        // psra: 符号を引きずる (カウント過大は幅-1に飽和)
        0xE1 => binop(m, d, |a, b| {
            let n = (b as u32).min(15);
            from16(to16(a).map(|x| ((x as i16) >> n) as u16))
        }),
        0xE2 => binop(m, d, |a, b| {
            let n = (b as u32).min(31);
            from32(to32(a).map(|x| ((x as i32) >> n) as u32))
        }),
        // 即値形 (71/72/73 はModRMのreg欄が演算選択)
        0x71..=0x73 => {
            let (kind, rm) = modrm(m, d);
            let n = fetch8(m) as u32;
            let Operand::Reg(r) = rm else {
                return false;
            };
            let v = m.cpu.fpu.mm(r);
            let out = match (op2, kind) {
                (0x71, 2) => from16(to16(v).map(|x| if n < 16 { x >> n } else { 0 })),
                (0x71, 4) => from16(to16(v).map(|x| ((x as i16) >> n.min(15)) as u16)),
                (0x71, 6) => from16(to16(v).map(|x| if n < 16 { x << n } else { 0 })),
                (0x72, 2) => from32(to32(v).map(|x| if n < 32 { x >> n } else { 0 })),
                (0x72, 4) => from32(to32(v).map(|x| ((x as i32) >> n.min(31)) as u32)),
                (0x72, 6) => from32(to32(v).map(|x| if n < 32 { x << n } else { 0 })),
                (0x73, 2) => {
                    if n < 64 {
                        v >> n
                    } else {
                        0
                    }
                }
                (0x73, 6) => {
                    if n < 64 {
                        v << n
                    } else {
                        0
                    }
                }
                _ => return false,
            };
            m.cpu.fpu.set_mm(r, out);
        }

        // maskmovq: 各バイトのマスクMSBが立つ所だけ [EDI] へ書く
        // (メモリオペランドはModRMではなく**暗黙のDS:EDI**。上書きは有効)
        0xF7 => {
            let (reg, rm) = modrm(m, d);
            let Operand::Reg(r) = rm else {
                return false;
            };
            let data = m.cpu.fpu.mm(reg).to_le_bytes();
            let mask = m.cpu.fpu.mm(r).to_le_bytes();
            let seg = d.seg_override.unwrap_or(crate::cpu::DS);
            let di = m.cpu.regs[crate::cpu::DI];
            for (i, (&b, &mk)) in data.iter().zip(mask.iter()).enumerate() {
                if mk & 0x80 != 0 {
                    let a = m.cpu.lin(seg, di.wrapping_add(i as u32));
                    m.write8(a, b);
                }
            }
        }

        // EMMS: MMXの世界からx87へ返す (ModRM無し)
        0x77 => {
            m.cpu.fpu.emms();
            return true; // mmx_touchは通さない — タグを空にしたばかり
        }

        _ => return false,
    }
    // 全MMX命令の共通作用 (Intel SDM): TOP=0、タグ全valid
    m.cpu.fpu.mmx_touch();
    true
}
