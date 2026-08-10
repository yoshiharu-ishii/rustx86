//! 実行器 — デコード済みの [`Uop`] を実行する。
//!
//! **意味論を二重実装しない**: フラグも演算も従来経路と同じヘルパ
//! (alu8/alu_w/shift_rot/condition/push_w/pop_w、string::exec) を呼ぶ。
//! ここにあるのは「オペランドの取り回し」だけである。

use super::super::alu::{alu8, alu_w, condition, set_szp_w};
use super::super::operand::{pop_w, push_w};
use super::super::shift::shift_rot;
use super::super::{sp_write, string, Decoder, AF, AX, BP, CF, OF};
use super::{MemRef, Rm, Uop};
use crate::Machine;

/// このuopがメモリ (線形アドレス) に触り得るか。
///
/// **#PF巻き戻しの控え (guard_save) を省いてよいかの判定**なので、
/// 迷ったら true に倒す (保守的に正しく)。フェッチは対象外 —
/// キャッシュ済み命令はページ内で完結し、番地は実行前に変換済み。
pub(super) fn may_touch_memory(u: &Uop) -> bool {
    let mem = |rm: &Rm| matches!(rm, Rm::Mem(_));
    match u {
        // レジスタとフラグしか触らない組 — #PFは起き得ない
        Uop::MovRImm { .. }
        | Uop::Lea { .. }
        | Uop::Jcc { .. }
        | Uop::JmpRel { .. }
        | Uop::IncR { .. }
        | Uop::DecR { .. }
        | Uop::XchgAR { .. }
        | Uop::AluAImm { .. }
        | Uop::Alu8AImm { .. } => false,
        // r/m形: メモリオペランドのときだけ
        Uop::MovRmR { rm, .. }
        | Uop::MovRRm { rm, .. }
        | Uop::Mov8RmR { rm, .. }
        | Uop::Mov8RRm { rm, .. }
        | Uop::AluRmR { rm, .. }
        | Uop::AluRRm { rm, .. }
        | Uop::Alu8RmR { rm, .. }
        | Uop::Alu8RRm { rm, .. }
        | Uop::Grp1RmImm { rm, .. }
        | Uop::Grp18RmImm { rm, .. }
        | Uop::TestRmR { rm, .. }
        | Uop::Test8RmR { rm, .. }
        | Uop::MovRmImm { rm, .. }
        | Uop::MovRm8Imm { rm, .. }
        | Uop::Grp3b { rm, .. }
        | Uop::Grp3w { rm, .. }
        | Uop::SetCC { rm, .. }
        | Uop::MovzxB { rm, .. }
        | Uop::MovzxW { rm, .. }
        | Uop::ImulRRm { rm, .. }
        | Uop::ShiftRmImm { rm, .. }
        | Uop::ShiftRmCl { rm, .. } => mem(rm),
        // スタック・moffs・ストリング・grp5 (push/call間接等) は常にメモリ
        Uop::CallRel { .. }
        | Uop::Ret
        | Uop::PushR { .. }
        | Uop::PopR { .. }
        | Uop::PushImm { .. }
        | Uop::Leave
        | Uop::MovAMoffs { .. }
        | Uop::Mov8AMoffs { .. }
        | Uop::Grp5 { .. }
        | Uop::StrOne { .. } => true,
        // 融合ペアは step_cached が部品ごとに判定する (ここには来ない)。
        // 保守的に true — 迷ったら控える側に倒す
        Uop::FusedJcc { .. } => true,
    }
}

// ---------- 実行 (従来経路と同じヘルパで) ----------

/// 実効アドレス。レジスタの**今の**値から組む
#[inline]
fn addr_of(m: &Machine, r: &MemRef) -> u32 {
    m.cpu.lin(r.seg as usize, off_of(m, r))
}

/// セグメント適用前の実効オフセット (LEAが使う)
#[inline]
fn off_of(m: &Machine, r: &MemRef) -> u32 {
    let mut off = r.disp;
    if r.base >= 0 {
        off = off.wrapping_add(m.cpu.regs[r.base as usize]);
    }
    if r.index >= 0 {
        off = off.wrapping_add(m.cpu.regs[r.index as usize] << r.scale);
    }
    off
}

pub(super) fn exec(m: &mut Machine, u: Uop) {
    match u {
        // 融合ペアは step_cached が部品 (reconstructした1命令目とJcc) に
        // ほどいてからここへ来る。丸ごとは来ない。
        // **panicさせない** — unreachable!() のpanic整形がこの関数の
        // インライン化を壊し、それだけで全体が2割遅くなった (実測)
        Uop::FusedJcc { .. } => {}
        Uop::MovRmR { rm, reg } => {
            let v = m.cpu.regs[reg as usize];
            match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize] = v,
                Rm::Mem(mr) => {
                    let a = addr_of(m, &mr);
                    m.write32(a, v);
                }
            }
        }
        Uop::MovRRm { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => m.read32(addr_of(m, &mr)),
            };
            m.cpu.regs[reg as usize] = v;
        }
        Uop::Mov8RmR { rm, reg } => {
            let v = m.cpu.reg8(reg as usize);
            match rm {
                Rm::Reg(r) => m.cpu.set_reg8(r as usize, v),
                Rm::Mem(mr) => {
                    let a = addr_of(m, &mr);
                    m.write8(a, v);
                }
            }
        }
        Uop::Mov8RRm { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => m.read8(addr_of(m, &mr)),
            };
            m.cpu.set_reg8(reg as usize, v);
        }
        Uop::MovRImm { reg, imm } => m.cpu.regs[reg as usize] = imm,
        Uop::AluRmR { kind, rm, reg } => {
            let b = m.cpu.regs[reg as usize];
            match rm {
                Rm::Reg(r) => {
                    let a = m.cpu.regs[r as usize];
                    let v = alu_w(&mut m.cpu, kind, a, b, true);
                    if kind != 7 {
                        m.cpu.regs[r as usize] = v;
                    }
                }
                Rm::Mem(mr) => {
                    let addr = addr_of(m, &mr);
                    let a = m.read32(addr);
                    let v = alu_w(&mut m.cpu, kind, a, b, true);
                    if kind != 7 {
                        m.write32(addr, v);
                    }
                }
            }
        }
        Uop::AluRRm { kind, reg, rm } => {
            let a = m.cpu.regs[reg as usize];
            let b = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => m.read32(addr_of(m, &mr)),
            };
            let v = alu_w(&mut m.cpu, kind, a, b, true);
            if kind != 7 {
                m.cpu.regs[reg as usize] = v;
            }
        }
        Uop::Alu8RmR { kind, rm, reg } => {
            let b = m.cpu.reg8(reg as usize);
            match rm {
                Rm::Reg(r) => {
                    let a = m.cpu.reg8(r as usize);
                    let v = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        m.cpu.set_reg8(r as usize, v);
                    }
                }
                Rm::Mem(mr) => {
                    let addr = addr_of(m, &mr);
                    let a = m.read8(addr);
                    let v = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        m.write8(addr, v);
                    }
                }
            }
        }
        Uop::Alu8RRm { kind, reg, rm } => {
            let a = m.cpu.reg8(reg as usize);
            let b = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => m.read8(addr_of(m, &mr)),
            };
            let v = alu8(&mut m.cpu, kind, a, b);
            if kind != 7 {
                m.cpu.set_reg8(reg as usize, v);
            }
        }
        Uop::AluAImm { kind, imm } => {
            let a = m.cpu.regs[AX];
            let v = alu_w(&mut m.cpu, kind, a, imm, true);
            if kind != 7 {
                m.cpu.regs[AX] = v;
            }
        }
        Uop::Alu8AImm { kind, imm } => {
            let a = m.cpu.reg8(0);
            let v = alu8(&mut m.cpu, kind, a, imm);
            if kind != 7 {
                m.cpu.set_reg8(0, v);
            }
        }
        Uop::Grp1RmImm { kind, rm, imm } => match rm {
            Rm::Reg(r) => {
                let a = m.cpu.regs[r as usize];
                let v = alu_w(&mut m.cpu, kind, a, imm, true);
                if kind != 7 {
                    m.cpu.regs[r as usize] = v;
                }
            }
            Rm::Mem(mr) => {
                let addr = addr_of(m, &mr);
                let a = m.read32(addr);
                let v = alu_w(&mut m.cpu, kind, a, imm, true);
                if kind != 7 {
                    m.write32(addr, v);
                }
            }
        },
        Uop::Grp18RmImm { kind, rm, imm } => match rm {
            Rm::Reg(r) => {
                let a = m.cpu.reg8(r as usize);
                let v = alu8(&mut m.cpu, kind, a, imm);
                if kind != 7 {
                    m.cpu.set_reg8(r as usize, v);
                }
            }
            Rm::Mem(mr) => {
                let addr = addr_of(m, &mr);
                let a = m.read8(addr);
                let v = alu8(&mut m.cpu, kind, a, imm);
                if kind != 7 {
                    m.write8(addr, v);
                }
            }
        },
        Uop::TestRmR { rm, reg } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => m.read32(addr_of(m, &mr)),
            };
            let b = m.cpu.regs[reg as usize];
            alu_w(&mut m.cpu, 4, a, b, true);
        }
        Uop::Test8RmR { rm, reg } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => m.read8(addr_of(m, &mr)),
            };
            let b = m.cpu.reg8(reg as usize);
            alu8(&mut m.cpu, 4, a, b);
        }
        Uop::Lea { reg, mem } => {
            let off = off_of(m, &mem);
            m.cpu.regs[reg as usize] = off;
        }
        Uop::Jcc { cc, rel } => {
            if condition(&m.cpu, cc) {
                let ip = m.cpu.ip.wrapping_add(rel);
                m.cpu.set_ip(ip);
            }
        }
        Uop::JmpRel { rel } => {
            let ip = m.cpu.ip.wrapping_add(rel);
            m.cpu.set_ip(ip);
        }
        Uop::CallRel { rel } => {
            let ret = m.cpu.ip;
            push_w(m, ret, true);
            m.cpu.set_ip(ret.wrapping_add(rel));
        }
        Uop::Ret => {
            let ip = pop_w(m, true);
            m.cpu.set_ip(ip);
        }
        Uop::PushR { reg } => {
            let v = m.cpu.regs[reg as usize];
            push_w(m, v, true);
        }
        Uop::PopR { reg } => {
            let v = pop_w(m, true);
            m.cpu.regs[reg as usize] = v;
        }
        Uop::ShiftRmImm { kind, rm, count } => shift_exec(m, kind, rm, count),
        Uop::ShiftRmCl { kind, rm } => {
            let count = m.cpu.reg8(1); // CL
            shift_exec(m, kind, rm, count);
        }
        Uop::MovzxB { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => m.read8(addr_of(m, &mr)),
            };
            m.cpu.regs[reg as usize] = v as u32;
        }
        Uop::IncR { reg } => {
            let a = m.cpu.regs[reg as usize];
            let v = a.wrapping_add(1);
            m.cpu.regs[reg as usize] = v;
            // CFは触らない (従来経路と同じ)
            m.cpu.set_flag(OF, a == 0x7FFF_FFFF);
            m.cpu.set_flag(AF, a & 0xF == 0xF);
            set_szp_w(&mut m.cpu, v, true);
        }
        Uop::DecR { reg } => {
            let a = m.cpu.regs[reg as usize];
            let v = a.wrapping_sub(1);
            m.cpu.regs[reg as usize] = v;
            m.cpu.set_flag(OF, a == 0x8000_0000);
            m.cpu.set_flag(AF, a & 0xF == 0);
            set_szp_w(&mut m.cpu, v, true);
        }
        Uop::MovRmImm { rm, imm } => match rm {
            Rm::Reg(r) => m.cpu.regs[r as usize] = imm,
            Rm::Mem(mr) => {
                let a = addr_of(m, &mr);
                m.write32(a, imm);
            }
        },
        Uop::MovRm8Imm { rm, imm } => match rm {
            Rm::Reg(r) => m.cpu.set_reg8(r as usize, imm),
            Rm::Mem(mr) => {
                let a = addr_of(m, &mr);
                m.write8(a, imm);
            }
        },
        Uop::MovAMoffs { load, seg, off } => {
            let a = m.cpu.lin(seg as usize, off);
            if load {
                let v = m.read32(a);
                m.cpu.regs[AX] = v;
            } else {
                m.write32(a, m.cpu.regs[AX]);
            }
        }
        Uop::Mov8AMoffs { load, seg, off } => {
            let a = m.cpu.lin(seg as usize, off);
            if load {
                let v = m.read8(a);
                m.cpu.set_reg8(0, v);
            } else {
                m.write8(a, m.cpu.reg8(0));
            }
        }
        Uop::PushImm { imm } => push_w(m, imm, true),
        Uop::Leave => {
            // LEAVE: SP←BP、そしてBPをpop (従来経路の写し)
            let bp = m.cpu.regs[BP];
            sp_write(m, bp);
            let v = pop_w(m, true);
            m.cpu.regs[BP] = v;
        }
        Uop::XchgAR { reg } => {
            m.cpu.regs.swap(AX, reg as usize);
        }
        Uop::Grp3b { kind, rm, imm } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => m.read8(addr_of(m, &mr)),
            };
            match kind {
                0 | 1 => {
                    alu8(&mut m.cpu, 4, a, imm);
                }
                2 => write_rm8(m, rm, !a),
                _ => {
                    let r = alu8(&mut m.cpu, 5, 0, a);
                    m.cpu.set_flag(CF, a != 0);
                    write_rm8(m, rm, r);
                }
            }
        }
        Uop::Grp3w { kind, rm, imm } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => m.read32(addr_of(m, &mr)),
            };
            match kind {
                0 | 1 => {
                    alu_w(&mut m.cpu, 4, a, imm, true);
                }
                2 => write_rm32(m, rm, !a),
                _ => {
                    let r = alu_w(&mut m.cpu, 5, 0, a, true);
                    m.cpu.set_flag(CF, a != 0);
                    write_rm32(m, rm, r);
                }
            }
        }
        Uop::Grp5 { kind, rm } => match kind {
            0 | 1 => {
                // inc/dec r/m: CFを保存して足し引き (従来経路の写し)
                let (a, addr) = read_rm32_addr(m, rm);
                let cf = m.cpu.flag(CF);
                let r = alu_w(&mut m.cpu, if kind == 0 { 0 } else { 5 }, a, 1, true);
                m.cpu.set_flag(CF, cf);
                match addr {
                    Some(a2) => m.write32(a2, r),
                    None => {
                        if let Rm::Reg(rr) = rm {
                            m.cpu.regs[rr as usize] = r;
                        }
                    }
                }
            }
            2 => {
                let t = read_rm32(m, rm);
                let ret = m.cpu.ip;
                push_w(m, ret, true);
                m.cpu.set_ip(t);
            }
            4 => {
                let t = read_rm32(m, rm);
                m.cpu.set_ip(t);
            }
            _ => {
                let v = read_rm32(m, rm);
                push_w(m, v, true);
            }
        },
        Uop::SetCC { cc, rm } => {
            let v = u8::from(condition(&m.cpu, cc));
            write_rm8(m, rm, v);
        }
        Uop::MovzxW { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize] as u16,
                Rm::Mem(mr) => m.read16(addr_of(m, &mr)),
            };
            m.cpu.regs[reg as usize] = v as u32;
        }
        Uop::ImulRRm { reg, rm } => {
            // IMUL r32, r/m32 (従来経路 twobyte 0xAF の写し)
            let a = m.cpu.regs[reg as usize] as i32 as i64;
            let b = read_rm32(m, rm) as i32 as i64;
            let r = a * b;
            m.cpu.regs[reg as usize] = r as u32;
            let ext = (r as i32 as i64) != r;
            m.cpu.set_flag(CF, ext);
            m.cpu.set_flag(OF, ext);
        }
        Uop::StrOne { op, seg } => {
            // 単発ストリング命令。従来の string::exec に丸ごと委譲 (二重実装しない)
            let d = Decoder {
                seg_override: if seg >= 0 { Some(seg as usize) } else { None },
                rep: None,
                opsize32: true,
                addrsize32: true,
            };
            string::exec(m, &d, op);
        }
    }
}

/// r/m32 の読み。メモリなら番地も返す (RMWで再変換しないため)
#[inline]
fn read_rm32_addr(m: &Machine, rm: Rm) -> (u32, Option<u32>) {
    match rm {
        Rm::Reg(r) => (m.cpu.regs[r as usize], None),
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr);
            (m.read32(a), Some(a))
        }
    }
}

#[inline]
fn read_rm32(m: &Machine, rm: Rm) -> u32 {
    match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize],
        Rm::Mem(mr) => m.read32(addr_of(m, &mr)),
    }
}

#[inline]
fn write_rm8(m: &mut Machine, rm: Rm, v: u8) {
    match rm {
        Rm::Reg(r) => m.cpu.set_reg8(r as usize, v),
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr);
            m.write8(a, v);
        }
    }
}

#[inline]
fn write_rm32(m: &mut Machine, rm: Rm, v: u32) {
    match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize] = v,
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr);
            m.write32(a, v);
        }
    }
}

/// シフトの共通部。従来経路 (grp2 のw形) と同じく**結果は常に書き戻す**
fn shift_exec(m: &mut Machine, kind: u8, rm: Rm, count: u8) {
    match rm {
        Rm::Reg(r) => {
            let a = m.cpu.regs[r as usize];
            let v = shift_rot(&mut m.cpu, kind, a, count, 32);
            m.cpu.regs[r as usize] = v;
        }
        Rm::Mem(mr) => {
            let addr = addr_of(m, &mr);
            let a = m.read32(addr);
            let v = shift_rot(&mut m.cpu, kind, a, count, 32);
            m.write32(addr, v);
        }
    }
}
