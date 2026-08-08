//! ストリング命令 (MOVS/CMPS/STOS/LODS/SCAS) とREPプレフィクス。
//!
//! REP付きはCXが尽きるまで自前でループする。CMPS/SCASはZFを見て
//! 打ち切る (REPE/REPNE) ため、単純な繰り返しにはできない。

use super::alu::{alu16, alu8};
use super::{Cpu, Decoder, AX, CX, DF, DI, DS, ES, SI, ZF};
use crate::Machine;

/// ストリング命令のインデックス更新量 (DF方向)
pub fn str_delta(c: &Cpu, size: u16) -> u16 {
    if c.flag(DF) {
        size.wrapping_neg()
    } else {
        size
    }
}

/// ストリング命令1個を実行する (REPがあればCXが尽きるまで繰り返す)
pub fn exec(m: &mut Machine, d: &Decoder, op: u8) {
    let word = op & 1 != 0;
    let size = if word { 2 } else { 1 };
    loop {
        if d.rep.is_some() && m.cpu.reg16(CX) == 0 {
            break;
        }
        let src_seg = d.seg_override.unwrap_or(DS);
        let si = m.cpu.reg16(SI);
        let di = m.cpu.reg16(DI);
        match op {
            0xA4 | 0xA5 => {
                // MOVS
                if word {
                    let v = m.read16(m.cpu.lin(src_seg, si as u32));
                    m.write16(m.cpu.lin(ES, di as u32), v);
                } else {
                    let v = m.read8(m.cpu.lin(src_seg, si as u32));
                    m.write8(m.cpu.lin(ES, di as u32), v);
                }
                let dl = str_delta(&m.cpu, size);
                m.cpu.set_reg16(SI, si.wrapping_add(dl));
                m.cpu.set_reg16(DI, di.wrapping_add(dl));
            }
            0xA6 | 0xA7 => {
                // CMPS
                if word {
                    let a = m.read16(m.cpu.lin(src_seg, si as u32));
                    let b = m.read16(m.cpu.lin(ES, di as u32));
                    alu16(&mut m.cpu, 7, a, b);
                } else {
                    let a = m.read8(m.cpu.lin(src_seg, si as u32));
                    let b = m.read8(m.cpu.lin(ES, di as u32));
                    alu8(&mut m.cpu, 7, a, b);
                }
                let dl = str_delta(&m.cpu, size);
                m.cpu.set_reg16(SI, si.wrapping_add(dl));
                m.cpu.set_reg16(DI, di.wrapping_add(dl));
            }
            0xAA | 0xAB => {
                // STOS
                if word {
                    let v = m.cpu.reg16(AX);
                    m.write16(m.cpu.lin(ES, di as u32), v);
                } else {
                    let v = m.cpu.reg8(0);
                    m.write8(m.cpu.lin(ES, di as u32), v);
                }
                let dl = str_delta(&m.cpu, size);
                m.cpu.set_reg16(DI, di.wrapping_add(dl));
            }
            0xAC | 0xAD => {
                // LODS
                if word {
                    let v = m.read16(m.cpu.lin(src_seg, si as u32));
                    m.cpu.set_reg16(AX, v);
                } else {
                    let v = m.read8(m.cpu.lin(src_seg, si as u32));
                    m.cpu.set_reg8(0, v);
                }
                let dl = str_delta(&m.cpu, size);
                m.cpu.set_reg16(SI, si.wrapping_add(dl));
            }
            _ => {
                // SCAS
                if word {
                    let a = m.cpu.reg16(AX);
                    let b = m.read16(m.cpu.lin(ES, di as u32));
                    alu16(&mut m.cpu, 7, a, b);
                } else {
                    let a = m.cpu.reg8(0);
                    let b = m.read8(m.cpu.lin(ES, di as u32));
                    alu8(&mut m.cpu, 7, a, b);
                }
                let dl = str_delta(&m.cpu, size);
                m.cpu.set_reg16(DI, di.wrapping_add(dl));
            }
        }
        match d.rep {
            None => break,
            Some(prefix) => {
                let cx = m.cpu.reg16(CX).wrapping_sub(1);
                m.cpu.set_reg16(CX, cx);
                // REPE(F3)/REPNE(F2) はCMPS/SCASでZFを見て打ち切る
                if matches!(op, 0xA6 | 0xA7 | 0xAE | 0xAF) {
                    let zf = m.cpu.flag(ZF);
                    let want = prefix == 0xF3;
                    if zf != want {
                        break;
                    }
                }
                if cx == 0 {
                    break;
                }
            }
        }
    }
}
