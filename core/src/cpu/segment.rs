//! セグメンテーション — セレクタの裏の「隠しレジスタ」。
//!
//! **これがプロテクトモードの正体**である ([ADR-0006](../../../docs/adr/0006-hidden-segment-registers.md))。
//! セグメントレジスタは「見える部分 (セレクタ)」と「隠し部分 (base/limit/属性)」の
//! 二層で、ロード命令だけがGDTを読んで隠し部分へ写す。以後のアクセスは写ししか
//! 見ない。リアルモードは「写しに常に sel×16 が入っている」特殊ケースになる。

use super::*;
use crate::Machine;

/// セグメントの隠しレジスタ1本分
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
/// **明示的な** セグメントロード (MOV Sreg / POP Sreg / far転送)。
/// ソフトウェアがやる操作なので特権チェックを受ける。
/// CPU内部のロード (ゲート・リング遷移・iret) は [`load_seg_raw`] を直に呼ぶ
pub(crate) fn load_seg(m: &mut Machine, idx: usize, sel: u16) {
    // データ/スタックの特権チェックは、GDTを引く前に「持てるか」を見る。
    // **DPL >= max(CPL, RPL)** — リング3がリング0のデータを覗くのを防ぐ、
    // 保護の一丁目。コードセグメント (CS) は far転送側の責任なのでここでは見ない
    if m.cpu.pe() && idx != CS && sel & !0x3 != 0 {
        let off = (sel & !0x7) as u32;
        let a = descriptor_table(m, sel).0.wrapping_add(off);
        // 記述子表の読みは暗黙のスーパーバイザアクセス (CPL=3でもU/S検査を受けない)
        let prev_sys = m.sys_access.replace(true);
        let access = ((m.read32(a.wrapping_add(4)) >> 8) & 0xFF) as u8;
        m.sys_access.set(prev_sys);
        if access & 0x10 != 0 && access & 0x08 == 0 {
            // コード以外 = データ/スタック
            let dpl = (access >> 5) & 3;
            let rpl = (sel & 3) as u8;
            let cpl = m.cpu.cpl();
            if dpl < cpl.max(rpl) {
                panic!(
                    "selector {sel:#06x}: DPL={dpl} < max(CPL={cpl}, RPL={rpl}) —                      general protection (まだ#GP配送は無いのでpanic)"
                );
            }
        }
    }
    load_seg_raw(m, idx, sel);
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
    use super::operand::{push32, push_w};
    if !m.cpu.pe() {
        let cs = m.cpu.sregs[CS] as u32;
        push_w(m, cs, wide);
        push_w(m, m.cpu.ip, wide);
        load_seg(m, CS, sel);
        m.cpu.set_ip(off);
        return;
    }
    let (lo, hi) = read_descriptor(m, sel);
    let access = ((hi >> 8) & 0xFF) as u8;
    if access & 0x10 != 0 {
        // ふつうのコードセグメント
        let cs = m.cpu.sregs[CS] as u32;
        push_w(m, cs, wide);
        push_w(m, m.cpu.ip, wide);
        load_seg(m, CS, sel);
        m.cpu.set_ip(off);
        return;
    }
    match access & 0x1F {
        // 386コールゲート。ゲートが持つのは「行き先」と「引数のdword数」
        0x0C => {
            let gate_sel = (lo >> 16) as u16;
            let gate_off = (lo & 0xFFFF) | (hi & 0xFFFF_0000);
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
                push32(m, old_ss as u32);
                push32(m, old_esp);
                // 引数を旧スタックから写す (呼び出し側が積んだ順のまま)
                for i in (0..parc).rev() {
                    let v = m.read32(old_ss_base.wrapping_add(old_esp).wrapping_add(4 * i));
                    push32(m, v);
                }
                let cs = m.cpu.sregs[CS] as u32;
                push32(m, cs);
                push32(m, m.cpu.ip);
                // 行き先のCSはRPL=行き先DPL (=新しいCPL) で据える
                load_seg_raw(m, CS, (gate_sel & !0x3) | target_dpl as u16);
                m.cpu.set_ip(gate_off);
            } else {
                // 同じリング: 行き先だけゲートが決める
                let cs = m.cpu.sregs[CS] as u32;
                push32(m, cs);
                push32(m, m.cpu.ip);
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
        let new_cpl = (sel & 3) as u8;
        // 呼び出し側が積んだ引数 (extra_pop) は**外側のESPに足す**前に、
        // まず内側スタックの ESP/SS を取り出す
        let esp = pop_w(m, wide);
        let ss = pop_w(m, wide) as u16;
        load_seg_raw(m, CS, sel);
        m.cpu.set_ip(ip);
        load_seg_raw(m, SS, ss);
        m.cpu.regs[super::SP] = esp.wrapping_add(extra_pop);
        null_unreachable_data_segs(m, new_cpl);
    } else {
        load_seg(m, CS, sel);
        m.cpu.set_ip(ip);
        let sp = sp_read(m).wrapping_add(extra_pop);
        sp_write(m, sp);
    }
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
    if !m.cpu.pe() {
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
    let (tbl_base, tbl_limit) = descriptor_table(m, sel);
    if off + 7 > tbl_limit {
        panic!(
            "selector {sel:#06x} is beyond {} limit {tbl_limit:#06x}",
            if sel & 0x4 != 0 { "LDT" } else { "GDT" },
        );
    }
    // 記述子8バイト。baseとlimitが細切れなのは、286の6バイト記述子に
    // 後方互換の形で32bit分の桁を継ぎ足したため (ここにも地層がある)
    let a = tbl_base.wrapping_add(off);
    let prev_sys = m.sys_access.replace(true);
    let lo = m.read32(a);
    let hi = m.read32(a.wrapping_add(4));
    m.sys_access.set(prev_sys);
    let base = (lo >> 16) | ((hi & 0xFF) << 16) | (hi & 0xFF00_0000);
    let mut limit = (lo & 0xFFFF) | (hi & 0x000F_0000);
    let access = ((hi >> 8) & 0xFF) as u8;
    if hi & 0x0080_0000 != 0 {
        // Gビット: limitの単位が4Kページになる
        limit = (limit << 12) | 0xFFF;
    }
    if access & 0x80 == 0 {
        panic!("selector {sel:#06x}: descriptor not present");
    }
    m.cpu.sregs[idx] = sel;
    m.cpu.hidden[idx] = SegHidden {
        base,
        limit,
        access,
        big: hi & 0x0040_0000 != 0, // Dビット
    };
}
