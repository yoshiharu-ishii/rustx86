//! オペランドの解決とアクセス。
//!
//! x86の命令はModRMバイトで「レジスタかメモリか」「どのアドレッシングか」を
//! 指定する。ここはその解読と、解決済みオペランドへの読み書きを担う。

use super::{Decoder, BP, BX, CS, DI, DS, SI, SP, SS};
use crate::Machine;

pub fn linear(seg: u16, off: u16) -> u32 {
    ((seg as u32) << 4).wrapping_add(off as u32) & 0xF_FFFF
}

/// ModRMのデコード結果
pub enum Operand {
    Reg(usize),
    /// addr = セグメント適用後のリニアアドレス、off = セグメント内オフセット (LEA用)
    Mem { addr: u32, off: u16 },
}

pub fn fetch8(m: &mut Machine) -> u8 {
    let v = m.read8(linear(m.cpu.sregs[CS], m.cpu.ip));
    m.cpu.ip = m.cpu.ip.wrapping_add(1);
    v
}

pub fn fetch16(m: &mut Machine) -> u16 {
    let lo = fetch8(m) as u16;
    let hi = fetch8(m) as u16;
    hi << 8 | lo
}

/// ModRMバイトを読み、(reg番号, 実効オペランド) を返す (16bitアドレッシング)
pub fn modrm(m: &mut Machine, d: &Decoder) -> (usize, Operand) {
    let b = fetch8(m);
    let md = b >> 6;
    let reg = ((b >> 3) & 7) as usize;
    let rm = (b & 7) as usize;
    if md == 3 {
        return (reg, Operand::Reg(rm));
    }
    let c = &m.cpu;
    // 16bit実効アドレスの基底 (rm=6かつmod=0はdisp16直接)
    let (base, default_seg) = match rm {
        0 => (c.reg16(BX).wrapping_add(c.reg16(SI)), DS),
        1 => (c.reg16(BX).wrapping_add(c.reg16(DI)), DS),
        2 => (c.reg16(BP).wrapping_add(c.reg16(SI)), SS),
        3 => (c.reg16(BP).wrapping_add(c.reg16(DI)), SS),
        4 => (c.reg16(SI), DS),
        5 => (c.reg16(DI), DS),
        6 => {
            if md == 0 {
                (0, DS) // disp16のみ
            } else {
                (c.reg16(BP), SS)
            }
        }
        _ => (c.reg16(BX), DS),
    };
    let disp = match md {
        0 => {
            if rm == 6 {
                fetch16(m)
            } else {
                0
            }
        }
        1 => fetch8(m) as i8 as u16,
        _ => fetch16(m),
    };
    let off = base.wrapping_add(disp);
    let seg = m.cpu.sregs[d.seg_override.unwrap_or(default_seg)];
    (reg, Operand::Mem { addr: linear(seg, off), off })
}

pub fn read_op8(m: &Machine, op: &Operand) -> u8 {
    match *op {
        Operand::Reg(r) => m.cpu.reg8(r),
        Operand::Mem { addr, .. } => m.read8(addr),
    }
}

pub fn write_op8(m: &mut Machine, op: &Operand, v: u8) {
    match *op {
        Operand::Reg(r) => m.cpu.set_reg8(r, v),
        Operand::Mem { addr, .. } => m.write8(addr, v),
    }
}

pub fn read_op16(m: &Machine, op: &Operand) -> u16 {
    match *op {
        Operand::Reg(r) => m.cpu.reg16(r),
        Operand::Mem { addr, .. } => m.read16(addr),
    }
}

pub fn write_op16(m: &mut Machine, op: &Operand, v: u16) {
    match *op {
        Operand::Reg(r) => m.cpu.set_reg16(r, v),
        Operand::Mem { addr, .. } => m.write16(addr, v),
    }
}

pub fn push16(m: &mut Machine, v: u16) {
    let sp = m.cpu.reg16(SP).wrapping_sub(2);
    m.cpu.set_reg16(SP, sp);
    let addr = linear(m.cpu.sregs[SS], sp);
    m.write16(addr, v);
}

pub fn pop16(m: &mut Machine) -> u16 {
    let sp = m.cpu.reg16(SP);
    let v = m.read16(linear(m.cpu.sregs[SS], sp));
    m.cpu.set_reg16(SP, sp.wrapping_add(2));
    v
}

