//! グループ命令 — **1つのオペコードを ModRM の reg 欄で再分岐**する族。
//!
//! 0xF6/0xF7 (GRP3) は同じオペコードから TEST/NOT/NEG/MUL/IMUL/DIV/IDIV の
//! どれかへ、0xFE/0xFF (GRP4/5) は INC/DEC/CALL/JMP/PUSH へ枝分かれする。
//! 「オペコードのビットで演算が決まる」実CPUのデコード構造そのもの。
//! (GRP1 = 0x80-0x83 のALU r/m,imm は ALU 族の隣に置いたまま)

use super::operand::{fetch8, modrm, read_op8, read_op_w, write_op8, Operand};
use super::*;
use crate::Machine;

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp2(m: &mut Machine, d: &Decoder, op: u8) {
    let (kind, rm) = modrm(m, d);
    let count = match op {
        0xC0 | 0xC1 => fetch8(m),
        0xD0 | 0xD1 => 1,
        _ => m.cpu.reg8(1), // CL
    };
    if op & 1 == 0 {
        let a = read_op8(m, &rm) as u32;
        let r = shift_rot(&mut m.cpu, kind as u8, a, count, 8);
        write_op8(m, &rm, r as u8);
    } else {
        let w = d.opsize32;
        let a = read_op_w(m, &rm, w);
        let r = shift_rot(&mut m.cpu, kind as u8, a, count, if w { 32 } else { 16 });
        write_op_w(m, &rm, r, w);
    }
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp3_byte(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let (kind, rm) = modrm(m, d);
    let a = read_op8(m, &rm);
    match kind {
        0 | 1 => {
            let b = fetch8(m);
            alu8(&mut m.cpu, 4, a, b);
        }
        2 => write_op8(m, &rm, !a),
        3 => {
            let r = alu8(&mut m.cpu, 5, 0, a);
            m.cpu.set_flag(CF, a != 0);
            write_op8(m, &rm, r);
        }
        4 => {
            let r = m.cpu.reg8(0) as u16 * a as u16;
            m.cpu.set_reg16(AX, r);
            let hi = r >> 8 != 0;
            m.cpu.set_flag(CF, hi);
            m.cpu.set_flag(OF, hi);
        }
        5 => {
            let r = (m.cpu.reg8(0) as i8 as i16) * (a as i8 as i16);
            m.cpu.set_reg16(AX, r as u16);
            let ext = (r as i8 as i16) != r;
            m.cpu.set_flag(CF, ext);
            m.cpu.set_flag(OF, ext);
        }
        6 => {
            let ax = m.cpu.reg16(AX);
            if a == 0 {
                return divide_error(m, start_ip);
            }
            let q = ax / a as u16;
            if q > 0xFF {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg8(0, q as u8);
            m.cpu.set_reg8(4, (ax % a as u16) as u8);
        }
        _ => {
            let ax = m.cpu.reg16(AX) as i16;
            let b = a as i8 as i16;
            if b == 0 {
                return divide_error(m, start_ip);
            }
            let q = ax / b;
            if !(-128..=127).contains(&q) {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg8(0, q as u8);
            m.cpu.set_reg8(4, (ax % b) as u8);
        }
    }
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群。
/// 幅はオペランドサイズ — 32bitでは積も被除数も **DX:AXではなくEDX:EAX** になる
pub(crate) fn grp3_word(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let w = d.opsize32;
    let (kind, rm) = modrm(m, d);
    let a = read_op_w(m, &rm, w);
    match kind {
        0 | 1 => {
            let b = fetch_w(m, w);
            alu_w(&mut m.cpu, 4, a, b, w);
        }
        2 => {
            let r = if w { !a } else { (!a) & 0xFFFF };
            write_op_w(m, &rm, r, w);
        }
        3 => {
            let r = alu_w(&mut m.cpu, 5, 0, a, w);
            m.cpu.set_flag(CF, a != 0);
            write_op_w(m, &rm, r, w);
        }
        4 => {
            // MUL: (E)AX × r/m → 上位は (E)DX へ
            let r = m.cpu.reg_w(AX, w) as u64 * a as u64;
            let bits = if w { 32 } else { 16 };
            m.cpu.set_reg_w(AX, r as u32, w);
            m.cpu.set_reg_w(DX, (r >> bits) as u32, w);
            let hi = r >> bits != 0;
            m.cpu.set_flag(CF, hi);
            m.cpu.set_flag(OF, hi);
        }
        5 => {
            // IMUL (1オペランド形)
            let (r, ext) = if w {
                let r = (m.cpu.reg_w(AX, true) as i32 as i64) * (a as i32 as i64);
                (r as u64, (r as i32 as i64) != r)
            } else {
                let r = (m.cpu.reg16(AX) as i16 as i32) * (a as i16 as i32);
                (r as u32 as u64, (r as i16 as i32) != r)
            };
            let bits = if w { 32 } else { 16 };
            m.cpu.set_reg_w(AX, r as u32, w);
            m.cpu.set_reg_w(DX, (r >> bits) as u32, w);
            m.cpu.set_flag(CF, ext);
            m.cpu.set_flag(OF, ext);
        }
        6 => {
            // DIV: (E)DX:(E)AX ÷ r/m
            let bits = if w { 32 } else { 16 };
            let n = ((m.cpu.reg_w(DX, w) as u64) << bits) | m.cpu.reg_w(AX, w) as u64;
            if a == 0 {
                return divide_error(m, start_ip);
            }
            let q = n / a as u64;
            let max = if w { 0xFFFF_FFFF } else { 0xFFFF };
            if q > max {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg_w(AX, q as u32, w);
            m.cpu.set_reg_w(DX, (n % a as u64) as u32, w);
        }
        _ => {
            // IDIV
            let bits = if w { 32 } else { 16 };
            let n = (((m.cpu.reg_w(DX, w) as u64) << bits) | m.cpu.reg_w(AX, w) as u64) as i64;
            let n = if w { n } else { n as i32 as i64 };
            let b = if w { a as i32 as i64 } else { a as i16 as i64 };
            if b == 0 {
                return divide_error(m, start_ip);
            }
            let q = n / b;
            let (lo, hi) = if w {
                (i32::MIN as i64, i32::MAX as i64)
            } else {
                (-32768, 32767)
            };
            if !(lo..=hi).contains(&q) {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg_w(AX, q as u32, w);
            m.cpu.set_reg_w(DX, (n % b) as u32, w);
        }
    }
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp4(m: &mut Machine, d: &Decoder) {
    let (kind, rm) = modrm(m, d);
    let a = read_op8(m, &rm);
    let r = super::alu::inc_dec8(&mut m.cpu, a, kind != 0); // INC/DECはCFを変更しない
    write_op8(m, &rm, r);
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp5(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let (kind, rm) = modrm(m, d);
    let w = d.opsize32;
    match kind {
        0 | 1 => {
            let a = read_op_w(m, &rm, w);
            let r = super::alu::inc_dec_w(&mut m.cpu, a, kind != 0, w);
            write_op_w(m, &rm, r, w);
        }
        2 => {
            let t = read_op_w(m, &rm, w);
            let ret = m.cpu.ip;
            push_w(m, ret, w);
            m.cpu.set_ip(t);
        }
        4 => {
            let t = read_op_w(m, &rm, w);
            m.cpu.set_ip(t);
        }
        6 => {
            let v = read_op_w(m, &rm, w);
            push_w(m, v, w);
        }
        // /3 CALL far、/5 JMP far: メモリ上の far ポインタを読んで飛ぶ。
        // オフセットの幅はオペランドサイズ (16bit=4バイト、32bit=6バイト)
        3 | 5 => {
            let addr = match rm {
                Operand::Mem { addr, .. } => addr,
                Operand::Reg(_) => {
                    m.trap("far call/jmp with register operand".into());
                    return;
                }
            };
            let (off, seg) = if w {
                (m.read32(addr), m.read16(addr.wrapping_add(4)))
            } else {
                (m.read16(addr) as u32, m.read16(addr.wrapping_add(2)))
            };
            if kind == 3 {
                // CALL far はコールゲート経由のリング遷移になり得る — 共通経路へ
                super::segment::far_call(m, seg, off, w);
            } else {
                super::load_seg(m, CS, seg);
                m.cpu.set_ip(off);
            }
        }
        _ => {
            let _ = start_ip;
            m.trap(format!("GRP5 /{kind} (undefined encoding)"));
        }
    }
}
