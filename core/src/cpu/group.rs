//! グループ命令 — **1つのオペコードを ModRM の reg 欄で再分岐**する族。
//!
//! 0xF6/0xF7 (GRP3) は同じオペコードから TEST/NOT/NEG/MUL/IMUL/DIV/IDIV の
//! どれかへ、0xFE/0xFF (GRP4/5) は INC/DEC/CALL/JMP/PUSH へ枝分かれする。
//! 「オペコードのビットで演算が決まる」実CPUのデコード構造そのもの。
//! (GRP1 = 0x80-0x83 のALU r/m,imm は ALU 族の隣に置いたまま)

use super::operand::{
    fetch16, fetch8, modrm, push16, read_op16, read_op8, read_op_w, write_op16, write_op8, Operand,
};
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
        let a = read_op16(m, &rm) as u32;
        let r = shift_rot(&mut m.cpu, kind as u8, a, count, 16);
        write_op16(m, &rm, r as u16);
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

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp3_word(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let (kind, rm) = modrm(m, d);
    let a = read_op16(m, &rm);
    match kind {
        0 | 1 => {
            let b = fetch16(m);
            alu16(&mut m.cpu, 4, a, b);
        }
        2 => write_op16(m, &rm, !a),
        3 => {
            let r = alu16(&mut m.cpu, 5, 0, a);
            m.cpu.set_flag(CF, a != 0);
            write_op16(m, &rm, r);
        }
        4 => {
            let r = m.cpu.reg16(AX) as u32 * a as u32;
            m.cpu.set_reg16(AX, r as u16);
            m.cpu.set_reg16(DX, (r >> 16) as u16);
            let hi = r >> 16 != 0;
            m.cpu.set_flag(CF, hi);
            m.cpu.set_flag(OF, hi);
        }
        5 => {
            let r = (m.cpu.reg16(AX) as i16 as i32) * (a as i16 as i32);
            m.cpu.set_reg16(AX, r as u16);
            m.cpu.set_reg16(DX, (r >> 16) as u16);
            let ext = (r as i16 as i32) != r;
            m.cpu.set_flag(CF, ext);
            m.cpu.set_flag(OF, ext);
        }
        6 => {
            let n = ((m.cpu.reg16(DX) as u32) << 16) | m.cpu.reg16(AX) as u32;
            if a == 0 {
                return divide_error(m, start_ip);
            }
            let q = n / a as u32;
            if q > 0xFFFF {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg16(AX, q as u16);
            m.cpu.set_reg16(DX, (n % a as u32) as u16);
        }
        _ => {
            let n = (((m.cpu.reg16(DX) as u32) << 16) | m.cpu.reg16(AX) as u32) as i32;
            let b = a as i16 as i32;
            if b == 0 {
                return divide_error(m, start_ip);
            }
            let q = n / b;
            if !(-32768..=32767).contains(&q) {
                return divide_error(m, start_ip);
            }
            m.cpu.set_reg16(AX, q as u16);
            m.cpu.set_reg16(DX, (n % b) as u16);
        }
    }
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp4(m: &mut Machine, d: &Decoder) {
    let (kind, rm) = modrm(m, d);
    let a = read_op8(m, &rm);
    let cf = m.cpu.flag(CF);
    let r = alu8(&mut m.cpu, if kind == 0 { 0 } else { 5 }, a, 1);
    m.cpu.set_flag(CF, cf); // INC/DECはCFを変更しない
    write_op8(m, &rm, r);
}

/// GRP: ModRMのreg欄が演算を選ぶ命令群
pub(crate) fn grp5(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let (kind, rm) = modrm(m, d);
    match kind {
        0 | 1 => {
            let a = read_op16(m, &rm);
            let cf = m.cpu.flag(CF);
            let r = alu16(&mut m.cpu, if kind == 0 { 0 } else { 5 }, a, 1);
            m.cpu.set_flag(CF, cf);
            write_op16(m, &rm, r);
        }
        2 => {
            let t = read_op_w(m, &rm, d.opsize32);
            let ret = m.cpu.ip;
            push_w(m, ret, d.opsize32);
            m.cpu.set_ip(t);
        }
        4 => {
            let t = read_op_w(m, &rm, d.opsize32);
            m.cpu.set_ip(t);
        }
        6 => {
            let v = read_op16(m, &rm);
            push16(m, v);
        }
        // /3 CALL far、/5 JMP far: メモリ上の4バイト far ポインタを読んで飛ぶ
        3 | 5 => {
            let addr = match rm {
                Operand::Mem { addr, .. } => addr,
                Operand::Reg(_) => panic!("far call/jmp with register operand"),
            };
            let off = m.read16(addr);
            let seg = m.read16(addr.wrapping_add(2));
            if kind == 3 {
                let cs = m.cpu.sregs[CS];
                push16(m, cs);
                let ret = m.cpu.ip as u16;
                push16(m, ret);
            }
            m.cpu.sregs[CS] = seg;
            m.cpu.set_ip(off as u32);
        }
        _ => panic!(
            "GRP5 /{kind} not implemented at {:04x}:{:04x}",
            m.cpu.sregs[CS], start_ip
        ),
    }
}
