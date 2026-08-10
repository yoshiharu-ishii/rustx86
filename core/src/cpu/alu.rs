//! ALU (8種の演算) とフラグ計算。
//!
//! x86のフラグ意味論、特にAF (下位4bitからの桁上がり) とOF (符号付き
//! オーバーフロー) は境界値でしか姿を現さない。ここがバグの主産地なので
//! co-simで重点的に検証している。

use super::{Cpu, CF, OF, PF, SF, ZF};

/// ALU本体 — 幅を問わない共通部。**フラグはここでは計算しない**。
///
/// 演算の材料 (op, a, b, キャリー入力, 結果) を [`Cpu::set_cc`] に控えるだけで、
/// フラグは読まれた瞬間に cpu/mod.rs の cc_* が計算する (遅延評価)。
/// フラグの式そのものは cc_cf/cc_of/cc_af に移った — 意味論の原本はそちら。
#[inline]
fn alu_lazy(c: &mut Cpu, op: u8, a: u32, b: u32, w: u8) -> u32 {
    let mask = (1u64 << (8 << w)) - 1;
    let cin = match op {
        2 | 3 => c.flag(CF) as u32, // ADC/SBBだけ前のCFを食う (遅延中なら1bitだけ計算)
        _ => 0,
    };
    let r = match op {
        0 | 2 => (a as u64 + b as u64 + cin as u64) & mask,
        1 => (a | b) as u64,
        3 | 5 | 7 => (a as u64)
            .wrapping_sub(b as u64)
            .wrapping_sub(cin as u64)
            & mask,
        4 => (a & b) as u64,
        _ => (a ^ b) as u64, // 6 = XOR
    } as u32;
    c.set_cc(op, w, a, b, cin, r);
    if op == 7 {
        a
    } else {
        r
    } // CMPは結果を書き戻さない
}

pub fn alu8(c: &mut Cpu, op: u8, a: u8, b: u8) -> u8 {
    alu_lazy(c, op, a as u32, b as u32, 0) as u8
}

pub fn alu16(c: &mut Cpu, op: u8, a: u16, b: u16) -> u16 {
    alu_lazy(c, op, a as u32, b as u32, 1) as u16
}

/// 32bit版。`0x66` プレフィクスが付いたときの演算。
///
/// **AFは下位4bitの桁上がりなので幅が変わっても同じ**、CFとOFだけが幅で変わる。
/// 8/16/32で構造が完全に平行しているのは偶然ではなく、
/// 386が16bitの意味論をそのまま広げる形で拡張されたからである。
pub fn alu32(c: &mut Cpu, op: u8, a: u32, b: u32) -> u32 {
    alu_lazy(c, op, a, b, 2)
}

/// INC/DEC (8bit)。ADD/SUBと違い**CFだけは触らない** — 多倍長加算ループの
/// キャリーを壊さないための8086以来の配慮。遅延側では CC_INC/CC_DEC
pub fn inc_dec8(c: &mut Cpu, a: u8, dec: bool) -> u8 {
    let r = if dec {
        a.wrapping_sub(1)
    } else {
        a.wrapping_add(1)
    };
    c.set_cc_incdec(if dec { super::CC_DEC } else { super::CC_INC }, 0, a as u32, r as u32);
    r
}

/// INC/DEC (16/32bit、幅は実行時に選ぶ)
pub fn inc_dec_w(c: &mut Cpu, a: u32, dec: bool, wide: bool) -> u32 {
    let (w, mask) = if wide { (2, 0xFFFF_FFFF) } else { (1, 0xFFFF) };
    let a = a & mask;
    let r = if dec {
        a.wrapping_sub(1)
    } else {
        a.wrapping_add(1)
    } & mask;
    c.set_cc_incdec(if dec { super::CC_DEC } else { super::CC_INC }, w, a, r);
    r
}

/// 幅を実行時に選ぶALU。`0x66` の有無で16bitと32bitを切り替える。
///
/// **呼ぶ側に同じ形のコードを2本書かせない**ための入口である。
/// 分岐表 (`cpu/mod.rs`) が幅ごとに倍に膨れるのを防ぐ。
pub fn alu_w(c: &mut Cpu, op: u8, a: u32, b: u32, wide: bool) -> u32 {
    if wide {
        alu32(c, op, a, b)
    } else {
        alu16(c, op, a as u16, b as u16) as u32
    }
}

pub fn set_szp8(c: &mut Cpu, v: u8) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x80 != 0);
    c.set_flag(PF, v.count_ones().is_multiple_of(2));
}

pub fn set_szp16(c: &mut Cpu, v: u16) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x8000 != 0);
    c.set_flag(PF, (v as u8).count_ones().is_multiple_of(2)); // PFは下位8bitのみ
}

pub fn set_szp32(c: &mut Cpu, v: u32) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x8000_0000 != 0);
    c.set_flag(PF, (v as u8).count_ones().is_multiple_of(2)); // PFは幅によらず下位8bitのみ
}

/// 幅を実行時に選ぶ SF/ZF/PF の更新
pub fn set_szp_w(c: &mut Cpu, v: u32, wide: bool) {
    if wide {
        set_szp32(c, v)
    } else {
        set_szp16(c, v as u16)
    }
}

pub fn condition(c: &Cpu, cc: u8) -> bool {
    let r = match cc >> 1 {
        0 => c.flag(OF),
        1 => c.flag(CF),
        2 => c.flag(ZF),
        3 => c.flag(CF) || c.flag(ZF),
        4 => c.flag(SF),
        5 => c.flag(PF),
        6 => c.flag(SF) != c.flag(OF),
        _ => c.flag(ZF) || (c.flag(SF) != c.flag(OF)),
    };
    if cc & 1 != 0 {
        !r
    } else {
        r
    }
}
