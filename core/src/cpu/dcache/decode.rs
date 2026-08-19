//! 純デコード — 物理メモリのバイト列を [`Uop`] に写す。**副作用なし**。
//!
//! [`super::super::onebyte`] / [`twobyte`] の該当armの写経であり、
//! 番地は解決せず「作り方 (MemRef)」を返す。対象外は None で
//! 従来経路に任せる (このNoneの寛容さが安全に刻める理由)。

use super::super::{BP, CS, DS, SP, SS};
use super::{MemRef, Rm, Uop};
use crate::Machine;

// ---------- 純デコード (副作用なし) ----------

fn u32le(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

/// ModRM (32bitアドレッシング) を読む。[`super::operand::modrm`] の写経 —
/// ただし番地を解決せず、**作り方 (MemRef)** を返す
fn dec_modrm(b: &[u8], i: &mut usize, seg_override: Option<u8>) -> Option<(u8, Rm)> {
    let mrm = *b.get(*i)?;
    *i += 1;
    let md = mrm >> 6;
    let reg = (mrm >> 3) & 7;
    let rm = mrm & 7;
    if md == 3 {
        return Some((reg, Rm::Reg(rm)));
    }
    let mut default_seg = DS as u8;
    let base: i8;
    let mut index: i8 = -1;
    let mut scale: u8 = 0;
    let mut disp: u32 = 0;
    if rm == 4 {
        // SIB
        let sib = *b.get(*i)?;
        *i += 1;
        scale = sib >> 6;
        let idx = ((sib >> 3) & 7) as usize;
        let bs = (sib & 7) as usize;
        if idx != 4 {
            index = idx as i8;
        }
        if bs == 5 && md == 0 {
            base = -1;
            disp = u32le(b, *i)?;
            *i += 4;
        } else {
            if bs == SP || bs == BP {
                default_seg = SS as u8;
            }
            base = bs as i8;
        }
    } else if rm == 5 && md == 0 {
        base = -1;
        disp = u32le(b, *i)?;
        *i += 4;
    } else {
        if rm as usize == BP {
            default_seg = SS as u8;
        }
        base = rm as i8;
    }
    disp = disp.wrapping_add(match md {
        0 => 0,
        1 => {
            let v = *b.get(*i)? as i8 as i32 as u32;
            *i += 1;
            v
        }
        _ => {
            let v = u32le(b, *i)?;
            *i += 4;
            v
        }
    });
    Some((
        reg,
        Rm::Mem(MemRef {
            base,
            index,
            scale,
            seg: seg_override.unwrap_or(default_seg),
            disp,
        }),
    ))
}

/// 物理 `pa` の命令を対象なら Uop へ。対象外・ページ跨ぎ・RAM外は None
pub(super) fn decode_at(m: &Machine, pa: u32) -> Option<(u8, Uop)> {
    // ページ内のバイト列だけを見る。跨いだら控えない
    let start = pa as usize;
    let page_end = (start | 0xFFF) + 1;
    let n = (page_end - start)
        .min(16)
        .min(m.mem.len().saturating_sub(start));
    if n == 0 {
        return None;
    }
    let b = &m.mem[start..start + n];
    let mut i = 0usize;
    let mut seg_override: Option<u8> = None;
    let mut o16 = false;
    let mut rep: Option<u8> = None;

    // プレフィクス。0x67/REPが来たら対象外 (従来経路が観測ごと面倒を見る)。
    // 0x66 (16bitオペランド) は受ける — 従来経路落ちの74%が0x66だった
    // (census 2026-08-13)。ただし語彙に入れるのはmovだけ (下のo16検査)
    let op = loop {
        let x = *b.get(i)?;
        i += 1;
        match x {
            0x26 => seg_override = Some(super::super::ES as u8),
            0x2E => seg_override = Some(CS as u8),
            0x36 => seg_override = Some(SS as u8),
            0x3E => seg_override = Some(DS as u8),
            0x64 => seg_override = Some(super::super::FS as u8),
            0x65 => seg_override = Some(super::super::GS as u8),
            0x66 => o16 = true,
            // LOCK: 実行の意味は持たない (シングルコア) が、付けてよい命令かの
            // #UD検査があるので従来経路に任せる (稀なので速さの損は無い)
            0xF0 => return None,
            0x67 => return None,
            // REP/REPNE: ストリング命令ならStrRepで受ける (それ以外は従来経路
            // — SSE系の0xF2/F3プレフィクス用途は語彙外)
            0xF2 | 0xF3 => rep = Some(x),
            _ => break x,
        }
        if i >= 15 {
            return None;
        }
    };
    // 0x66つきで語彙に居るのは mov r16 (89/8B) とストリング命令だけ。
    // 他は従来経路へ (ストリングはブート従来経路落ちの53%が66 A5=movswだった
    //  — census 2026-08-18、ADR-0027 PR3)
    if o16 && op != 0x89 && op != 0x8B && !matches!(op, 0xA4..=0xA7 | 0xAA..=0xAF) {
        return None;
    }
    // REPつきで語彙に居るのはストリング命令 (A4-A7/AA-AF) だけ。
    // INS/OUTS (6C-6F) はio_permittedがtrap_ipを使うので従来経路のまま
    if rep.is_some() && !matches!(op, 0xA4..=0xA7 | 0xAA..=0xAF) {
        return None;
    }

    let uop = match op {
        // --- ALUグリッド (プレフィクスは上で消化済みなので 26/2E/36/3E は来ない) ---
        0x00..=0x3F if op & 7 <= 5 && (op & 0x27) != 0x26 && (op & 0x27) != 0x27 => {
            let kind = (op >> 3) & 7;
            match op & 7 {
                0 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::Alu8RmR { kind, rm, reg }
                }
                1 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::AluRmR { kind, rm, reg }
                }
                2 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::Alu8RRm { kind, reg, rm }
                }
                3 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::AluRRm { kind, reg, rm }
                }
                4 => {
                    let imm = *b.get(i)?;
                    i += 1;
                    Uop::Alu8AImm { kind, imm }
                }
                _ => {
                    let imm = u32le(b, i)?;
                    i += 4;
                    Uop::AluAImm { kind, imm }
                }
            }
        }
        0x40..=0x47 => Uop::IncR { reg: op & 7 },
        0x48..=0x4F => Uop::DecR { reg: op & 7 },
        0x50..=0x57 => Uop::PushR { reg: op & 7 },
        0x58..=0x5F => Uop::PopR { reg: op & 7 },
        0x68 => {
            let imm = u32le(b, i)?;
            i += 4;
            Uop::PushImm { imm }
        }
        0x6A => {
            let imm = *b.get(i)? as i8 as i32 as u32;
            i += 1;
            Uop::PushImm { imm }
        }
        0x70..=0x7F => {
            let rel = *b.get(i)? as i8 as i32 as u32;
            i += 1;
            Uop::Jcc { cc: op & 0xF, rel }
        }
        0x80 | 0x81 | 0x83 => {
            let (kind, rm) = dec_modrm(b, &mut i, seg_override)?;
            if op == 0x80 {
                let imm = *b.get(i)?;
                i += 1;
                Uop::Grp18RmImm { kind, rm, imm }
            } else {
                // 0x83 は符号拡張された8bit即値 (従来経路と同じ拡張をここで済ます)
                let imm = if op == 0x81 {
                    let v = u32le(b, i)?;
                    i += 4;
                    v
                } else {
                    let v = *b.get(i)? as i8 as i32 as u32;
                    i += 1;
                    v
                };
                Uop::Grp1RmImm { kind, rm, imm }
            }
        }
        0x84 => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::Test8RmR { rm, reg }
        }
        0x85 => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::TestRmR { rm, reg }
        }
        0x88 => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::Mov8RmR { rm, reg }
        }
        0x89 => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            if o16 {
                Uop::Mov16RmR { rm, reg }
            } else {
                Uop::MovRmR { rm, reg }
            }
        }
        0x8A => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::Mov8RRm { reg, rm }
        }
        0x8B => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            if o16 {
                Uop::Mov16RRm { reg, rm }
            } else {
                Uop::MovRRm { reg, rm }
            }
        }
        0x90..=0x97 => Uop::XchgAR { reg: op & 7 },
        0xA0..=0xA3 => {
            let off = u32le(b, i)?;
            i += 4;
            let seg = seg_override.unwrap_or(DS as u8);
            let load = op & 2 == 0; // A0/A1 = 読む、A2/A3 = 書く
            if op & 1 == 0 {
                Uop::Mov8AMoffs { load, seg, off }
            } else {
                Uop::MovAMoffs { load, seg, off }
            }
        }
        // REPなしの単発ストリング命令。意味論は従来の string::exec に丸ごと
        // 委譲する (二重実装しない)。REP付きはプレフィクスで弾かれ従来経路へ
        0xA4..=0xA7 | 0xAA..=0xAF => {
            let seg = seg_override.map(|s| s as i8).unwrap_or(-1);
            match rep {
                // REP付き: 意味論は従来のstring::execに丸ごと委譲 (ADR-0027)。
                // 勘定は従来どおり「REP全体=1命令」— 基線不変
                Some(r) => Uop::StrRep {
                    op,
                    seg,
                    rep: r,
                    o16,
                },
                None => Uop::StrOne { op, seg, o16 },
            }
        }
        0x8D => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            match rm {
                Rm::Mem(mem) => Uop::Lea { reg, mem },
                Rm::Reg(_) => return None, // LEAのレジスタ形は従来経路 (panic) に任せる
            }
        }
        0xB8..=0xBF => {
            let imm = u32le(b, i)?;
            i += 4;
            Uop::MovRImm { reg: op & 7, imm }
        }
        0xC1 | 0xD1 | 0xD3 => {
            let (kind, rm) = dec_modrm(b, &mut i, seg_override)?;
            if op == 0xC1 {
                let count = *b.get(i)?;
                i += 1;
                Uop::ShiftRmImm { kind, rm, count }
            } else if op == 0xD1 {
                Uop::ShiftRmImm { kind, rm, count: 1 }
            } else {
                Uop::ShiftRmCl { kind, rm }
            }
        }
        0xC3 => Uop::Ret,
        0xC6 => {
            let (_, rm) = dec_modrm(b, &mut i, seg_override)?;
            let imm = *b.get(i)?;
            i += 1;
            Uop::MovRm8Imm { rm, imm }
        }
        0xC7 => {
            let (_, rm) = dec_modrm(b, &mut i, seg_override)?;
            let imm = u32le(b, i)?;
            i += 4;
            Uop::MovRmImm { rm, imm }
        }
        0xC9 => Uop::Leave,
        0xF6 | 0xF7 => {
            let (kind, rm) = dec_modrm(b, &mut i, seg_override)?;
            if kind > 3 {
                return None; // mul/div は divide_error の配送ごと従来経路に任せる
            }
            if op == 0xF6 {
                let imm = if kind <= 1 {
                    let v = *b.get(i)?;
                    i += 1;
                    v
                } else {
                    0
                };
                Uop::Grp3b { kind, rm, imm }
            } else {
                let imm = if kind <= 1 {
                    let v = u32le(b, i)?;
                    i += 4;
                    v
                } else {
                    0
                };
                Uop::Grp3w { kind, rm, imm }
            }
        }
        0xFF => {
            let (kind, rm) = dec_modrm(b, &mut i, seg_override)?;
            match kind {
                0 | 1 | 2 | 4 | 6 => Uop::Grp5 { kind, rm },
                _ => return None, // far call/jmp と予約は従来経路
            }
        }
        0xE8 => {
            let rel = u32le(b, i)?;
            i += 4;
            Uop::CallRel { rel }
        }
        0xE9 => {
            let rel = u32le(b, i)?;
            i += 4;
            Uop::JmpRel { rel }
        }
        0xEB => {
            let rel = *b.get(i)? as i8 as i32 as u32;
            i += 1;
            Uop::JmpRel { rel }
        }
        0x0F => {
            let op2 = *b.get(i)?;
            i += 1;
            match op2 {
                0x80..=0x8F => {
                    let rel = u32le(b, i)?;
                    i += 4;
                    Uop::Jcc { cc: op2 & 0xF, rel }
                }
                0x90..=0x9F => {
                    let (_, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::SetCC { cc: op2 & 0xF, rm }
                }
                0xAF => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::ImulRRm { reg, rm }
                }
                0xB6 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::MovzxB { reg, rm }
                }
                0xB7 => {
                    let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
                    Uop::MovzxW { reg, rm }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((i as u8, uop))
}
