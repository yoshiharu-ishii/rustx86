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
