//! ストリング命令 (MOVS/CMPS/STOS/LODS/SCAS) とREPプレフィクス。
//!
//! REP付きはカウンタが尽きるまで自前でループする。CMPS/SCASはZFを見て
//! 打ち切る (REPE/REPNE) ため、単純な繰り返しにはできない。
//!
//! **幅は2軸で決まる**:
//!   - オペランドサイズ: 転送1個のバイト数。偶数オペコード=1、奇数=2 or 4
//!     (0xA5 は 66なしで MOVSD=4、ありで MOVSW=2)
//!   - アドレスサイズ: カウンタとインデックスが ECX/ESI/EDI(32bit) か
//!     CX/SI/DI(16bit ラップ) か
//!
//! 昔は16bit固定で書いていて、Linuxデコンプレッサの `rep movsl` が
//! 2バイトずつ・CXカウントで回り、再配置コピーが途中で尽きて墜落した。

use super::alu::{alu16, alu32, alu8};
use super::{Cpu, Decoder, AX, CX, DF, DI, DS, ES, SI, ZF};
use crate::Machine;

/// メモリを幅ぶん読む (1/2/4バイト)
fn read_w(m: &Machine, lin: u32, width: u32) -> u32 {
    match width {
        1 => m.read8(lin) as u32,
        2 => m.read16(lin) as u32,
        _ => m.read32(lin),
    }
}

/// メモリへ幅ぶん書く (1/2/4バイト)
fn write_w(m: &mut Machine, lin: u32, v: u32, width: u32) {
    match width {
        1 => m.write8(lin, v as u8),
        2 => m.write16(lin, v as u16),
        _ => m.write32(lin, v),
    }
}

/// フラグだけ立てる比較 (CMPS/SCAS 用)。幅ごとに正しいALUを呼ぶ
fn cmp_w(c: &mut Cpu, a: u32, b: u32, width: u32) {
    match width {
        1 => {
            alu8(c, 7, a as u8, b as u8);
        }
        2 => {
            alu16(c, 7, a as u16, b as u16);
        }
        _ => {
            alu32(c, 7, a, b);
        }
    }
}

/// インデックスレジスタを DF 方向へ width だけ進める。
/// a32 なら 32bit 全体、そうでなければ 16bit ラップ (上位を保つ)
fn advance(c: &mut Cpu, reg: usize, a32: bool, width: u32) {
    let delta = if c.flag(DF) {
        width.wrapping_neg()
    } else {
        width
    };
    let cur = c.reg_w(reg, a32);
    c.set_reg_w(reg, cur.wrapping_add(delta), a32);
}

/// REP STOS の一括処理 (前進・DF=0)。ページ境界ごとにまとめて埋める。
/// フォールトや VRAM に当たったら、その手前で止めて残りをスカラへ返す
fn bulk_stos(m: &mut Machine, width: u32, a32: bool) {
    let pat = m.cpu.reg_w(AX, width == 4).to_le_bytes();
    loop {
        let cx = m.cpu.reg_w(CX, a32);
        if cx == 0 {
            return;
        }
        let di = m.cpu.reg_w(DI, a32);
        let la = m.cpu.lin(ES, di);
        // ページ境界を跨ぐ1要素は、この道では扱わない (スカラへ返す)
        if (la & 0xFFF) + width > 0x1000 {
            return;
        }
        let (pa, page_remain) = match m.phys_span(la, true) {
            Some(x) => x,
            None => return, // フォールトは記録済み。スカラループが止まる
        };
        // VRAM窓に落ちるなら遅い道 (dirtyの合図が要る)
        if (pa as u32) <= crate::bus::VRAM_TEXT_END
            && (pa as u32 + page_remain as u32) > crate::bus::VRAM_TEXT_BASE
        {
            return;
        }
        // このページで埋められる要素数 (CXと残りバイトの小さい方)
        let n = cx.min((page_remain as u32) / width);
        if n == 0 {
            return;
        }
        let bytes = (n * width) as usize;
        let mem = m.mem_slice_mut();
        match width {
            1 => mem[pa..pa + bytes].fill(pat[0]),
            _ => {
                for chunk in mem[pa..pa + bytes].chunks_exact_mut(width as usize) {
                    chunk.copy_from_slice(&pat[..width as usize]);
                }
            }
        }
        m.cpu.set_reg_w(DI, di.wrapping_add(bytes as u32), a32);
        m.cpu.set_reg_w(CX, cx - n, a32);
    }
}

/// REP MOVS の一括処理 (前進・DF=0)。src と dest 両方のページ内で
/// 連続する分をまとめてコピーする
fn bulk_movs(m: &mut Machine, src_seg: usize, width: u32, a32: bool) {
    loop {
        let cx = m.cpu.reg_w(CX, a32);
        if cx == 0 {
            return;
        }
        let si = m.cpu.reg_w(SI, a32);
        let di = m.cpu.reg_w(DI, a32);
        let src_la = m.cpu.lin(src_seg, si);
        let dst_la = m.cpu.lin(ES, di);
        if (src_la & 0xFFF) + width > 0x1000 || (dst_la & 0xFFF) + width > 0x1000 {
            return; // 跨ぎはスカラへ
        }
        // 読み側を先に (フォールトしたら書かない)
        let (spa, s_remain) = match m.phys_span(src_la, false) {
            Some(x) => x,
            None => return,
        };
        let (dpa, d_remain) = match m.phys_span(dst_la, true) {
            Some(x) => x,
            None => return,
        };
        if (dpa as u32) <= crate::bus::VRAM_TEXT_END
            && (dpa as u32 + d_remain as u32) > crate::bus::VRAM_TEXT_BASE
        {
            return;
        }
        let n = cx
            .min((s_remain as u32) / width)
            .min((d_remain as u32) / width);
        if n == 0 {
            return;
        }
        let bytes = (n * width) as usize;
        // src と dest は別ページなので範囲は重ならない。copy_within は使わず
        // 一時コピーで安全に (重なり得ないが借用を分けるため)
        let mem = m.mem_slice_mut();
        mem.copy_within(spa..spa + bytes, dpa);
        m.cpu.set_reg_w(SI, si.wrapping_add(bytes as u32), a32);
        m.cpu.set_reg_w(DI, di.wrapping_add(bytes as u32), a32);
        m.cpu.set_reg_w(CX, cx - n, a32);
    }
}

/// ストリング命令1個を実行する (REPがあればカウンタが尽きるまで繰り返す)
pub fn exec(m: &mut Machine, d: &Decoder, op: u8) {
    let a32 = d.addrsize32;
    // 転送幅: 偶数=1バイト、奇数=オペランドサイズ (66で 2、無しで 4)
    let width: u32 = if op & 1 == 0 {
        1
    } else if d.opsize32 {
        4
    } else {
        2
    };
    let src_seg = d.seg_override.unwrap_or(DS);

    // --- 一括処理の速い道 (起動時の大量ゼロ埋め/コピーを桁で速くする) ---
    //
    // カーネルの初期化は BSS・ページテーブル・スラブ・initramfs展開で
    // 何百MBもの `rep stosl` / `rep movsl` を回す。1要素ごとに変換+書きを
    // していたのが起動の遅さの正体だった。**前進・REP付き・DF=0**という
    // 圧倒的多数のケースだけ、ページ単位でまとめて処理する。
    // 端 (ページ境界・フォールト・跨ぎ) は下のスカラループが拾う。
    //
    // **32bitアドレス限定**。16bitリアルモードの string は小さく、しかも
    // 1MB折り返しが 4Kページ前提と噛み合わないので、速い道には乗せない
    if a32 && d.rep.is_some() && !m.cpu.flag(DF) && !m.dbg.on {
        if op == 0xAA || op == 0xAB {
            bulk_stos(m, width, a32);
        } else if op == 0xA4 || op == 0xA5 {
            bulk_movs(m, src_seg, width, a32);
        }
        // 一括で尽きた/端に達したら、残りは下のループが処理する
    }

    loop {
        // ページフォールトが起きたら**その反復を確定せずに**止める。
        // 命令ごと巻き戻され、CX/SI/DIは完了済みの反復だけを指しているので、
        // ハンドラ復帰後の再実行が続きから正しく再開する (実機のREP再開と同じ)
        if m.pending_fault.get().is_some() {
            break;
        }
        if d.rep.is_some() && m.cpu.reg_w(CX, a32) == 0 {
            break;
        }
        let si = m.cpu.reg_w(SI, a32);
        let di = m.cpu.reg_w(DI, a32);
        match op {
            0xA4 | 0xA5 => {
                // MOVS。読みがフォールトしたらゴミを書かずに止める
                let v = read_w(m, m.cpu.lin(src_seg, si), width);
                if m.pending_fault.get().is_some() {
                    break;
                }
                write_w(m, m.cpu.lin(ES, di), v, width);
                if m.pending_fault.get().is_some() {
                    break;
                }
                advance(&mut m.cpu, SI, a32, width);
                advance(&mut m.cpu, DI, a32, width);
            }
            0xA6 | 0xA7 => {
                // CMPS
                let a = read_w(m, m.cpu.lin(src_seg, si), width);
                let b = read_w(m, m.cpu.lin(ES, di), width);
                cmp_w(&mut m.cpu, a, b, width);
                advance(&mut m.cpu, SI, a32, width);
                advance(&mut m.cpu, DI, a32, width);
            }
            0xAA | 0xAB => {
                // STOS
                let v = m.cpu.reg_w(AX, width == 4);
                let v = if width == 1 { v & 0xFF } else { v };
                write_w(m, m.cpu.lin(ES, di), v, width);
                advance(&mut m.cpu, DI, a32, width);
            }
            0xAC | 0xAD => {
                // LODS
                let v = read_w(m, m.cpu.lin(src_seg, si), width);
                match width {
                    1 => m.cpu.set_reg8(0, v as u8),
                    2 => m.cpu.set_reg16(AX, v as u16),
                    _ => m.cpu.set_reg32(AX, v),
                }
                advance(&mut m.cpu, SI, a32, width);
            }
            _ => {
                // SCAS
                let a = m.cpu.reg_w(AX, width == 4);
                let a = if width == 1 { a & 0xFF } else { a };
                let b = read_w(m, m.cpu.lin(ES, di), width);
                cmp_w(&mut m.cpu, a, b, width);
                advance(&mut m.cpu, DI, a32, width);
            }
        }
        match d.rep {
            None => break,
            Some(prefix) => {
                let cx = m.cpu.reg_w(CX, a32).wrapping_sub(1);
                m.cpu.set_reg_w(CX, cx, a32);
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
