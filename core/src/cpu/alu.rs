//! ALU (8種の演算) とフラグ計算。
//!
//! x86のフラグ意味論、特にAF (下位4bitからの桁上がり) とOF (符号付き
//! オーバーフロー) は境界値でしか姿を現さない。ここがバグの主産地なので
//! co-simで重点的に検証している。

use super::{Cpu, AF, CF, OF, PF, SF, ZF};

pub fn alu8(c: &mut Cpu, op: u8, a: u8, b: u8) -> u8 {
    let carry = c.flag(CF) as u16;
    let (r, cf, of, af) = match op {
        0 => {
            let r = a as u16 + b as u16;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) > 0xF)
        }
        1 => ((a | b) as u16, false, false, false),
        2 => {
            let r = a as u16 + b as u16 + carry;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) + carry as u8 > 0xF)
        }
        3 => {
            let r = (a as u16).wrapping_sub(b as u16).wrapping_sub(carry);
            (r, (a as u16) < b as u16 + carry, ((a ^ b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) < (b & 0xF) + carry as u8)
        }
        4 => ((a & b) as u16, false, false, false),
        5 | 7 => {
            let r = (a as u16).wrapping_sub(b as u16);
            (r, (a as u16) < b as u16, ((a ^ b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) < (b & 0xF))
        }
        _ => ((a ^ b) as u16, false, false, false), // 6 = XOR
    };
    let r8 = r as u8;
    c.set_flag(CF, cf);
    c.set_flag(OF, of);
    c.set_flag(AF, af);
    set_szp8(c, r8);
    if op == 7 { a } else { r8 } // CMPは結果を書き戻さない
}

pub fn alu16(c: &mut Cpu, op: u8, a: u16, b: u16) -> u16 {
    let carry = c.flag(CF) as u32;
    let (r, cf, of, af) = match op {
        0 => {
            let r = a as u32 + b as u32;
            (r, r > 0xFFFF, ((a ^ !b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) + (b & 0xF) > 0xF)
        }
        1 => ((a | b) as u32, false, false, false),
        2 => {
            let r = a as u32 + b as u32 + carry;
            (r, r > 0xFFFF, ((a ^ !b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) + (b & 0xF) + carry as u16 > 0xF)
        }
        3 => {
            let r = (a as u32).wrapping_sub(b as u32).wrapping_sub(carry);
            (r, (a as u32) < b as u32 + carry, ((a ^ b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) < (b & 0xF) + carry as u16)
        }
        4 => ((a & b) as u32, false, false, false),
        5 | 7 => {
            let r = (a as u32).wrapping_sub(b as u32);
            (r, (a as u32) < b as u32, ((a ^ b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) < (b & 0xF))
        }
        _ => ((a ^ b) as u32, false, false, false),
    };
    let r16 = r as u16;
    c.set_flag(CF, cf);
    c.set_flag(OF, of);
    c.set_flag(AF, af);
    set_szp16(c, r16);
    if op == 7 { a } else { r16 }
}

pub fn set_szp8(c: &mut Cpu, v: u8) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x80 != 0);
    c.set_flag(PF, v.count_ones() % 2 == 0);
}

pub fn set_szp16(c: &mut Cpu, v: u16) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x8000 != 0);
    c.set_flag(PF, (v as u8).count_ones() % 2 == 0); // PFは下位8bitのみ
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
    if cc & 1 != 0 { !r } else { r }
}
