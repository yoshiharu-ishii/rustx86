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
    if m.cpu.pe() && idx != CS && sel & !0x7 != 0 {
        let off = (sel & !0x7) as u32;
        let a = m.cpu.gdtr_base.wrapping_add(off);
        let access = ((m.read32(a.wrapping_add(4)) >> 8) & 0xFF) as u8;
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

/// セグメントレジスタへ記述子を写す (**特権チェック無し**)。
/// CPUが内部でやるロード — ゲートのCS、リング遷移のSS0、iretの復帰 — 用。
pub(crate) fn load_seg_raw(m: &mut Machine, idx: usize, sel: u16) {
    if !m.cpu.pe() {
        m.cpu.sregs[idx] = sel;
        m.cpu.hidden[idx] = SegHidden::real(sel);
        return;
    }
    // ヌルセレクタ: 写しを空にする。**使った瞬間に咎める**のは後の仕事
    if sel & !0x7 == 0 {
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
    if off + 7 > m.cpu.gdtr_limit as u32 {
        panic!(
            "selector {sel:#06x} is beyond GDT limit {:#06x}",
            m.cpu.gdtr_limit
        );
    }
    if sel & 0x4 != 0 {
        panic!("LDT selector {sel:#06x} (LDT is not implemented)");
    }
    // 記述子8バイト。baseとlimitが細切れなのは、286の6バイト記述子に
    // 後方互換の形で32bit分の桁を継ぎ足したため (ここにも地層がある)
    let a = m.cpu.gdtr_base.wrapping_add(off);
    let lo = m.read32(a);
    let hi = m.read32(a.wrapping_add(4));
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
