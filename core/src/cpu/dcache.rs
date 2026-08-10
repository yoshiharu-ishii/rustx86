//! デコード済み命令キャッシュ (ADR-0007 の本丸、P1a)。
//!
//! 同じ命令を何百万回もデコードし直すのをやめる。物理アドレスをキーに
//! デコード結果 (Uop) を控え、2回目からはデコードを飛ばして実行だけを行う。
//! 実CPUのuopキャッシュ、QEMU TCG のTBと同じ系譜の答えである。
//!
//! ## 対象は実測の上位だけ (opstats で選定)
//!
//! Linuxブート624M命令の実測で、mov (89/8B) 24% / ALUグリッド ~16% /
//! jcc ~10% / lea 4.7% / シフト 4.5% / movzx 3.8% / test 3.5% が上位。
//! これらを Uop 化し、**それ以外は従来の `cpu::step` にそのまま落とす**。
//! 全命令を一気にIR化しない — フォールバックがあるから安全に刻める。
//!
//! ## 意味を変えないための約束
//!
//! - 実行器は従来経路と**同じヘルパ** (alu8/alu_w/shift_rot/condition/
//!   push_w/pop_w) を呼ぶ。意味論を二重実装しない
//! - 0x66/0x67/REP 付きは対象外 (従来経路へ)。prefixed_ops の観測も保たれる
//! - ページを跨ぐ命令は控えない — 無効化の世代がページ単位のため
//! - デバッガONのときは使わない (before_exec/トレースの意味を守る)
//! - 16bitコードは対象外 (ELKS/FreeDOSは従来経路のまま)
//!
//! ## 自己書き換えは書き込みで受ける
//!
//! DOSどころかLinuxも起動時にコードを書き換える (alternatives/jump label)。
//! TLB・VRAM検出と同じ発想で、**コードを控えたページへの書き込み**が
//! そのページの世代を進め、古い控えは照合で外れる。データページへの
//! 書き込みは has_code の1判定だけで素通りする。

use super::alu::{alu8, alu_w, condition, set_szp_w};
use super::operand::{pop_w, push_w};
use super::shift::shift_rot;
use super::{Decoder, AF, AX, BP, CF, CS, DS, OF, SP, SS};
use crate::Machine;

/// 直接マップのスロット数。ブートの熱い命令アドレス集合を覆う広さと、
/// ホストのキャッシュに収まる小ささの折り合い (768KB)。要調整なら実測で
const SLOTS: usize = 32 * 1024;

const TAG_INVALID: u32 = 0xFFFF_FFFF;

/// メモリオペランドの形。**解決済みの番地ではなく作り方**を持つ —
/// 実効アドレスはレジスタの今の値から実行のたびに組む
#[derive(Clone, Copy)]
pub(crate) struct MemRef {
    /// 基底レジスタ (-1 = 無し)
    base: i8,
    /// インデックスレジスタ (-1 = 無し)
    index: i8,
    scale: u8,
    /// セグメント (デコード時に上書き規則まで解決済み)
    seg: u8,
    disp: u32,
}

#[derive(Clone, Copy)]
pub(crate) enum Rm {
    Reg(u8),
    Mem(MemRef),
}

/// デコード済み命令。従来経路の各armと1対1で対応する
#[derive(Clone, Copy)]
pub(crate) enum Uop {
    /// 89: mov r/m32, r32
    MovRmR {
        rm: Rm,
        reg: u8,
    },
    /// 8B: mov r32, r/m32
    MovRRm {
        reg: u8,
        rm: Rm,
    },
    /// 88 / 8A (8bit)
    Mov8RmR {
        rm: Rm,
        reg: u8,
    },
    Mov8RRm {
        reg: u8,
        rm: Rm,
    },
    /// B8-BF: mov r32, imm32
    MovRImm {
        reg: u8,
        imm: u32,
    },
    /// ALUグリッド op&7==1 (01/09/…/39): kind = (op>>3)&7
    AluRmR {
        kind: u8,
        rm: Rm,
        reg: u8,
    },
    /// op&7==3 (03/0B/…/3B)
    AluRRm {
        kind: u8,
        reg: u8,
        rm: Rm,
    },
    /// op&7==0 / 2 (8bit)
    Alu8RmR {
        kind: u8,
        rm: Rm,
        reg: u8,
    },
    Alu8RRm {
        kind: u8,
        reg: u8,
        rm: Rm,
    },
    /// op&7==5: eAX, imm
    AluAImm {
        kind: u8,
        imm: u32,
    },
    /// op&7==4: AL, imm8
    Alu8AImm {
        kind: u8,
        imm: u8,
    },
    /// 81/83: GRP1 r/m32, imm (0x83の符号拡張はデコード時に済ませてある)
    Grp1RmImm {
        kind: u8,
        rm: Rm,
        imm: u32,
    },
    /// 80: GRP1 r/m8, imm8
    Grp18RmImm {
        kind: u8,
        rm: Rm,
        imm: u8,
    },
    /// 85 / 84: test
    TestRmR {
        rm: Rm,
        reg: u8,
    },
    Test8RmR {
        rm: Rm,
        reg: u8,
    },
    /// 8D: lea (セグメントを適用しない実効オフセット)
    Lea {
        reg: u8,
        mem: MemRef,
    },
    /// 70-7F: jcc rel8 / 0F 80-8F: jcc rel32 (relは拡張済み)
    Jcc {
        cc: u8,
        rel: u32,
    },
    /// E9 / EB
    JmpRel {
        rel: u32,
    },
    /// E8
    CallRel {
        rel: u32,
    },
    /// C3
    Ret,
    /// 50-57 / 58-5F
    PushR {
        reg: u8,
    },
    PopR {
        reg: u8,
    },
    /// C1: shift r/m32, imm8 / D3: shift r/m32, CL (kindはModRMのreg欄)
    ShiftRmImm {
        kind: u8,
        rm: Rm,
        count: u8,
    },
    ShiftRmCl {
        kind: u8,
        rm: Rm,
    },
    /// 0F B6: movzx r32, r/m8
    MovzxB {
        reg: u8,
        rm: Rm,
    },
    // ---- ここから P1b (フォールバック43Mの実測上位から追加) ----
    /// 40-47 / 48-4F: inc/dec r32 (CFを触らないのがADD/SUBとの違い)
    IncR {
        reg: u8,
    },
    DecR {
        reg: u8,
    },
    /// C7 /any: mov r/m32, imm32 (regは無視 — 従来経路と同じ) / C6: 8bit
    MovRmImm {
        rm: Rm,
        imm: u32,
    },
    MovRm8Imm {
        rm: Rm,
        imm: u8,
    },
    /// A1 / A3: mov eax↔moffs32 (segは解決済み)
    MovAMoffs {
        load: bool,
        seg: u8,
        off: u32,
    },
    /// A0 / A2 (8bit)
    Mov8AMoffs {
        load: bool,
        seg: u8,
        off: u32,
    },
    /// 68 / 6A: push imm (6Aの符号拡張はデコード時に済み)
    PushImm {
        imm: u32,
    },
    /// C9: leave
    Leave,
    /// 90-97: xchg eAX, r (90はnop = 自分と交換)
    XchgAR {
        reg: u8,
    },
    /// F6 / F7 の kind 0-3 (test imm / not / neg)。mul/divは従来経路
    Grp3b {
        kind: u8,
        rm: Rm,
        imm: u8,
    },
    Grp3w {
        kind: u8,
        rm: Rm,
        imm: u32,
    },
    /// FF: inc/dec r/m (0/1)、call間接 (2)、jmp間接 (4)、push r/m (6)
    Grp5 {
        kind: u8,
        rm: Rm,
    },
    /// 0F 90-9F: setcc r/m8
    SetCC {
        cc: u8,
        rm: Rm,
    },
    /// 0F B7: movzx r32, r/m16
    MovzxW {
        reg: u8,
        rm: Rm,
    },
    /// 0F AF: imul r32, r/m32
    ImulRRm {
        reg: u8,
        rm: Rm,
    },
    /// A4-AF (REPなしの単発ストリング命令)。意味論は従来の string::exec に委譲
    StrOne {
        op: u8,
        seg: i8,
    },
}

#[derive(Clone, Copy)]
struct Entry {
    /// 命令先頭の物理アドレス (TAG_INVALID = 空き)
    tag: u32,
    /// 控えたときのページ世代。ページに書き込みがあると合わなくなる
    gen: u32,
    len: u8,
    uop: Uop,
}

pub struct DecodeCache {
    /// 直接マップ。**最初の32bitデコードまで確保しない** —
    /// 16bit機やcosimの単発Machineに768KBずつ払わせない
    entries: Vec<Entry>,
    /// 物理4Kページごとの世代。書き込みで進む
    page_gen: Vec<u32>,
    /// そのページにデコード済みコードがあるか。
    /// データページへの書き込みをタダにするための1判定
    page_has_code: Vec<bool>,
    /// 観測: ヒット / 新規デコード / 対象外 (従来経路行き)
    pub hits: u64,
    pub fills: u64,
    pub fallbacks: u64,
}

impl DecodeCache {
    pub fn new(ram_bytes: usize) -> Self {
        let pages = ram_bytes.div_ceil(4096);
        DecodeCache {
            entries: Vec::new(),
            page_gen: vec![0; pages],
            page_has_code: vec![false; pages],
            hits: 0,
            fills: 0,
            fallbacks: 0,
        }
    }

    /// 物理1バイト書き込みの通知。コードを控えたページだけ世代を進める
    #[inline]
    pub(crate) fn note_write(&mut self, pa: u32) {
        let p = (pa >> 12) as usize;
        if let Some(has) = self.page_has_code.get_mut(p) {
            if *has {
                *has = false;
                self.page_gen[p] = self.page_gen[p].wrapping_add(1);
            }
        }
    }

    /// 範囲書き込みの通知 (REP一括処理など、write_phys8を通らない道)
    pub(crate) fn note_write_range(&mut self, pa: u32, len: usize) {
        if len == 0 {
            return;
        }
        let first = (pa >> 12) as usize;
        let last = ((pa as usize).saturating_add(len - 1)) >> 12;
        for p in first..=last {
            if let Some(has) = self.page_has_code.get_mut(p) {
                if *has {
                    *has = false;
                    self.page_gen[p] = self.page_gen[p].wrapping_add(1);
                }
            }
        }
    }
}

/// キャッシュ経由の1命令実行。対象外は従来の [`super::step`] へ落ちる
pub(crate) fn step_cached(m: &mut Machine) {
    // 16bitコードは対象外 (ELKS/FreeDOSは従来経路)
    if !m.cpu.seg_is32(CS) {
        return super::step(m);
    }
    let lin = m.cpu.lin(CS, m.cpu.ip);
    let Ok(pa) = m.translate_for(lin, false) else {
        // フェッチがフォールトする状況は従来経路に任せる (#PF配送もそちら)
        return super::step(m);
    };
    let page = (pa >> 12) as usize;
    let slot = (pa as usize) & (SLOTS - 1);

    if !m.dcache.entries.is_empty() {
        let e = &m.dcache.entries[slot];
        if e.tag == pa && e.gen == m.dcache.page_gen.get(page).copied().unwrap_or(0) {
            let (len, uop) = (e.len, e.uop);
            m.dcache.hits += 1;
            m.cpu.advance_ip(len as u32);
            exec(m, uop);
            return;
        }
    }

    match decode_at(m, pa) {
        Some((len, uop)) => {
            if m.dcache.entries.is_empty() {
                m.dcache.entries = vec![
                    Entry {
                        tag: TAG_INVALID,
                        gen: 0,
                        len: 0,
                        uop: Uop::Ret,
                    };
                    SLOTS
                ];
            }
            let gen = m.dcache.page_gen.get(page).copied().unwrap_or(0);
            m.dcache.entries[slot] = Entry {
                tag: pa,
                gen,
                len,
                uop,
            };
            if let Some(h) = m.dcache.page_has_code.get_mut(page) {
                *h = true;
            }
            m.dcache.fills += 1;
            m.cpu.advance_ip(len as u32);
            exec(m, uop);
        }
        None => {
            m.dcache.fallbacks += 1;
            super::step(m);
        }
    }
}

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
fn decode_at(m: &Machine, pa: u32) -> Option<(u8, Uop)> {
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

    // プレフィクス。0x66/0x67/REPが来たら対象外 (従来経路が観測ごと面倒を見る)
    let op = loop {
        let x = *b.get(i)?;
        i += 1;
        match x {
            0x26 => seg_override = Some(super::ES as u8),
            0x2E => seg_override = Some(CS as u8),
            0x36 => seg_override = Some(SS as u8),
            0x3E => seg_override = Some(DS as u8),
            0x64 => seg_override = Some(super::FS as u8),
            0x65 => seg_override = Some(super::GS as u8),
            0xF0 => {} // LOCK: シングルコアなので順序の意味を持たない (従来経路と同じ)
            0x66 | 0x67 | 0xF2 | 0xF3 => return None,
            _ => break x,
        }
        if i >= 15 {
            return None;
        }
    };

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
            Uop::MovRmR { rm, reg }
        }
        0x8A => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::Mov8RRm { reg, rm }
        }
        0x8B => {
            let (reg, rm) = dec_modrm(b, &mut i, seg_override)?;
            Uop::MovRRm { reg, rm }
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
        0xA4..=0xA7 | 0xAA..=0xAF => Uop::StrOne {
            op,
            seg: seg_override.map(|s| s as i8).unwrap_or(-1),
        },
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

fn exec(m: &mut Machine, u: Uop) {
    match u {
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
            super::sp_write(m, bp);
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
            super::string::exec(m, &d, op);
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
