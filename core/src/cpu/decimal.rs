//! 十進補正 (BCD演算の補助)。
//!
//! 8080時代から受け継がれた命令群。ALの値とCF/AFだけで分岐が決まるため
//! 状態空間が小さく、co-simでは総当たり検証している (ランダムでは
//! ALがちょうど0x9Aになるような境界値を踏み損ねる)。

use super::alu::set_szp8;
use super::operand::fetch8;
use super::{AF, AX, CF};
use crate::Machine;

/// DAA (op=0x27) / DAS (op=0x2F): 加減算結果をBCDに補正する
pub fn daa_das(m: &mut Machine, op: u8) {
    let old_al = m.cpu.reg8(0);
    let old_cf = m.cpu.flag(CF);
    let sub = op == 0x2F;
    let mut al = old_al;
    let mut cf = false;
    if al & 0x0F > 9 || m.cpu.flag(AF) {
        al = if sub {
            al.wrapping_sub(6)
        } else {
            al.wrapping_add(6)
        };
        cf = old_cf || if sub { old_al < 6 } else { al < old_al };
        m.cpu.set_flag(AF, true);
    } else {
        m.cpu.set_flag(AF, false);
    }
    if old_al > 0x99 || old_cf {
        al = if sub {
            al.wrapping_sub(0x60)
        } else {
            al.wrapping_add(0x60)
        };
        cf = true;
    }
    m.cpu.set_reg8(0, al);
    m.cpu.set_flag(CF, cf);
    set_szp8(&mut m.cpu, al);
}

/// AAA (op=0x37) / AAS (op=0x3F): アンパックBCDの補正
pub fn aaa_aas(m: &mut Machine, op: u8) {
    let al = m.cpu.reg8(0);
    let sub = op == 0x3F;
    if al & 0x0F > 9 || m.cpu.flag(AF) {
        let ax = m.cpu.reg16(AX);
        let ax = if sub {
            ax.wrapping_sub(6)
        } else {
            ax.wrapping_add(6)
        };
        m.cpu.set_reg16(AX, ax);
        let ah = m.cpu.reg8(4);
        m.cpu.set_reg8(
            4,
            if sub {
                ah.wrapping_sub(1)
            } else {
                ah.wrapping_add(1)
            },
        );
        m.cpu.set_flag(AF, true);
        m.cpu.set_flag(CF, true);
    } else {
        m.cpu.set_flag(AF, false);
        m.cpu.set_flag(CF, false);
    }
    let al = m.cpu.reg8(0) & 0x0F;
    m.cpu.set_reg8(0, al);
}

/// AAM: 乗算後の補正 (ALを基数で割る)
pub fn aam(m: &mut Machine) {
    let base = fetch8(m);
    if base == 0 {
        panic!("AAM by zero");
    }
    let al = m.cpu.reg8(0);
    m.cpu.set_reg8(4, al / base);
    let r = al % base;
    m.cpu.set_reg8(0, r);
    set_szp8(&mut m.cpu, r);
}

/// AAD: 除算前の補正 (AH*基数 + AL をALにまとめる)
pub fn aad(m: &mut Machine) {
    let base = fetch8(m);
    let r = m.cpu.reg8(0).wrapping_add(m.cpu.reg8(4).wrapping_mul(base));
    m.cpu.set_reg8(0, r);
    m.cpu.set_reg8(4, 0);
    set_szp8(&mut m.cpu, r);
}
