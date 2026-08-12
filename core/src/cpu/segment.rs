//! セグメンテーション — セレクタの裏の「隠しレジスタ」。
//!
//! **これがプロテクトモードの正体**である ([ADR-0006](../../../docs/adr/0006-hidden-segment-registers.md))。
//! セグメントレジスタは「見える部分 (セレクタ)」と「隠し部分 (base/limit/属性)」の
//! 二層で、ロード命令だけがGDTを読んで隠し部分へ写す。以後のアクセスは写ししか
//! 見ない。リアルモードは「写しに常に sel×16 が入っている」特殊ケースになる。

use super::*;
use crate::Machine;

/// セグメントの隠しレジスタ1本分。
/// repr(C)は歴代の外部ビュー (cosim/スナップショット) との契約 —
/// フィールドの並びを安定させておく
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegHidden {
    pub base: u32,
    pub limit: u32,
    /// 記述子のaccessバイト (P/DPL/type)
    pub access: u8,
    /// Dビット。コードセグメントなら既定オペランド幅が32bitになる
    pub big: bool,
}

impl SegHidden {
    /// 平坦か: base=0・limit=4GB・present な書けるデータ (伸長方向は通常)。
    /// **Linuxの常態**であり、このときデータアクセスの検査 (limit・書込可否・
    /// ヌル) は読み書きどちらでも恒真、base加算も恒等 — data_addr が
    /// 1分岐で素通しする根拠 (S1、互換税+10%の取り返し)。
    /// マスク 0x9E = P|S|code|E|W、値 0x92 = P=1,S=1,データ,通常伸長,W=1
    #[inline]
    pub fn flat_rw(&self) -> bool {
        self.base == 0 && self.limit == 0xFFFF_FFFF && self.access & 0x9E == 0x92
    }

    /// リアルモードの写し: base = sel×16、64K、16bit
    pub(crate) fn real(sel: u16) -> Self {
        Self {
            base: (sel as u32) << 4,
            limit: 0xFFFF,
            access: 0x93, // present, data, writable相当
            big: false,
        }
    }
}

/// GDTから記述子を読んで、セグメントの隠しレジスタへ写す。
///
/// **実機がセグメントロードのたびにやっていることそのもの**である。
/// ここで写した base/limit/Dビット だけが以後のアクセスに使われ、
/// GDT本体は次のロードまで見られない。
///
/// 特権チェック (DPL/RPL/CPL) はまだ実装しない。リング0だけの世界では
/// 全部 0=0 で恒真になるためで、リング3を作るときに一緒に入れる。
/// **黙って通す場合とは違い、チェックすべき材料 (access) は写してある**
/// フォールト扱いのセグメント例外 (#GP=13 / #NP=11 / #SS=12)。
///
/// **命令の頭 (trap_ip) へ巻き戻してから配送する** — セグメントロードに
/// 失敗した命令は何も起きなかったことになる (#PFと同じ約束)。だから
/// 呼び出し側は「先に検査、通ってから状態を書く」順で組むこと。
/// エラーコードはセレクタの index+TI (RPLの2bitはEXT/IDTフラグの席なので落とす)
fn seg_exc(m: &mut Machine, vector: u8, sel: u16) {
    m.cpu.set_ip(m.trap_ip);
    let (cs, ip) = (m.cpu.sregs[CS], m.cpu.ip);
    if m.first_fault.is_none() {
        m.first_fault = Some((vector, cs, ip));
    }
    m.int_counts[vector as usize] += 1;
    if m.int_recent.len() == 32 {
        m.int_recent.pop_front();
    }
    m.int_recent.push_back((vector, cs, ip));
    super::interrupt::interrupt_protected_err(m, vector, Some((sel & 0xFFFC) as u32));
}

/// **明示的な** セグメントロード (MOV Sreg / POP Sreg / far転送)。
/// ソフトウェアがやる操作なので、実CPUと同じ検査を受けて失敗は例外になる。
/// **false = 例外を配送した (状態は書いていない)** — 呼び出し側は即 return し、
/// この命令の残りの効果 (SPの確定・レジスタ書き込み) を捨てること。
/// CPU内部のロード (ゲート・リング遷移・iret) は [`load_seg_raw`] を直に呼ぶ
#[must_use]
pub(crate) fn load_seg(m: &mut Machine, idx: usize, sel: u16) -> bool {
    // V86もリアルモード同様: 記述子表は引かず sel×16。検査も無い
    // (檻の外に出る手段がセグメントロードに無いから許される)
    if !m.cpu.pe() || m.cpu.vm86() {
        m.cpu.sregs[idx] = sel;
        m.cpu.hidden[idx] = SegHidden::real(sel);
        return true;
    }
    let cpl = m.cpu.cpl();
    let rpl = (sel & 3) as u8;
    // ヌルセレクタ: SSには積めない (#GP(0))。他は空にして、使った瞬間に咎める
    if sel & !0x3 == 0 {
        if idx == SS {
            seg_exc(m, 13, 0);
            return false;
        }
        m.cpu.sregs[idx] = sel;
        m.cpu.hidden[idx] = SegHidden {
            base: 0,
            limit: 0,
            access: 0,
            big: false,
        };
        return true;
    }
    // 表の範囲外は #GP
    let off = (sel & !0x7) as u32;
    if off + 7 > descriptor_table(m, sel).1 {
        seg_exc(m, 13, sel);
        return false;
    }
    let (lo, hi) = read_descriptor(m, sel);
    let access = ((hi >> 8) & 0xFF) as u8;
    let s = access & 0x10 != 0; // 1=コード/データ、0=システム記述子
    let code = access & 0x08 != 0;
    let dpl = (access >> 5) & 3;
    let present = access & 0x80 != 0;
    let mut sel = sel;
    match idx {
        // SS: 書けるデータ、RPL==CPL、DPL==CPL。不在だけは #SS
        SS => {
            let writable_data = s && !code && access & 0x02 != 0;
            if !writable_data || rpl != cpl || dpl != cpl {
                seg_exc(m, 13, sel);
                return false;
            }
            if !present {
                seg_exc(m, 12, sel);
                return false;
            }
        }
        // CS (far jmp/ret経由): コードであること + 特権規則。
        // 非適合は RPL<=CPL かつ DPL==CPL、適合は DPL<=CPL。載るRPLはCPLに揃う
        CS => {
            if !s || !code {
                seg_exc(m, 13, sel);
                return false;
            }
            let conforming = access & 0x04 != 0;
            let ok = if conforming {
                dpl <= cpl
            } else {
                rpl <= cpl && dpl == cpl
            };
            if !ok {
                seg_exc(m, 13, sel);
                return false;
            }
            if !present {
                seg_exc(m, 11, sel);
                return false;
            }
            sel = (sel & !0x3) | cpl as u16;
        }
        // データセグメント: データか「読めるコード」。非適合なら DPL >= max(CPL,RPL)
        _ => {
            let readable = s && (!code || access & 0x02 != 0);
            if !readable {
                seg_exc(m, 13, sel);
                return false;
            }
            let conforming = code && access & 0x04 != 0;
            if !conforming && dpl < cpl.max(rpl) {
                seg_exc(m, 13, sel);
                return false;
            }
            if !present {
                seg_exc(m, 11, sel);
                return false;
            }
        }
    }
    install_seg(m, idx, sel, lo, hi);
    true
}

/// セレクタが引く記述子表の (base, limit)。TIビット (bit2) がGDT/LDTを選ぶ
pub(crate) fn descriptor_table(m: &Machine, sel: u16) -> (u32, u32) {
    if sel & 0x4 != 0 {
        (m.cpu.ldtr_base, m.cpu.ldtr_limit)
    } else {
        (m.cpu.gdtr_base, m.cpu.gdtr_limit as u32)
    }
}

/// 記述子8バイトを読む (lo, hi)。表の読みは暗黙のスーパーバイザアクセス
pub(crate) fn read_descriptor(m: &mut Machine, sel: u16) -> (u32, u32) {
    let a = descriptor_table(m, sel).0.wrapping_add((sel & !0x7) as u32);
    let prev_sys = m.sys_access.replace(true);
    let lo = m.read32(a);
    let hi = m.read32(a.wrapping_add(4));
    m.sys_access.set(prev_sys);
    (lo, hi)
}

/// far CALL の共通経路 (9A / FF /3)。
///
/// 保護モードでは、セレクタの指す記述子が**コードセグメントとは限らない** —
/// 386コールゲート (type 0xC) なら「ゲートの中のセレクタとオフセット」へ
/// 飛び、行き先のDPLが深ければ**リング遷移** (TSSから新スタック、旧SS:ESPを
/// 積み替え) になる。OSのシステムコール以前の、セグメント機構そのものの
/// リング渡り — test386のPOST 20 (switchToRing0) が要求する
pub(crate) fn far_call(m: &mut Machine, sel: u16, off: u32, wide: bool) {
    use super::operand::push_w;
    if !m.cpu.pe() {
        let cs = m.cpu.sregs[CS] as u32;
        push_w(m, cs, wide);
        push_w(m, m.cpu.ip, wide);
        let _ = load_seg(m, CS, sel);
        m.cpu.set_ip(off);
        return;
    }
    let (lo, hi) = read_descriptor(m, sel);
    let access = ((hi >> 8) & 0xFF) as u8;
    if access & 0x10 != 0 {
        // ふつうのコードセグメント。**検証を先に** — CSが載ってから復帰番地を
        // 積む (load_segが#GPしたら何も積まない。SPは同特権で不変なので順序自由)
        let (old_cs, ret) = (m.cpu.sregs[CS] as u32, m.cpu.ip);
        if load_seg(m, CS, sel) {
            push_w(m, old_cs, wide);
            push_w(m, ret, wide);
            m.cpu.set_ip(off);
        }
        return;
    }
    match access & 0x1F {
        // コールゲート (0x04=286の16bit / 0x0C=386の32bit)。ゲートが持つのは
        // 「行き先」と「引数の個数」。**ゲートの幅が積む幅を決める** — CALL命令の
        // オペランド幅ではない (16bitゲート越しはSS/SP/引数/CS/IP全部ワード)
        t @ (0x04 | 0x0C) => {
            let gate32 = t == 0x0C;
            let gate_sel = (lo >> 16) as u16;
            // 286ゲートはオフセット16bitのみ (上位16bitの席が記述子に無い)
            let gate_off = if gate32 {
                (lo & 0xFFFF) | (hi & 0xFFFF_0000)
            } else {
                lo & 0xFFFF
            };
            let parc = hi & 0x1F;
            let (_, thi) = read_descriptor(m, gate_sel);
            let target_dpl = ((thi >> 13) & 3) as u8;
            let cpl = m.cpu.cpl();
            if target_dpl < cpl {
                // ---- 深いリングへの遷移: スタックを差し替えてから積み直す ----
                let old_ss = m.cpu.sregs[SS];
                let old_esp = m.cpu.regs[super::SP];
                let old_ss_base = m.cpu.seg_base(SS);
                // 32bit TSS: DPL nの新スタックは +4+8n=ESPn, +8+8n=SSn
                let t = m.cpu.tr_base.wrapping_add(4 + 8 * target_dpl as u32);
                let new_esp = m.read32(t);
                let new_ss = m.read16(t.wrapping_add(4));
                load_seg_raw(m, SS, new_ss);
                m.cpu.regs[super::SP] = new_esp;
                push_w(m, old_ss as u32, gate32);
                push_w(m, old_esp, gate32);
                // 引数を旧スタックから写す (呼び出し側が積んだ順のまま)
                let unit = if gate32 { 4 } else { 2 };
                for i in (0..parc).rev() {
                    let a = old_ss_base.wrapping_add(old_esp).wrapping_add(unit * i);
                    let v = if gate32 {
                        m.read32(a)
                    } else {
                        m.read16(a) as u32
                    };
                    push_w(m, v, gate32);
                }
                let cs = m.cpu.sregs[CS] as u32;
                push_w(m, cs, gate32);
                push_w(m, m.cpu.ip, gate32);
                // 行き先のCSはRPL=行き先DPL (=新しいCPL) で据える
                load_seg_raw(m, CS, (gate_sel & !0x3) | target_dpl as u16);
                m.cpu.set_ip(gate_off);
            } else {
                // 同じリング: 行き先だけゲートが決める
                let cs = m.cpu.sregs[CS] as u32;
                push_w(m, cs, gate32);
                push_w(m, m.cpu.ip, gate32);
                load_seg_raw(m, CS, (gate_sel & !0x3) | cpl as u16);
                m.cpu.set_ip(gate_off);
            }
        }
        t => panic!("far call to system descriptor type {t:#04x} (未実装のゲート)"),
    }
}

/// far RET の共通経路 (CB / CA)。
///
/// 戻り先CSのRPLがいまのCPLより浅ければ**外側リングへの復帰**:
/// SS:ESPもスタックから取り出し、外側では持てないデータセグメントを
/// ヌルに落とす (実CPUの仕様 — カーネルのセグメントをリング3に残さない)
pub(crate) fn far_ret(m: &mut Machine, wide: bool, extra_pop: u32) {
    use super::operand::{pop_w, sp_read, sp_write};
    let ip = pop_w(m, wide);
    let sel = pop_w(m, wide) as u16;
    if m.cpu.pe() && ((sel & 3) as u8) > m.cpu.cpl() {
        // ---- 外側リングへの復帰 ----
        // 戻り先CSはコード・DPL==RPL・present でなければ #GP/#NP。
        // 実CPUの retf の規則で、載るCPLは戻り先CSのRPLになる
        let new_cpl = (sel & 3) as u8;
        if sel & !0x3 == 0 {
            seg_exc(m, 13, 0);
            return;
        }
        if (sel & !0x7) as u32 + 7 > descriptor_table(m, sel).1 {
            seg_exc(m, 13, sel);
            return;
        }
        let (lo, hi) = read_descriptor(m, sel);
        let access = ((hi >> 8) & 0xFF) as u8;
        let dpl = (access >> 5) & 3;
        if access & 0x18 != 0x18 || dpl != new_cpl {
            seg_exc(m, 13, sel);
            return;
        }
        if access & 0x80 == 0 {
            seg_exc(m, 11, sel);
            return;
        }
        // ゲート経由で来た引数の**写し**が内側スタックにも積まれている —
        // extra_pop は「内側で写しを捨てる」「外側で原本を捨てる」の二度使う
        // (Intel SDMのRET(n)外側復帰の順序)。写しを飛ばしてから ESP/SS を取り出す
        sp_write(m, sp_read(m).wrapping_add(extra_pop));
        let esp = pop_w(m, wide);
        let ss = pop_w(m, wide) as u16;
        // 戻り先SSは書けるデータ・RPL==新CPL・DPL==新CPL・present。
        // ヌルや不正は #GP、不在は #SS (実CPUの retf 外側復帰の規則)
        if let Some(v) = ss_load_fault(m, ss, new_cpl) {
            seg_exc(m, v, ss);
            return;
        }
        install_seg(m, CS, sel, lo, hi);
        m.cpu.set_ip(ip);
        load_seg_raw(m, SS, ss);
        m.cpu.regs[super::SP] = esp.wrapping_add(extra_pop);
        null_unreachable_data_segs(m, new_cpl);
    } else if load_seg(m, CS, sel) {
        m.cpu.set_ip(ip);
        let sp = sp_read(m).wrapping_add(extra_pop);
        sp_write(m, sp);
    }
}

/// SSに `sel` を CPL=`cpl` で積めるか検査する。積める=None、
/// ダメなら配送すべき例外ベクタ (#GP=13 / #SS=12)。
/// 規則: 書けるデータ・RPL==CPL・DPL==CPL、不在は #SS
fn ss_load_fault(m: &mut Machine, sel: u16, cpl: u8) -> Option<u8> {
    if sel & !0x3 == 0 || (sel & 3) as u8 != cpl {
        return Some(13);
    }
    if (sel & !0x7) as u32 + 7 > descriptor_table(m, sel).1 {
        return Some(13);
    }
    let (_, hi) = read_descriptor(m, sel);
    let access = ((hi >> 8) & 0xFF) as u8;
    let writable_data = access & 0x1A == 0x12; // S=1,code=0,writable=1
    let dpl = (access >> 5) & 3;
    if !writable_data || dpl != cpl {
        return Some(13);
    }
    if access & 0x80 == 0 {
        return Some(12); // #SS
    }
    None
}

/// 外側リングへ出るとき、そこでは持てないデータセグメントをヌルにする。
/// 条件: データ or 非適合コードで DPL < 新CPL (実CPUのiret/retfの仕様)
pub(crate) fn null_unreachable_data_segs(m: &mut Machine, new_cpl: u8) {
    for r in [super::DS, super::ES, super::FS, super::GS] {
        let h = m.cpu.hidden[r];
        if h.access & 0x80 == 0 {
            continue; // もともと空
        }
        let dpl = (h.access >> 5) & 3;
        let conforming_code = h.access & 0x1C == 0x1C;
        if !conforming_code && dpl < new_cpl {
            load_seg_raw(m, r, 0);
        }
    }
}

/// セグメントレジスタへ記述子を写す (**特権チェック無し**)。
/// CPUが内部でやるロード — ゲートのCS、リング遷移のSS0、iretの復帰 — 用。
pub(crate) fn load_seg_raw(m: &mut Machine, idx: usize, sel: u16) {
    if !m.cpu.pe() || m.cpu.vm86() {
        m.cpu.sregs[idx] = sel;
        m.cpu.hidden[idx] = SegHidden::real(sel);
        return;
    }
    // ヌルセレクタ: 写しを空にする。**使った瞬間に咎める**のは後の仕事。
    // ヌルの定義は「index=0 かつ TI=0」(RPLは不問) — !0x7でマスクすると
    // TI=1のLDT先頭 (0x0004、正当なセレクタ) までヌル扱いになる
    // (test386のPOST 09がDS=0x0004のbase消失として暴いた)
    if sel & !0x3 == 0 {
        m.cpu.sregs[idx] = sel;
        m.cpu.hidden[idx] = SegHidden {
            base: 0,
            limit: 0,
            access: 0,
            big: false,
        };
        return;
    }
    let off = (sel & !0x7) as u32;
    // TIビット (bit2): 0=GDT 1=LDT。LDTはLLDTで写した base/limit を使う
    let (_, tbl_limit) = descriptor_table(m, sel);
    if off + 7 > tbl_limit {
        panic!(
            "selector {sel:#06x} is beyond {} limit {tbl_limit:#06x}",
            if sel & 0x4 != 0 { "LDT" } else { "GDT" },
        );
    }
    let (lo, hi) = read_descriptor(m, sel);
    if hi & 0x8000 == 0 {
        panic!("selector {sel:#06x}: descriptor not present");
    }
    install_seg(m, idx, sel, lo, hi);
}

/// 記述子をセグメントの隠しレジスタへ写す (検査は済んでいる前提)。
/// baseとlimitが細切れなのは、286の6バイト記述子に後方互換の形で
/// 32bit分の桁を継ぎ足したため (ここにも地層がある)
fn install_seg(m: &mut Machine, idx: usize, sel: u16, lo: u32, hi: u32) {
    let base = (lo >> 16) | ((hi & 0xFF) << 16) | (hi & 0xFF00_0000);
    let mut limit = (lo & 0xFFFF) | (hi & 0x000F_0000);
    if hi & 0x0080_0000 != 0 {
        // Gビット: limitの単位が4Kページになる
        limit = (limit << 12) | 0xFFF;
    }
    m.cpu.sregs[idx] = sel;
    m.cpu.hidden[idx] = SegHidden {
        base,
        limit,
        access: ((hi >> 8) & 0xFF) as u8,
        big: hi & 0x0040_0000 != 0, // Dビット
    };
}
