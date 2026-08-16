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
        3 | 5 | 7 => (a as u64).wrapping_sub(b as u64).wrapping_sub(cin as u64) & mask,
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
    c.set_cc_incdec(
        if dec { super::CC_DEC } else { super::CC_INC },
        0,
        a as u32,
        r as u32,
    );
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

/// jcc/setcc の条件判定。**遅延の有無を先に1回だけ裁く** (C13) —
/// 以前は flag() を条件ごとに1〜3回呼び、そのたびに cc_op==CC_NONE の
/// 判定をやり直していた。jcc の結果は次のIPを作る鎖の上に居るので、
/// ここの縦深はそのまま反復の縦深になる。個々の式は flag() の遅延側と
/// 逐語同一 (意味論の原本は cc_cf/cc_of — 二重実装ではなく展開)
pub fn condition(c: &Cpu, cc: u8) -> bool {
    let r = if c.cc_op == super::CC_NONE {
        // eager: flags フィールドが6フラグ含めて真実
        let f = c.flags;
        match cc >> 1 {
            0 => f & OF != 0,
            1 => f & CF != 0,
            2 => f & ZF != 0,
            3 => f & (CF | ZF) != 0,
            4 => f & SF != 0,
            5 => f & PF != 0,
            6 => (f & SF != 0) != (f & OF != 0),
            _ => f & ZF != 0 || ((f & SF != 0) != (f & OF != 0)),
        }
    } else {
        // lazy: 材料から必要なビットだけ (flag()の遅延側と同じ式)
        match cc >> 1 {
            0 => c.cc_of(),
            1 => c.cc_cf(),
            2 => c.cc_r == 0,
            3 => c.cc_cf() || c.cc_r == 0,
            4 => c.cc_r & c.cc_sign() != 0,
            5 => (c.cc_r as u8).count_ones().is_multiple_of(2),
            6 => (c.cc_r & c.cc_sign() != 0) != c.cc_of(),
            _ => c.cc_r == 0 || ((c.cc_r & c.cc_sign() != 0) != c.cc_of()),
        }
    };
    if cc & 1 != 0 {
        !r
    } else {
        r
    }
}
