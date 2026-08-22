//! 実行器 — デコード済みの [`Uop`] を実行する。
//!
//! **意味論を二重実装しない**: フラグも演算も従来経路と同じヘルパ
//! (alu8/alu_w/shift_rot/condition/push_w/pop_w、string::exec) を呼ぶ。
//! ここにあるのは「オペランドの取り回し」だけである。

use super::super::alu::{alu8, alu_w, condition, inc_dec_w, set_szp_w};
use super::super::operand::{pop_w, push_w};
use super::super::shift::shift_rot;
use super::super::twobyte;
use super::super::{sp_write, string, Decoder, AX, BP, CF, DX, OF, SP, SS};
use super::{MemRef, Rm, Uop};
use crate::Machine;

/// このuopがメモリ (線形アドレス) に触り得るか。
///
/// **#PF巻き戻しの控え (guard_save) を省いてよいかの判定**なので、
/// 迷ったら true に倒す (保守的に正しく)。フェッチは対象外 —
/// キャッシュ済み命令はページ内で完結し、番地は実行前に変換済み。
/// IPを直線以外へ動かし得るuopか (Entryメタデータ用)。真なら実行後に
/// 「IPが直線のままか」を実測で確かめる。偽なら直線が確定していて、
/// 毎命令の ip==ip_linear 比較を省ける
pub(super) fn is_control(u: &Uop) -> bool {
    matches!(
        u,
        Uop::Jcc { .. }
            | Uop::JmpRel { .. }
            | Uop::CallRel { .. }
            | Uop::Ret
            | Uop::Grp5 { kind: 2, .. }
            | Uop::Grp5 { kind: 4, .. }
    )
}

/// JIT の語彙に無い uop のうち、従来は従来経路落ち = チェーン切断で次命令が
/// JIT の受け口になっていたもの (C16 で語彙入りした家系)。実行後に再プローブ
/// させて、JIT で走れる後続ブロックを interp に取り残さない
pub(super) fn reprobe_after(u: &Uop) -> bool {
    matches!(
        u,
        Uop::MovsxB { .. }
            | Uop::MovsxW { .. }
            | Uop::Cmov { .. }
            | Uop::TestAImm { .. }
            | Uop::Test8AImm { .. }
            | Uop::ImulRRmI { .. }
            | Uop::Cdq
            | Uop::BitScan { .. }
            | Uop::ShxdImm { .. }
            | Uop::ShxdCl { .. }
    )
}

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
        | Uop::Alu8AImm { .. }
        | Uop::TestAImm { .. }
        | Uop::Test8AImm { .. }
        | Uop::Cdq => false,
        // translate-first済み (F1c-d5): 低速路に落ちるときだけ自前で控える
        Uop::MovRmR { .. }
        | Uop::MovRRm { .. }
        | Uop::AluRRm { .. }
        | Uop::TestRmR { .. }
        | Uop::MovRmImm { .. }
        | Uop::MovzxB { .. }
        | Uop::MovsxB { .. }
        | Uop::MovzxW { .. }
        | Uop::PushR { .. }
        | Uop::PopR { .. }
        | Uop::PushImm { .. }
        | Uop::CallRel { .. }
        | Uop::Ret
        | Uop::Mov8RmR { .. }
        | Uop::Mov8RRm { .. }
        | Uop::Mov16RmR { .. }
        | Uop::Mov16RRm { .. }
        | Uop::AluRmR { .. }
        | Uop::Alu8RmR { .. }
        | Uop::Alu8RRm { .. }
        | Uop::Grp1RmImm { .. }
        | Uop::Grp18RmImm { .. }
        | Uop::Test8RmR { .. }
        | Uop::MovRm8Imm { .. }
        | Uop::Leave => false,
        // r/m形: メモリオペランドのときだけ
        Uop::Grp3b { rm, .. }
        | Uop::Grp3w { rm, .. }
        | Uop::SetCC { rm, .. }
        | Uop::ImulRRm { rm, .. }
        | Uop::ImulRRmI { rm, .. }
        | Uop::MovsxW { rm, .. }
        | Uop::Cmov { rm, .. }
        | Uop::BitScan { rm, .. }
        | Uop::ShxdImm { rm, .. }
        | Uop::ShxdCl { rm, .. }
        | Uop::ShiftRmImm { rm, .. }
        | Uop::ShiftRmCl { rm, .. } => mem(rm),
        // スタック・moffs・ストリング・grp5 (push/call間接等) は常にメモリ
        Uop::MovAMoffs { .. }
        | Uop::Mov8AMoffs { .. }
        | Uop::Grp5 { .. }
        | Uop::StrOne { .. }
        | Uop::StrRep { .. } => true,
    }
}

// ---------- 実行 (従来経路と同じヘルパで) ----------

// ---- translate-firstの低速路 (F1c-d5増分3の#[cold]化) ----
//
// 増分3の初版 (+17%悪化、タグ exp/translate-first-rmw) の敗因は
// armにfast+slowを二重内蔵したコードの嵩 (B3と同族)。低速路を
// #[cold]#[inline(never)]でホットなmatchの外に追い出し、armには
// fast path数行+呼び出し1行だけを残す

#[cold]
#[inline(never)]
fn slow_rmw32(m: &mut Machine, mr: &MemRef, kind: u8, b: u32, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    let addr = addr_of(m, mr, 4, true);
    let a = m.read32(addr);
    let v = alu_w(&mut m.cpu, kind, a, b, true);
    m.write32(addr, v);
}

#[cold]
#[inline(never)]
fn slow_rmw8(m: &mut Machine, mr: &MemRef, kind: u8, b: u8, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    let addr = addr_of(m, mr, 1, true);
    let a = m.read8(addr);
    let v = alu8(&mut m.cpu, kind, a, b);
    m.write8(addr, v);
}

#[cold]
#[inline(never)]
fn slow_read32(m: &mut Machine, mr: &MemRef, prev_ip: u32) -> u32 {
    m.guard_save_slim_at(prev_ip);
    m.read32(addr_of(m, mr, 4, false))
}

#[cold]
#[inline(never)]
fn slow_read16(m: &mut Machine, mr: &MemRef, prev_ip: u32) -> u16 {
    m.guard_save_slim_at(prev_ip);
    m.read16(addr_of(m, mr, 2, false))
}

#[cold]
#[inline(never)]
fn slow_read8(m: &mut Machine, mr: &MemRef, prev_ip: u32) -> u8 {
    m.guard_save_slim_at(prev_ip);
    m.read8(addr_of(m, mr, 1, false))
}

#[cold]
#[inline(never)]
fn slow_write8(m: &mut Machine, mr: &MemRef, v: u8, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    let a = addr_of(m, mr, 1, true);
    m.write8(a, v);
}

#[cold]
#[inline(never)]
fn slow_write16(m: &mut Machine, mr: &MemRef, v: u16, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    let a = addr_of(m, mr, 2, true);
    m.write16(a, v);
}

#[cold]
#[inline(never)]
fn slow_leave(m: &mut Machine, bp: u32, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    sp_write(m, bp);
    let v = pop_w(m, true);
    m.cpu.regs[BP] = v;
}

#[cold]
#[inline(never)]
fn slow_write32(m: &mut Machine, mr: &MemRef, v: u32, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    let a = addr_of(m, mr, 4, true);
    m.write32(a, v);
}

#[cold]
#[inline(never)]
fn slow_push32(m: &mut Machine, v: u32, prev_ip: u32) {
    m.guard_save_slim_at(prev_ip);
    push_w(m, v, true);
}

#[cold]
#[inline(never)]
fn slow_pop32(m: &mut Machine, prev_ip: u32) -> u32 {
    m.guard_save_slim_at(prev_ip);
    pop_w(m, true)
}

// ---- 稀なuopのarm本体 (各~1%以下)。ホットなmatchをI-cacheから痩せさせる ----
//
// 意味論は移動のみ (中身は元のarmの逐語)。控えは対応するF_MEM分類で
// step_cached側が済ませている

#[cold]
#[inline(never)]
fn cold_grp3b(m: &mut Machine, kind: u8, rm: Rm, imm: u8) {
    let a = match rm {
        Rm::Reg(r) => m.cpu.reg8(r as usize),
        Rm::Mem(mr) => m.read8(addr_of(m, &mr, 1, false)),
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

#[cold]
#[inline(never)]
fn cold_grp3w(m: &mut Machine, kind: u8, rm: Rm, imm: u32) {
    let a = match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize],
        Rm::Mem(mr) => m.read32(addr_of(m, &mr, 4, false)),
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

#[cold]
#[inline(never)]
fn cold_grp5(m: &mut Machine, kind: u8, rm: Rm) {
    match kind {
        0 | 1 => {
            // inc/dec r/m: CF不変 (従来経路と同じヘルパ)
            let (a, addr) = read_rm32_addr(m, rm);
            let r = inc_dec_w(&mut m.cpu, a, kind != 0, true);
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
    }
}

#[cold]
#[inline(never)]
fn cold_moffs32(m: &mut Machine, load: bool, seg: u8, off: u32) {
    let a = m.data_addr(seg as usize, off, 4, !load);
    if load {
        let v = m.read32(a);
        m.cpu.regs[AX] = v;
    } else {
        m.write32(a, m.cpu.regs[AX]);
    }
}

#[cold]
#[inline(never)]
fn cold_moffs8(m: &mut Machine, load: bool, seg: u8, off: u32) {
    let a = m.data_addr(seg as usize, off, 1, !load);
    if load {
        let v = m.read8(a);
        m.cpu.set_reg8(0, v);
    } else {
        m.write8(a, m.cpu.reg8(0));
    }
}

#[cold]
#[inline(never)]
fn cold_imul(m: &mut Machine, reg: u8, rm: Rm) {
    // IMUL r32, r/m32 (従来経路 twobyte 0xAF の写し)
    let a = m.cpu.regs[reg as usize] as i32 as i64;
    let b = read_rm32(m, rm) as i32 as i64;
    let r = a * b;
    m.cpu.regs[reg as usize] = r as u32;
    let ext = (r as i32 as i64) != r;
    m.cpu.set_flag(CF, ext);
    m.cpu.set_flag(OF, ext);
}

#[cold]
#[inline(never)]
fn cold_movsx16(m: &mut Machine, reg: u8, rm: Rm) {
    let v = match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize] as u16,
        Rm::Mem(mr) => m.read16(addr_of(m, &mr, 2, false)),
    };
    m.cpu.regs[reg as usize] = v as i16 as i32 as u32;
}

#[cold]
#[inline(never)]
fn cold_imul3(m: &mut Machine, reg: u8, rm: Rm, imm: u32) {
    // IMUL r32, r/m32, imm (従来経路 onebyte 0x69/0x6B の32bit形の写し —
    // cold_imul と同じフラグ規則: CF/OFは幅に収まらなかったかだけ)
    let a = read_rm32(m, rm) as i32 as i64;
    let r = a * (imm as i32 as i64);
    m.cpu.regs[reg as usize] = r as u32;
    let ext = (r as i32 as i64) != r;
    m.cpu.set_flag(CF, ext);
    m.cpu.set_flag(OF, ext);
}

#[cold]
#[inline(never)]
fn cold_bit_scan(m: &mut Machine, reg: u8, rm: Rm, reverse: bool) {
    let v = read_rm32(m, rm);
    if let Some(pos) = twobyte::bit_scan(&mut m.cpu, v, reverse) {
        m.cpu.regs[reg as usize] = pos;
    }
}

#[cold]
#[inline(never)]
fn cold_shxd(m: &mut Machine, rm: Rm, reg: u8, count: u8, left: bool) {
    // SHLD/SHRD r/m32, r32, count。本体は twobyte::shxd (原本1つ)。
    // count==0 は何もしない (フラグも不変) — 従来経路の早期returnと同じ
    let count = (count & 0x1F) as u32;
    if count == 0 {
        return;
    }
    let (dst, addr) = read_rm32_addr(m, rm);
    let src = m.cpu.regs[reg as usize];
    let (r, cf) = twobyte::shxd(dst, src, count, left, true);
    match addr {
        Some(a) => m.write32(a, r),
        None => write_rm32(m, rm, r),
    }
    m.cpu.set_flag(CF, cf);
    set_szp_w(&mut m.cpu, r, true);
}

#[cold]
#[inline(never)]
fn cold_strrep(m: &mut Machine, op: u8, seg: i8, rep: u8, o16: bool) {
    // REP付きストリング。cold_stroneと同形でrepだけ足す — bulk一括化
    // (string.rs) も割り込み受付粒度 (REP完走後) も従来経路と同一。
    // o16 (movsw等) は幅をひっくり返すだけ — 32bitコード既定32→16
    let d = Decoder {
        seg_override: if seg >= 0 { Some(seg as usize) } else { None },
        rep: Some(rep),
        opsize32: !o16,
        addrsize32: true,
        p66: o16,
        lock: false,
    };
    string::exec(m, &d, op);
}

#[cold]
#[inline(never)]
fn cold_strone(m: &mut Machine, op: u8, seg: i8, o16: bool) {
    // 単発ストリング命令。従来の string::exec に丸ごと委譲 (二重実装しない)
    let d = Decoder {
        seg_override: if seg >= 0 { Some(seg as usize) } else { None },
        rep: None,
        opsize32: !o16,
        addrsize32: true,
        p66: o16,
        lock: false,
    };
    string::exec(m, &d, op);
}

/// pushの速い道 (translate-first)。SSが平坦・32bitスタックで書き込みが
/// 確定するときだけ実行してtrue。falseなら呼び手が控えて従来経路へ
#[inline]
fn fast_push32(m: &mut Machine, v: u32) -> bool {
    m.jit_try_push32(v) // 実装は1本 (mem/mod.rs) — JITヘルパと共有
}

/// popの速い道。読みが確定したときだけSPを確定 (push32/pop_wと同じ約束)
#[inline]
fn fast_pop32(m: &mut Machine) -> Option<u32> {
    m.jit_try_pop32() // 実装は1本 (mem/mod.rs) — JITヘルパと共有
}

/// 実効アドレス。レジスタの**今の**値から組む。
/// セグメント検査つき (data_addr) — 違反は控えられ、毒番地が返る
#[inline]
fn addr_of(m: &Machine, r: &MemRef, size: u32, write: bool) -> u32 {
    m.data_addr(r.seg as usize, off_of(m, r), size, write)
}

/// セグメント適用前の実効オフセット (LEAが使う)
#[inline]
fn off_of(m: &Machine, r: &MemRef) -> u32 {
    // census (P4): 形の動的分布。opstats以外ではコストゼロ
    if cfg!(feature = "opstats") {
        let shape = usize::from(r.base >= 0)
            | (usize::from(r.index >= 0) << 1)
            | (usize::from(r.disp != 0) << 2);
        m.ea_counts[shape].set(m.ea_counts[shape].get() + 1);
    }
    let mut off = r.disp;
    if r.base >= 0 {
        off = off.wrapping_add(m.cpu.regs[r.base as usize]);
    }
    if r.index >= 0 {
        off = off.wrapping_add(m.cpu.regs[r.index as usize] << r.scale);
    }
    off
}

pub(super) fn exec(m: &mut Machine, u: Uop, prev_ip: u32) {
    match u {
        Uop::MovRmR { rm, reg } => {
            let v = m.cpu.regs[reg as usize];
            match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize] = v,
                Rm::Mem(mr) => {
                    // translate-first (F1c-d5): 成功が確定するまで状態を変えない
                    // ので、成功路はguard控えが要らない。ダメなら控えて従来経路
                    let off = off_of(m, &mr);
                    if m.fast_write32(mr.seg as usize, off, v).is_none() {
                        // 低速路: 控えの巻き戻し先は**命令頭** (prev_ip) —
                        // advance_ip後なので素のsave_slimではダメ (slow_*が担う)
                        slow_write32(m, &mr, v, prev_ip);
                    }
                }
            }
        }
        Uop::MovRRm { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read32(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read32(m, &mr, prev_ip),
                    }
                }
            };
            m.cpu.regs[reg as usize] = v;
        }
        Uop::Mov8RmR { rm, reg } => {
            let v = m.cpu.reg8(reg as usize);
            match rm {
                Rm::Reg(r) => m.cpu.set_reg8(r as usize, v),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    if m.fast_write8(mr.seg as usize, off, v).is_none() {
                        slow_write8(m, &mr, v, prev_ip);
                    }
                }
            }
        }
        Uop::Mov16RmR { rm, reg } => {
            let v = m.cpu.reg16(reg as usize);
            match rm {
                Rm::Reg(r) => m.cpu.set_reg16(r as usize, v),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    if m.fast_write16(mr.seg as usize, off, v).is_none() {
                        slow_write16(m, &mr, v, prev_ip);
                    }
                }
            }
        }
        Uop::Mov16RRm { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg16(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read16(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read16(m, &mr, prev_ip),
                    }
                }
            };
            m.cpu.set_reg16(reg as usize, v);
        }
        Uop::Mov8RRm { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    }
                }
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
                    let off = off_of(m, &mr);
                    if kind == 7 {
                        let a = match m.fast_read32(mr.seg as usize, off) {
                            Some(v) => v,
                            None => slow_read32(m, &mr, prev_ip),
                        };
                        alu_w(&mut m.cpu, kind, a, b, true);
                    } else if let Some(pa) = m.fast_rmw32_addr(mr.seg as usize, off) {
                        let a = u32::from_le_bytes(m.mem[pa..pa + 4].try_into().unwrap());
                        let v = alu_w(&mut m.cpu, kind, a, b, true);
                        m.mem[pa..pa + 4].copy_from_slice(&v.to_le_bytes());
                        // 直書きも自己書き換え検出の網に入れる (ADR-0020)
                        m.dcache.note_write(pa as u32);
                    } else {
                        slow_rmw32(m, &mr, kind, b, prev_ip);
                    }
                }
            }
        }
        Uop::AluRRm { kind, reg, rm } => {
            let a = m.cpu.regs[reg as usize];
            let b = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read32(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read32(m, &mr, prev_ip),
                    }
                }
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
                    let off = off_of(m, &mr);
                    if kind == 7 {
                        let a = match m.fast_read8(mr.seg as usize, off) {
                            Some(v) => v,
                            None => slow_read8(m, &mr, prev_ip),
                        };
                        alu8(&mut m.cpu, kind, a, b);
                    } else if let Some(pa) = m.fast_rmw8_addr(mr.seg as usize, off) {
                        let a = m.mem[pa];
                        let v = alu8(&mut m.cpu, kind, a, b);
                        m.mem[pa] = v;
                        m.dcache.note_write(pa as u32);
                    } else {
                        slow_rmw8(m, &mr, kind, b, prev_ip);
                    }
                }
            }
        }
        Uop::Alu8RRm { kind, reg, rm } => {
            let a = m.cpu.reg8(reg as usize);
            let b = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    }
                }
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
                let off = off_of(m, &mr);
                if kind == 7 {
                    let a = match m.fast_read32(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read32(m, &mr, prev_ip),
                    };
                    alu_w(&mut m.cpu, kind, a, imm, true);
                } else if let Some(pa) = m.fast_rmw32_addr(mr.seg as usize, off) {
                    let a = u32::from_le_bytes(m.mem[pa..pa + 4].try_into().unwrap());
                    let v = alu_w(&mut m.cpu, kind, a, imm, true);
                    m.mem[pa..pa + 4].copy_from_slice(&v.to_le_bytes());
                    m.dcache.note_write(pa as u32);
                } else {
                    slow_rmw32(m, &mr, kind, imm, prev_ip);
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
                let off = off_of(m, &mr);
                if kind == 7 {
                    let a = match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    };
                    alu8(&mut m.cpu, kind, a, imm);
                } else if let Some(pa) = m.fast_rmw8_addr(mr.seg as usize, off) {
                    let a = m.mem[pa];
                    let v = alu8(&mut m.cpu, kind, a, imm);
                    m.mem[pa] = v;
                    m.dcache.note_write(pa as u32);
                } else {
                    slow_rmw8(m, &mr, kind, imm, prev_ip);
                }
            }
        },
        Uop::TestRmR { rm, reg } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize],
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read32(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read32(m, &mr, prev_ip),
                    }
                }
            };
            let b = m.cpu.regs[reg as usize];
            alu_w(&mut m.cpu, 4, a, b, true);
        }
        Uop::Test8RmR { rm, reg } => {
            let a = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    }
                }
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
            if !fast_push32(m, ret) {
                slow_push32(m, ret, prev_ip);
            }
            m.cpu.set_ip(ret.wrapping_add(rel));
        }
        Uop::Ret => {
            let ip = match fast_pop32(m) {
                Some(v) => v,
                None => slow_pop32(m, prev_ip),
            };
            m.cpu.set_ip(ip);
        }
        Uop::PushR { reg } => {
            let v = m.cpu.regs[reg as usize];
            if !fast_push32(m, v) {
                slow_push32(m, v, prev_ip);
            }
        }
        Uop::PopR { reg } => match fast_pop32(m) {
            Some(v) => m.cpu.regs[reg as usize] = v,
            None => {
                let v = slow_pop32(m, prev_ip);
                m.cpu.regs[reg as usize] = v;
            }
        },
        Uop::ShiftRmImm { kind, rm, count } => shift_exec(m, kind, rm, count),
        Uop::ShiftRmCl { kind, rm } => {
            let count = m.cpu.reg8(1); // CL
            shift_exec(m, kind, rm, count);
        }
        Uop::MovzxB { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    }
                }
            };
            m.cpu.regs[reg as usize] = v as u32;
        }
        Uop::IncR { reg } => {
            let a = m.cpu.regs[reg as usize];
            // CFは触らない (従来経路と同じヘルパ)
            m.cpu.regs[reg as usize] = inc_dec_w(&mut m.cpu, a, false, true);
        }
        Uop::DecR { reg } => {
            let a = m.cpu.regs[reg as usize];
            m.cpu.regs[reg as usize] = inc_dec_w(&mut m.cpu, a, true, true);
        }
        Uop::MovRmImm { rm, imm } => match rm {
            Rm::Reg(r) => m.cpu.regs[r as usize] = imm,
            Rm::Mem(mr) => {
                let off = off_of(m, &mr);
                if m.fast_write32(mr.seg as usize, off, imm).is_none() {
                    slow_write32(m, &mr, imm, prev_ip);
                }
            }
        },
        Uop::MovRm8Imm { rm, imm } => match rm {
            Rm::Reg(r) => m.cpu.set_reg8(r as usize, imm),
            Rm::Mem(mr) => {
                let off = off_of(m, &mr);
                if m.fast_write8(mr.seg as usize, off, imm).is_none() {
                    slow_write8(m, &mr, imm, prev_ip);
                }
            }
        },
        Uop::MovAMoffs { load, seg, off } => cold_moffs32(m, load, seg, off),
        Uop::Mov8AMoffs { load, seg, off } => cold_moffs8(m, load, seg, off),
        Uop::PushImm { imm } => {
            if !fast_push32(m, imm) {
                slow_push32(m, imm, prev_ip);
            }
        }
        Uop::Leave => {
            // LEAVE: SP←BP、そしてBPをpop
            let bp = m.cpu.regs[BP];
            let fast = m.cpu.hidden[SS].big;
            if fast {
                if let Some(v) = m.fast_read32(SS, bp) {
                    // 読みが確定してから両レジスタを動かす (jit_try_leaveと同順)
                    m.cpu.regs[SP] = bp.wrapping_add(4);
                    m.cpu.regs[BP] = v;
                } else {
                    slow_leave(m, bp, prev_ip);
                }
            } else {
                slow_leave(m, bp, prev_ip);
            }
        }
        Uop::XchgAR { reg } => {
            m.cpu.regs.swap(AX, reg as usize);
        }
        Uop::Grp3b { kind, rm, imm } => cold_grp3b(m, kind, rm, imm),
        Uop::Grp3w { kind, rm, imm } => cold_grp3w(m, kind, rm, imm),
        Uop::Grp5 { kind, rm } => cold_grp5(m, kind, rm),
        Uop::SetCC { cc, rm } => {
            let v = u8::from(condition(&m.cpu, cc));
            write_rm8(m, rm, v);
        }
        Uop::MovzxW { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.regs[r as usize] as u16,
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read16(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read16(m, &mr, prev_ip),
                    }
                }
            };
            m.cpu.regs[reg as usize] = v as u32;
        }
        Uop::ImulRRm { reg, rm } => cold_imul(m, reg, rm),
        // C16 (ADR-0028): X窓の従来経路落ちを語彙へ。意味論は従来経路と同じ
        // ヘルパ (alu_w / condition / twobyte::shxd / bit_scan) を呼ぶ
        Uop::TestAImm { imm } => {
            let a = m.cpu.regs[AX];
            alu_w(&mut m.cpu, 4, a, imm, true);
        }
        Uop::Test8AImm { imm } => {
            let a = m.cpu.reg8(0);
            alu8(&mut m.cpu, 4, a, imm);
        }
        Uop::Cdq => {
            let v = if m.cpu.regs[AX] & 0x8000_0000 != 0 {
                0xFFFF_FFFF
            } else {
                0
            };
            m.cpu.regs[DX] = v;
        }
        Uop::MovsxB { reg, rm } => {
            let v = match rm {
                Rm::Reg(r) => m.cpu.reg8(r as usize),
                Rm::Mem(mr) => {
                    let off = off_of(m, &mr);
                    match m.fast_read8(mr.seg as usize, off) {
                        Some(v) => v,
                        None => slow_read8(m, &mr, prev_ip),
                    }
                }
            };
            m.cpu.regs[reg as usize] = v as i8 as i32 as u32;
        }
        Uop::MovsxW { reg, rm } => cold_movsx16(m, reg, rm),
        Uop::Cmov { cc, reg, rm } => {
            // 読みは条件に関わらず行う (偽でもメモリオペランドのフォールトは起きる)
            let v = read_rm32(m, rm);
            if condition(&m.cpu, cc) {
                m.cpu.regs[reg as usize] = v;
            }
        }
        Uop::ImulRRmI { reg, rm, imm } => cold_imul3(m, reg, rm, imm),
        Uop::BitScan { reg, rm, reverse } => cold_bit_scan(m, reg, rm, reverse),
        Uop::ShxdImm { rm, reg, imm, left } => cold_shxd(m, rm, reg, imm, left),
        Uop::ShxdCl { rm, reg, left } => {
            let count = m.cpu.reg8(1); // CL
            cold_shxd(m, rm, reg, count, left)
        }
        Uop::StrOne { op, seg, o16 } => cold_strone(m, op, seg, o16),
        Uop::StrRep { op, seg, rep, o16 } => cold_strrep(m, op, seg, rep, o16),
    }
}

/// r/m32 の読み。メモリなら番地も返す (RMWで再変換しないため — 検査も書き込みで受ける)
#[inline]
fn read_rm32_addr(m: &Machine, rm: Rm) -> (u32, Option<u32>) {
    match rm {
        Rm::Reg(r) => (m.cpu.regs[r as usize], None),
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr, 4, true);
            (m.read32(a), Some(a))
        }
    }
}

#[inline]
fn read_rm32(m: &Machine, rm: Rm) -> u32 {
    match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize],
        Rm::Mem(mr) => m.read32(addr_of(m, &mr, 4, false)),
    }
}

#[inline]
fn write_rm8(m: &mut Machine, rm: Rm, v: u8) {
    match rm {
        Rm::Reg(r) => m.cpu.set_reg8(r as usize, v),
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr, 1, true);
            m.write8(a, v);
        }
    }
}

#[inline]
fn write_rm32(m: &mut Machine, rm: Rm, v: u32) {
    match rm {
        Rm::Reg(r) => m.cpu.regs[r as usize] = v,
        Rm::Mem(mr) => {
            let a = addr_of(m, &mr, 4, true);
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
            let addr = addr_of(m, &mr, 4, true);
            let a = m.read32(addr);
            let v = shift_rot(&mut m.cpu, kind, a, count, 32);
            m.write32(addr, v);
        }
    }
}
