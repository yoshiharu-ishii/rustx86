//! 0F で始まる二バイト命令 (386〜)。
//!
//! 1バイトの256席が埋まったので、`0F` をエスケープにしてもう1バイト読む。
//! ここが将来いちばん伸びる区画である — システム命令 (LGDT/LIDT/LTR/MOV CR)
//! がまず入り、フォーク先ではSSE/AVXやロングモードもこの空間に積まれる。
//! **伸びる場所を1ファイルに隔離する**のが、この分割の最大の狙い。

use super::operand::{fetch8, modrm, read_op16, Operand};
use super::*;
use crate::Machine;

/// `0F` を読んだ後の分岐。`start_ip` は命令の先頭 (フォールトの配送に使う)
pub(crate) fn step_0f(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let op2 = fetch8(m);
    match op2 {
        // LLDT/LTR系。ModRMのreg欄が「何をするか」を選ぶ
        0x00 => {
            let (reg, rm) = modrm(m, d);
            match reg {
                // LTR: TSSの場所をTRへ。記述子はGDTから読む
                3 => {
                    let sel = read_op16(m, &rm);
                    let off = (sel & !0x7) as u32;
                    let a = m.cpu.gdtr_base.wrapping_add(off);
                    let lo = m.read32(a);
                    let hi = m.read32(a.wrapping_add(4));
                    let ty = ((hi >> 8) & 0x1F) as u8;
                    if ty != 0x09 {
                        panic!("LTR: not an available 32bit TSS (type {ty:#04x})");
                    }
                    m.cpu.tr_sel = sel;
                    m.cpu.tr_base = (lo >> 16) | ((hi & 0xFF) << 16) | (hi & 0xFF00_0000);
                    m.cpu.tr_limit = (lo & 0xFFFF) | (hi & 0x000F_0000);
                }
                _ => panic!(
                    "unimplemented 0f 00 /{reg} at {:04x}:{:04x}",
                    m.cpu.sregs[CS], start_ip
                ),
            }
        }
        // システム表の操作
        0x01 => {
            let (reg, rm) = modrm(m, d);
            match (reg, &rm) {
                // LGDT m16&32: limit(2バイト) + base(4バイト)。
                // 16bitオペランドのときbaseは24bitしか読まれない —
                // 286互換の名残がここにも居る
                (2, Operand::Mem { addr, .. }) => {
                    m.cpu.gdtr_limit = m.read16(*addr);
                    let base = m.read32(addr.wrapping_add(2));
                    m.cpu.gdtr_base = if d.opsize32 { base } else { base & 0x00FF_FFFF };
                }
                // LIDT: 形はLGDTと同じ
                (3, Operand::Mem { addr, .. }) => {
                    m.cpu.idtr_limit = m.read16(*addr);
                    let base = m.read32(addr.wrapping_add(2));
                    m.cpu.idtr_base = if d.opsize32 { base } else { base & 0x00FF_FFFF };
                }
                _ => panic!(
                    "unimplemented 0f 01 /{reg} at {:04x}:{:04x}",
                    m.cpu.sregs[CS], start_ip
                ),
            }
        }
        // MOV r32, CRn / MOV CRn, r32。ModRMだがmodは無視して常にレジスタ形式
        0x20 | 0x22 => {
            let mrm = fetch8(m);
            let cr = ((mrm >> 3) & 7) as usize;
            let r = (mrm & 7) as usize;
            // MOV r32,CRn (0x20) と MOV CRn,r32 (0x22)。CR0/CR2/CR3を扱う
            if op2 == 0x20 {
                m.cpu.regs[r] = match cr {
                    0 => m.cpu.cr0,
                    2 => m.cpu.cr2,
                    3 => m.cpu.cr3,
                    _ => panic!("unimplemented read of CR{cr}"),
                };
            } else {
                let v = m.cpu.regs[r];
                match cr {
                    0 => {
                        let was_pe = m.cpu.pe();
                        m.cpu.cr0 = v;
                        // PEが立った瞬間、隠しレジスタを今のリアルモードの姿で
                        // 初期化する (リアルモードは写しを遅延評価しているため)
                        if !was_pe && m.cpu.pe() {
                            for i in 0..6 {
                                m.cpu.hidden[i] = SegHidden::real(m.cpu.sregs[i]);
                            }
                        }
                    }
                    2 => m.cpu.cr2 = v,
                    3 => m.cpu.cr3 = v, // ページテーブルが替わる。TLBは持たないので何もしない
                    _ => panic!("unimplemented write of CR{cr}"),
                }
            }
        }
        // ud2: 「わざと#UDを起こす」ための公式の命令。
        // 未実装オペコードの即panicとは別物 — こちらは**仕様どおりの例外**
        0x0B => {
            // フォールトは**その命令自身を指すIP**で配送する (再実行できる形)
            m.cpu.ip = start_ip;
            interrupt(m, 6);
        }
        _ => {
            let _ = start_ip;
            m.trap(format!("unimplemented opcode 0f {op2:#04x}"));
        }
    }
}
