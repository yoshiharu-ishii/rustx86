//! x87 FPU — **f64裏打ち** (QEMU-tiny / v86 と同じ割り切り)。
//!
//! ## なぜ今作るか
//!
//! 以前は「検出と初期化に答えるだけ」のスタブで、演算のESCを黙って流していた。
//! その結果 musl の strtod (x87の長倍精度で計算する) が壊れ、busybox の
//! `sleep 3` が **parse_duration の時点で20msに化けた**。ゲストのpingが
//! 本物のインターネットへ毎秒数百発の洪水になった事故 (2026-08-14) の
//! 根本原因がこれである。printf '%f' は dtoa が収束せず無限ループした。
//!
//! ## 割り切り
//!
//! - **レジスタは f64。** 実機の80bit拡張倍精度より仮数が11bit短いが、
//!   strtod/printf/libmの実用には足りる (QEMU-tinyやv86も同じ設計)。
//!   80bitのロード/ストアは境界で変換する
//! - **例外は起こさない。** マスク済み例外の既定動作 (NaN伝播・∞・0) は
//!   f64の算術がそのまま与える。SW の例外ビットは常に0
//! - **未実装のESCは黙って流さず trap で止める。** 黙って流した結果が
//!   今回の事故なので、二度と同じ穴には落ちない
//!
//! 検証: strtod・printf・(long)キャストが踏む列を単体テストで釘打ちし、
//! ゲスト実測 (`sleep 3` が3秒・`printf '%f' 3` が 3.000000) で締める。

use super::{Operand, AX, CF, PF, ZF};
use crate::Machine;

/// x87の状態 (制御語だけは歴史的経緯で `Cpu::fpu_cw` に居る)
#[derive(Debug, Clone)]
pub struct Fpu {
    /// 物理レジスタ。論理 st(i) は `regs[(top + i) & 7]`
    pub regs: [f64; 8],
    /// 空きビットマスク (物理番号)。1 = 空
    pub empty: u8,
    /// スタックトップ (物理番号)
    pub top: u8,
    /// 条件コード C0-C3 (SWのビット位置のまま保持)
    pub cond: u16,
}

const C0: u16 = 1 << 8;
const C1: u16 = 1 << 9;
const C2: u16 = 1 << 10;
const C3: u16 = 1 << 14;

impl Default for Fpu {
    fn default() -> Self {
        Self {
            regs: [0.0; 8],
            empty: 0xFF,
            top: 0,
            cond: 0,
        }
    }
}

impl Fpu {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// ステータスワード。例外ビットは常に0 (例外は起こさない設計)
    pub fn status(&self) -> u16 {
        self.cond | ((self.top as u16) << 11)
    }

    fn phys(&self, i: usize) -> usize {
        (self.top as usize + i) & 7
    }

    fn st(&self, i: usize) -> f64 {
        self.regs[self.phys(i)]
    }

    fn set_st(&mut self, i: usize, v: f64) {
        let p = self.phys(i);
        self.regs[p] = v;
        self.empty &= !(1 << p);
    }

    fn push(&mut self, v: f64) {
        self.top = self.top.wrapping_sub(1) & 7;
        let t = self.top as usize;
        self.regs[t] = v;
        self.empty &= !(1 << t);
    }

    fn pop(&mut self) {
        self.empty |= 1 << self.top;
        self.top = (self.top + 1) & 7;
    }

    /// st(i) が空か (FXSAVEの簡約タグ用)
    pub fn st_empty(&self, i: usize) -> bool {
        self.empty & (1 << self.phys(i)) != 0
    }

    /// st(i) の生の値 (空でも読む — FXSAVEは8本全部書く)
    pub fn st_raw(&self, i: usize) -> f64 {
        self.regs[self.phys(i)]
    }

    /// st(i) へ書き戻す (FXRSTOR用)。valid=false なら空印を立てる
    pub fn set_st_raw(&mut self, i: usize, v: f64, valid: bool) {
        let p = self.phys(i);
        self.regs[p] = v;
        if valid {
            self.empty &= !(1 << p);
        } else {
            self.empty |= 1 << p;
        }
    }

    /// タグワード (2bit×8、物理番号順)。00=有効 01=ゼロ 10=特殊 11=空
    fn tag_word(&self) -> u16 {
        let mut w = 0u16;
        for p in 0..8 {
            let t = if self.empty & (1 << p) != 0 {
                3
            } else {
                let v = self.regs[p];
                if v == 0.0 {
                    1
                } else if !v.is_finite() {
                    2
                } else {
                    0
                }
            };
            w |= t << (p * 2);
        }
        w
    }

    fn load_tag_word(&mut self, w: u16) {
        self.empty = 0;
        for p in 0..8 {
            if (w >> (p * 2)) & 3 == 3 {
                self.empty |= 1 << p;
            }
        }
    }
}

// ---------- 80bit拡張倍精度との変換 (メモリ境界だけで使う) ----------

/// f64 → (仮数64bit, 符号+指数15bit)。仮数のbit63は明示的な整数ビット
pub fn f64_to_f80(v: f64) -> (u64, u16) {
    let bits = v.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    match exp {
        0 => {
            if frac == 0 {
                (0, sign) // ±0
            } else {
                // 非正規化数: 仮数を正規化して指数に繰り込む
                let lz = frac.leading_zeros(); // 上位12bitは0なので lz >= 12
                let mant = frac << lz; // bit63 に整数ビットが立つ
                let e80 = 16383 + 63 - 1074 - lz as i32;
                (mant, sign | (e80 as u16 & 0x7FFF))
            }
        }
        0x7FF => {
            // ∞ / NaN。NaNは仮数を引き継ぐ (bit63は立てる)
            let mant = (1u64 << 63) | (frac << 11);
            (mant, sign | 0x7FFF)
        }
        _ => {
            let mant = (1u64 << 63) | (frac << 11);
            let e80 = exp - 1023 + 16383;
            (mant, sign | (e80 as u16))
        }
    }
}

/// (仮数, 符号+指数) → f64。落ちる下位11bitは最近接偶数丸め
pub fn f80_to_f64(mant: u64, se: u16) -> f64 {
    let sign = if se & 0x8000 != 0 { -1.0f64 } else { 1.0 };
    let e = (se & 0x7FFF) as i32;
    if e == 0x7FFF {
        return if mant << 1 == 0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        };
    }
    if mant == 0 {
        return sign * 0.0;
    }
    // 擬似非正規も含め、bit63へ寄せてから2の冪で位取りする
    let lz = mant.leading_zeros();
    let m = mant << lz;
    let e2 = e - lz as i32 - 16383 - 63 + 64; // m×2^(e2-64) が値
                                              // f64へ: m (bit63=1) を仮数53bitへ。位取りは段階掛け (scale2) で行う —
                                              // powi は非正規化数の指数で先にアンダーフローする (テストが捕まえた)
    let target = scale2((m >> 11) as f64, e2 - 53) * sign;
    // 丸め落ちした11bitの寄与 (ほぼ効かないが、strtodの境界で効く)
    let low = (m & 0x7FF) as f64;
    target + scale2(low, e2 - 64) * sign
}

// ---------- メモリの読み書き ----------

fn read_f32(m: &mut Machine, a: u32) -> f64 {
    f32::from_bits(m.read32(a)) as f64
}
fn read_f64(m: &mut Machine, a: u32) -> f64 {
    f64::from_bits(m.read32(a) as u64 | (m.read32(a + 4) as u64) << 32)
}
fn read_f80(m: &mut Machine, a: u32) -> f64 {
    let mant = m.read32(a) as u64 | (m.read32(a + 4) as u64) << 32;
    let se = m.read16(a + 8);
    f80_to_f64(mant, se)
}
fn write_f32(m: &mut Machine, a: u32, v: f64) {
    m.write32(a, (v as f32).to_bits());
}
fn write_f64(m: &mut Machine, a: u32, v: f64) {
    let b = v.to_bits();
    m.write32(a, b as u32);
    m.write32(a + 4, (b >> 32) as u32);
}
fn write_f80(m: &mut Machine, a: u32, v: f64) {
    let (mant, se) = f64_to_f80(v);
    m.write32(a, mant as u32);
    m.write32(a + 4, (mant >> 32) as u32);
    m.write16(a + 8, se);
}

/// 整数変換。丸めは制御語のRC (bit10-11): 0=最近接偶数 1=床 2=天井 3=切り捨て
fn round_by_cw(m: &Machine, v: f64) -> f64 {
    match (m.cpu.fpu_cw >> 10) & 3 {
        0 => {
            // 最近接・タイは偶数 (f64::round は四捨五入なので使わない)
            let r = v.round();
            if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
                r - v.signum()
            } else {
                r
            }
        }
        1 => v.floor(),
        2 => v.ceil(),
        _ => v.trunc(),
    }
}

fn to_int(m: &Machine, v: f64, min: i64, max: i64) -> i64 {
    let r = round_by_cw(m, v);
    if r.is_nan() || r < min as f64 || r > max as f64 {
        min // 実機の「不定値」= 最小値 (0x8000…)
    } else {
        r as i64
    }
}

// ---------- 比較 ----------

fn compare(fpu: &mut Fpu, a: f64, b: f64) {
    fpu.cond &= !(C0 | C2 | C3);
    if a.is_nan() || b.is_nan() {
        fpu.cond |= C0 | C2 | C3; // 順序なし
    } else if a < b {
        fpu.cond |= C0;
    } else if a == b {
        fpu.cond |= C3;
    }
}

/// FCOMI系: 結果をEFLAGSへ (ZF/PF/CF)。i686の作法
fn compare_eflags(m: &mut Machine, a: f64, b: f64) {
    let mut f = m.cpu.flags & !(ZF | PF | CF);
    if a.is_nan() || b.is_nan() {
        f |= ZF | PF | CF;
    } else if a < b {
        f |= CF;
    } else if a == b {
        f |= ZF;
    }
    m.cpu.flags = f;
}

// ---------- 環境の保存/復元 (FNSTENV / FNSAVE) ----------

/// 32bit保護モードの環境 28バイト: CW, SW, TAG, FIP, FCS, FDP, FDS (各32bit)
fn store_env(m: &mut Machine, a: u32) {
    let cw = m.cpu.fpu_cw as u32;
    let sw = m.cpu.fpu.status() as u32;
    let tag = m.cpu.fpu.tag_word() as u32;
    m.write32(a, cw);
    m.write32(a + 4, sw);
    m.write32(a + 8, tag);
    // 命令/データポインタは持っていない (例外を起こさないので誰も見ない)
    for i in 3..7 {
        m.write32(a + i * 4, 0);
    }
}

fn load_env(m: &mut Machine, a: u32) {
    m.cpu.fpu_cw = m.read32(a) as u16;
    let sw = m.read32(a + 4) as u16;
    m.cpu.fpu.top = ((sw >> 11) & 7) as u8;
    m.cpu.fpu.cond = sw & (C0 | C1 | C2 | C3);
    let tag = m.read32(a + 8) as u16;
    m.cpu.fpu.load_tag_word(tag);
}

/// ldexp相当: v × 2^e (羃の直接演算。powiの誤差と範囲切れを避ける)
fn scale2(v: f64, e: i32) -> f64 {
    let step = |x: f64, e: i32| -> f64 {
        let e = e.clamp(-1022, 1023);
        x * f64::from_bits(((e + 1023) as u64) << 52)
    };
    // 3回に分ける — 非正規化数の端 (2^-1074) は2段では届かない
    let a = e / 3;
    let b = (e - a) / 2;
    step(step(step(v, a), b), e - a - b)
}

// ---------- 本体 ----------

/// ESC命令 (0xD8-0xDF) の実行。`reg` はModRMのreg欄、`rm` は実効オペランド
pub fn exec(m: &mut Machine, op: u8, reg: usize, rm: &Operand) {
    match rm {
        Operand::Mem { addr, .. } => exec_mem(m, op, reg, *addr),
        Operand::Reg(i) => exec_reg(m, op, reg, *i),
    }
}

fn exec_mem(m: &mut Machine, op: u8, reg: usize, a: u32) {
    // 算術グループ (D8=m32real DA=m32int DC=m64real DE=m16int)
    let arith_src = |m: &mut Machine| -> Option<f64> {
        Some(match op {
            0xD8 => read_f32(m, a),
            0xDA => m.read32(a) as i32 as f64,
            0xDC => read_f64(m, a),
            0xDE => m.read16(a) as i16 as f64,
            _ => return None,
        })
    };
    match (op, reg) {
        // --- ロード/ストア ---
        (0xD9, 0) => {
            let v = read_f32(m, a);
            m.cpu.fpu.push(v);
        }
        (0xDD, 0) => {
            let v = read_f64(m, a);
            m.cpu.fpu.push(v);
        }
        (0xDB, 5) => {
            let v = read_f80(m, a);
            m.cpu.fpu.push(v);
        }
        (0xD9, 2) => write_f32(m, a, m.cpu.fpu.st(0)),
        (0xD9, 3) => {
            write_f32(m, a, m.cpu.fpu.st(0));
            m.cpu.fpu.pop();
        }
        (0xDD, 2) => write_f64(m, a, m.cpu.fpu.st(0)),
        (0xDD, 3) => {
            write_f64(m, a, m.cpu.fpu.st(0));
            m.cpu.fpu.pop();
        }
        (0xDB, 7) => {
            write_f80(m, a, m.cpu.fpu.st(0));
            m.cpu.fpu.pop();
        }
        // --- 整数ロード/ストア ---
        (0xDF, 0) => {
            let v = m.read16(a) as i16 as f64;
            m.cpu.fpu.push(v);
        }
        (0xDB, 0) => {
            let v = m.read32(a) as i32 as f64;
            m.cpu.fpu.push(v);
        }
        (0xDF, 5) => {
            let v = (m.read32(a) as u64 | (m.read32(a + 4) as u64) << 32) as i64 as f64;
            m.cpu.fpu.push(v);
        }
        (0xDF, 2) | (0xDF, 3) => {
            let v = to_int(m, m.cpu.fpu.st(0), i16::MIN as i64, i16::MAX as i64);
            m.write16(a, v as u16);
            if reg == 3 {
                m.cpu.fpu.pop();
            }
        }
        (0xDB, 2) | (0xDB, 3) => {
            let v = to_int(m, m.cpu.fpu.st(0), i32::MIN as i64, i32::MAX as i64);
            m.write32(a, v as u32);
            if reg == 3 {
                m.cpu.fpu.pop();
            }
        }
        (0xDF, 7) => {
            let v = to_int(m, m.cpu.fpu.st(0), i64::MIN, i64::MAX);
            m.write32(a, v as u32);
            m.write32(a + 4, (v as u64 >> 32) as u32);
            m.cpu.fpu.pop();
        }
        // FISTTP (常に切り捨て。SSE3世代だがコンパイラが出すことがある)
        (0xDF, 1) => {
            let v = m.cpu.fpu.st(0).trunc();
            let v = if v.is_nan() || v < i16::MIN as f64 || v > i16::MAX as f64 {
                i16::MIN as i64
            } else {
                v as i64
            };
            m.write16(a, v as u16);
            m.cpu.fpu.pop();
        }
        (0xDB, 1) => {
            let v = m.cpu.fpu.st(0).trunc();
            let v = if v.is_nan() || v < i32::MIN as f64 || v > i32::MAX as f64 {
                i32::MIN as i64
            } else {
                v as i64
            };
            m.write32(a, v as u32);
            m.cpu.fpu.pop();
        }
        (0xDD, 1) => {
            let v = m.cpu.fpu.st(0).trunc();
            let v = if v.is_nan() || v < i64::MIN as f64 || v > i64::MAX as f64 {
                i64::MIN
            } else {
                v as i64
            };
            m.write32(a, v as u32);
            m.write32(a + 4, (v as u64 >> 32) as u32);
            m.cpu.fpu.pop();
        }
        // --- 算術 (メモリオペランド) ---
        (0xD8 | 0xDA | 0xDC | 0xDE, _) => {
            let b = arith_src(m).unwrap();
            let a0 = m.cpu.fpu.st(0);
            match reg {
                0 => m.cpu.fpu.set_st(0, a0 + b),
                1 => m.cpu.fpu.set_st(0, a0 * b),
                2 => compare(&mut m.cpu.fpu, a0, b), // FCOM
                3 => {
                    compare(&mut m.cpu.fpu, a0, b); // FCOMP
                    m.cpu.fpu.pop();
                }
                4 => m.cpu.fpu.set_st(0, a0 - b),
                5 => m.cpu.fpu.set_st(0, b - a0),
                6 => m.cpu.fpu.set_st(0, a0 / b),
                _ => m.cpu.fpu.set_st(0, b / a0),
            }
        }
        // --- 制御語・環境 ---
        (0xD9, 4) => load_env(m, a),
        (0xD9, 5) => m.cpu.fpu_cw = m.read16(a),
        (0xD9, 6) => {
            store_env(m, a);
            m.cpu.fpu_cw |= 0x3F; // FNSTENVは保存後に全例外をマスクする (仕様)
        }
        (0xD9, 7) => {
            let cw = m.cpu.fpu_cw;
            m.write16(a, cw);
        }
        (0xDD, 4) => {
            // FRSTOR: 環境 + レジスタ8本 (80bit×8)
            load_env(m, a);
            for i in 0..8 {
                let v = read_f80(m, a + 28 + i as u32 * 10);
                let p = m.cpu.fpu.phys(i);
                m.cpu.fpu.regs[p] = v;
            }
        }
        (0xDD, 6) => {
            // FNSAVE: 環境 + レジスタ、その後 FNINIT
            store_env(m, a);
            for i in 0..8 {
                let v = m.cpu.fpu.st(i);
                write_f80(m, a + 28 + i as u32 * 10, v);
            }
            m.cpu.fpu.reset();
            m.cpu.fpu_cw = 0x037F;
        }
        (0xDD, 7) => {
            let sw = m.cpu.fpu.status();
            m.write16(a, sw);
        }
        // FBLD/FBSTP (BCD)。使い手が現れたら実装する
        _ => m.trap(format!("x87 mem op={op:02X} /{reg}")),
    }
}

fn exec_reg(m: &mut Machine, op: u8, reg: usize, i: usize) {
    match (op, reg) {
        // --- D8: st(0) を先に、st(i) を後に ---
        (0xD8, _) => {
            let b = m.cpu.fpu.st(i);
            let a0 = m.cpu.fpu.st(0);
            match reg {
                0 => m.cpu.fpu.set_st(0, a0 + b),
                1 => m.cpu.fpu.set_st(0, a0 * b),
                2 => compare(&mut m.cpu.fpu, a0, b),
                3 => {
                    compare(&mut m.cpu.fpu, a0, b);
                    m.cpu.fpu.pop();
                }
                4 => m.cpu.fpu.set_st(0, a0 - b),
                5 => m.cpu.fpu.set_st(0, b - a0),
                6 => m.cpu.fpu.set_st(0, a0 / b),
                _ => m.cpu.fpu.set_st(0, b / a0),
            }
        }
        // --- D9: ロード・入れ替え・定数・単項演算 ---
        (0xD9, 0) => {
            let v = m.cpu.fpu.st(i);
            m.cpu.fpu.push(v);
        }
        (0xD9, 1) => {
            let a0 = m.cpu.fpu.st(0);
            let b = m.cpu.fpu.st(i);
            m.cpu.fpu.set_st(0, b);
            m.cpu.fpu.set_st(i, a0);
        }
        (0xD9, 2) if i == 0 => {} // FNOP
        (0xD9, 3) => {
            // FSTP1 (別名)。FSTP st(i) と同じ
            let v = m.cpu.fpu.st(0);
            m.cpu.fpu.set_st(i, v);
            m.cpu.fpu.pop();
        }
        (0xD9, 4) => {
            let a0 = m.cpu.fpu.st(0);
            match i {
                0 => m.cpu.fpu.set_st(0, -a0),         // FCHS
                1 => m.cpu.fpu.set_st(0, a0.abs()),    // FABS
                4 => compare(&mut m.cpu.fpu, a0, 0.0), // FTST
                5 => {
                    // FXAM: C3C2C0 で分類、C1 に符号
                    let f = &mut m.cpu.fpu;
                    f.cond &= !(C0 | C1 | C2 | C3);
                    if a0.is_sign_negative() {
                        f.cond |= C1;
                    }
                    if f.empty & (1 << f.top) != 0 {
                        f.cond |= C0 | C3; // 空
                    } else if a0.is_nan() {
                        f.cond |= C0;
                    } else if a0.is_infinite() {
                        f.cond |= C0 | C2;
                    } else if a0 == 0.0 {
                        f.cond |= C3;
                    } else if a0.is_subnormal() {
                        f.cond |= C2 | C3;
                    } else {
                        f.cond |= C2; // 正規数
                    }
                }
                _ => m.trap(format!("x87 D9 E{i:X}")),
            }
        }
        (0xD9, 5) => {
            // 定数のロード
            let v = match i {
                0 => 1.0,
                1 => std::f64::consts::LOG2_10,
                2 => std::f64::consts::LOG2_E,
                3 => std::f64::consts::PI,
                4 => std::f64::consts::LOG10_2,
                5 => std::f64::consts::LN_2,
                6 => 0.0,
                _ => {
                    m.trap("x87 D9 EF".into());
                    return;
                }
            };
            m.cpu.fpu.push(v);
        }
        (0xD9, 6) => {
            let a0 = m.cpu.fpu.st(0);
            match i {
                0 => m.cpu.fpu.set_st(0, a0.exp2() - 1.0), // F2XM1
                1 => {
                    // FYL2X: st1 = st1 * log2(st0); pop
                    let y = m.cpu.fpu.st(1);
                    m.cpu.fpu.set_st(1, y * a0.log2());
                    m.cpu.fpu.pop();
                }
                2 => {
                    // FPTAN: st0 = tan(st0); push 1.0
                    m.cpu.fpu.set_st(0, a0.tan());
                    m.cpu.fpu.push(1.0);
                    m.cpu.fpu.cond &= !C2;
                }
                3 => {
                    // FPATAN: st1 = atan2(st1, st0); pop
                    let y = m.cpu.fpu.st(1);
                    m.cpu.fpu.set_st(1, y.atan2(a0));
                    m.cpu.fpu.pop();
                }
                4 => {
                    // FXTRACT: st0 = 仮数、push 指数
                    let e = if a0 == 0.0 {
                        f64::NEG_INFINITY
                    } else {
                        a0.abs().log2().floor()
                    };
                    let sig = if a0 == 0.0 {
                        a0
                    } else {
                        scale2(a0, -(e as i32))
                    };
                    m.cpu.fpu.set_st(0, e);
                    m.cpu.fpu.push(sig);
                }
                5 => {
                    // FPREM1 (IEEE剰余)
                    let b = m.cpu.fpu.st(1);
                    let r = a0 - (a0 / b).round() * b;
                    m.cpu.fpu.set_st(0, r);
                    m.cpu.fpu.cond &= !C2; // 完了
                }
                6 => m.cpu.fpu.top = m.cpu.fpu.top.wrapping_sub(1) & 7, // FDECSTP
                7 => m.cpu.fpu.top = (m.cpu.fpu.top + 1) & 7,           // FINCSTP
                _ => unreachable!(),
            }
        }
        (0xD9, 7) => {
            let a0 = m.cpu.fpu.st(0);
            match i {
                0 => {
                    // FPREM (切り捨て剰余)。C2=0 で「完了」を報告し、
                    // 商の下位3bitを C0/C3/C1 へ (仕様の並び)
                    let b = m.cpu.fpu.st(1);
                    let q = (a0 / b).trunc();
                    m.cpu.fpu.set_st(0, a0 - q * b);
                    let f = &mut m.cpu.fpu;
                    f.cond &= !(C0 | C1 | C2 | C3);
                    let qi = q.abs() as u64;
                    if qi & 1 != 0 {
                        f.cond |= C1;
                    }
                    if qi & 2 != 0 {
                        f.cond |= C3;
                    }
                    if qi & 4 != 0 {
                        f.cond |= C0;
                    }
                }
                1 => {
                    // FYL2XP1
                    let y = m.cpu.fpu.st(1);
                    m.cpu.fpu.set_st(1, y * (a0 + 1.0).log2());
                    m.cpu.fpu.pop();
                }
                2 => m.cpu.fpu.set_st(0, a0.sqrt()),
                3 => {
                    // FSINCOS
                    m.cpu.fpu.set_st(0, a0.sin());
                    m.cpu.fpu.push(a0.cos());
                    m.cpu.fpu.cond &= !C2;
                }
                4 => {
                    let v = round_by_cw(m, a0);
                    m.cpu.fpu.set_st(0, v); // FRNDINT
                }
                5 => {
                    // FSCALE: st0 × 2^trunc(st1)
                    let e = m.cpu.fpu.st(1).trunc();
                    let e = e.clamp(-99999.0, 99999.0) as i32;
                    m.cpu.fpu.set_st(0, scale2(a0, e));
                }
                6 => {
                    m.cpu.fpu.set_st(0, a0.sin());
                    m.cpu.fpu.cond &= !C2;
                }
                _ => {
                    m.cpu.fpu.set_st(0, a0.cos());
                    m.cpu.fpu.cond &= !C2;
                }
            }
        }
        // --- DA: FCMOVcc / FUCOMPP ---
        (0xDA, 0..=3) => {
            let take = match reg {
                0 => m.cpu.flags & CF != 0,        // FCMOVB
                1 => m.cpu.flags & ZF != 0,        // FCMOVE
                2 => m.cpu.flags & (CF | ZF) != 0, // FCMOVBE
                _ => m.cpu.flags & PF != 0,        // FCMOVU
            };
            if take {
                let v = m.cpu.fpu.st(i);
                m.cpu.fpu.set_st(0, v);
            }
        }
        (0xDA, 5) if i == 1 => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(1));
            compare(&mut m.cpu.fpu, a0, b); // FUCOMPP
            m.cpu.fpu.pop();
            m.cpu.fpu.pop();
        }
        // --- DB: FCMOVNcc / 管理 / FUCOMI / FCOMI ---
        (0xDB, 0..=3) => {
            let take = match reg {
                0 => m.cpu.flags & CF == 0,
                1 => m.cpu.flags & ZF == 0,
                2 => m.cpu.flags & (CF | ZF) == 0,
                _ => m.cpu.flags & PF == 0,
            };
            if take {
                let v = m.cpu.fpu.st(i);
                m.cpu.fpu.set_st(0, v);
            }
        }
        (0xDB, 4) => match i {
            0 | 1 | 4 => {} // FNENI/FNDISI/FSETPM (287の遺物。何もしない)
            2 => {}         // FNCLEX (例外を持っていないので常に済んでいる)
            3 => {
                // FNINIT
                m.cpu.fpu.reset();
                m.cpu.fpu_cw = 0x037F;
            }
            _ => m.trap(format!("x87 DB E{i:X}")),
        },
        (0xDB, 5) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare_eflags(m, a0, b); // FUCOMI
        }
        (0xDB, 6) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare_eflags(m, a0, b); // FCOMI
        }
        // --- DC: st(i) が行き先 (SUB/DIVは向きが入れ替わる) ---
        (0xDC, _) => {
            let a0 = m.cpu.fpu.st(0);
            let b = m.cpu.fpu.st(i);
            match reg {
                0 => m.cpu.fpu.set_st(i, b + a0),
                1 => m.cpu.fpu.set_st(i, b * a0),
                2 => compare(&mut m.cpu.fpu, a0, b), // FCOM2 (別名)
                3 => {
                    compare(&mut m.cpu.fpu, a0, b);
                    m.cpu.fpu.pop();
                }
                4 => m.cpu.fpu.set_st(i, a0 - b), // FSUBR
                5 => m.cpu.fpu.set_st(i, b - a0), // FSUB
                6 => m.cpu.fpu.set_st(i, a0 / b), // FDIVR
                _ => m.cpu.fpu.set_st(i, b / a0), // FDIV
            }
        }
        // --- DD: FFREE / FST / FSTP / FUCOM ---
        (0xDD, 0) => {
            let p = m.cpu.fpu.phys(i);
            m.cpu.fpu.empty |= 1 << p; // FFREE
        }
        (0xDD, 1) => {
            // FXCH4 (別名)
            let a0 = m.cpu.fpu.st(0);
            let b = m.cpu.fpu.st(i);
            m.cpu.fpu.set_st(0, b);
            m.cpu.fpu.set_st(i, a0);
        }
        (0xDD, 2) => {
            let v = m.cpu.fpu.st(0);
            m.cpu.fpu.set_st(i, v);
        }
        (0xDD, 3) => {
            let v = m.cpu.fpu.st(0);
            m.cpu.fpu.set_st(i, v);
            m.cpu.fpu.pop();
        }
        (0xDD, 4) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare(&mut m.cpu.fpu, a0, b); // FUCOM
        }
        (0xDD, 5) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare(&mut m.cpu.fpu, a0, b); // FUCOMP
            m.cpu.fpu.pop();
        }
        // --- DE: 演算してpop / FCOMPP ---
        (0xDE, 3) if i == 1 => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(1));
            compare(&mut m.cpu.fpu, a0, b); // FCOMPP
            m.cpu.fpu.pop();
            m.cpu.fpu.pop();
        }
        (0xDE, _) => {
            let a0 = m.cpu.fpu.st(0);
            let b = m.cpu.fpu.st(i);
            match reg {
                0 => m.cpu.fpu.set_st(i, b + a0),
                1 => m.cpu.fpu.set_st(i, b * a0),
                2 => {
                    compare(&mut m.cpu.fpu, a0, b); // FCOMP5 (別名)
                }
                4 => m.cpu.fpu.set_st(i, a0 - b), // FSUBRP
                5 => m.cpu.fpu.set_st(i, b - a0), // FSUBP
                6 => m.cpu.fpu.set_st(i, a0 / b), // FDIVRP
                _ => m.cpu.fpu.set_st(i, b / a0), // FDIVP
            }
            m.cpu.fpu.pop();
        }
        // --- DF: FNSTSW AX / FUCOMIP / FCOMIP / FFREEP ---
        (0xDF, 4) if i == 0 => {
            let sw = m.cpu.fpu.status();
            m.cpu.set_reg16(AX, sw);
        }
        (0xDF, 0) => {
            let p = m.cpu.fpu.phys(i);
            m.cpu.fpu.empty |= 1 << p; // FFREEP
            m.cpu.fpu.pop();
        }
        (0xDF, 5) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare_eflags(m, a0, b); // FUCOMIP
            m.cpu.fpu.pop();
        }
        (0xDF, 6) => {
            let (a0, b) = (m.cpu.fpu.st(0), m.cpu.fpu.st(i));
            compare_eflags(m, a0, b); // FCOMIP
            m.cpu.fpu.pop();
        }
        _ => m.trap(format!("x87 reg op={op:02X} /{reg} st{i}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f80_roundtrip_keeps_f64_values() {
        for v in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            3.0,
            0.02,
            1e-300,
            1e300,
            1.5e-320, // 非正規化数も
            std::f64::consts::PI,
        ] {
            let (m, se) = f64_to_f80(v);
            let back = f80_to_f64(m, se);
            assert_eq!(back.to_bits(), v.to_bits(), "{v} が往復で化けた");
        }
        let (m, se) = f64_to_f80(f64::INFINITY);
        assert_eq!(f80_to_f64(m, se), f64::INFINITY);
        let (m, se) = f64_to_f80(f64::NAN);
        assert!(f80_to_f64(m, se).is_nan());
    }

    #[test]
    fn f80_literals_from_real_hardware() {
        // 実機のm80表現を読めること: 3.0 = 4000C000000000000000
        assert_eq!(f80_to_f64(0xC000_0000_0000_0000, 0x4000), 3.0);
        // 1.0 = 3FFF8000000000000000
        assert_eq!(f80_to_f64(0x8000_0000_0000_0000, 0x3FFF), 1.0);
        // 10.0 = 4002A000000000000000
        assert_eq!(f80_to_f64(0xA000_0000_0000_0000, 0x4002), 10.0);
    }

    #[test]
    fn scale2_covers_the_range() {
        assert_eq!(scale2(1.0, 10), 1024.0);
        assert_eq!(scale2(1.0, -10), 1.0 / 1024.0);
        assert_eq!(scale2(1.5, 0), 1.5);
        assert_eq!(scale2(1.0, 1050), f64::INFINITY); // 範囲切れは∞
    }
}
