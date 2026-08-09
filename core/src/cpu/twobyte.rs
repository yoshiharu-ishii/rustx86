//! 0F で始まる二バイト命令 (386〜)。
//!
//! 1バイトの256席が埋まったので、`0F` をエスケープにしてもう1バイト読む。
//! ここが将来いちばん伸びる区画である — システム命令 (LGDT/LIDT/LTR/MOV CR)
//! がまず入り、フォーク先ではSSE/AVXやロングモードもこの空間に積まれる。
//! **伸びる場所を1ファイルに隔離する**のが、この分割の最大の狙い。

use super::operand::{fetch8, modrm, read_op16, Operand};
use super::*;
use crate::Machine;

/// BTファミリの本体。kind: 0=BT 1=BTS 2=BTR 3=BTC。
/// テストしたビットを CF に入れ、BT以外は書き換える
fn bit_op(m: &mut Machine, d: &Decoder, rm: &Operand, off: i32, kind: u8) {
    match rm {
        Operand::Reg(r) => {
            let bits = if d.opsize32 { 32 } else { 16 };
            let bit = (off as u32) % bits; // レジスタ相手は幅で折り返す
            let v = m.cpu.reg_w(*r, d.opsize32);
            m.cpu.set_flag(super::CF, (v >> bit) & 1 != 0);
            let new = match kind {
                1 => v | (1 << bit),
                2 => v & !(1 << bit),
                3 => v ^ (1 << bit),
                _ => return,
            };
            m.cpu.set_reg_w(*r, new, d.opsize32);
        }
        Operand::Mem { addr, .. } => {
            // メモリ相手は「ビット列」扱い: バイト単位で先へ (負なら手前へ) 進む
            let byte = addr.wrapping_add((off >> 3) as u32);
            let bit = (off & 7) as u32;
            let v = m.read8(byte);
            m.cpu.set_flag(super::CF, (v >> bit) & 1 != 0);
            let new = match kind {
                1 => v | (1 << bit),
                2 => v & !(1 << bit),
                3 => v ^ (1 << bit),
                _ => return,
            };
            m.write8(byte, new);
        }
    }
}

/// `0F` を読んだ後の分岐。`start_ip` は命令の先頭 (フォールトの配送に使う)
pub(crate) fn step_0f(m: &mut Machine, d: &Decoder, start_ip: u32) {
    let op2 = fetch8(m);
    match op2 {
        // LLDT/LTR系。ModRMのreg欄が「何をするか」を選ぶ
        0x00 => {
            let (reg, rm) = modrm(m, d);
            match reg {
                // SLDT / LLDT: LDTセレクタの読み書き。表は引かない (保持のみ)
                0 => {
                    let v = m.cpu.ldtr_sel;
                    super::operand::write_op16(m, &rm, v);
                }
                2 => {
                    let sel = read_op16(m, &rm);
                    if sel & !0x7 != 0 {
                        // 実LDTを積もうとした — 保持だけでは嘘になるので止める
                        m.trap(format!("LLDT with non-null selector {sel:#06x}"));
                        return;
                    }
                    m.cpu.ldtr_sel = sel;
                }
                // LTR: TSSの場所をTRへ。記述子はGDTから読む
                3 => {
                    let sel = read_op16(m, &rm);
                    let off = (sel & !0x7) as u32;
                    let a = m.cpu.gdtr_base.wrapping_add(off);
                    let prev_sys = m.sys_access.replace(true);
                    let lo = m.read32(a);
                    let hi = m.read32(a.wrapping_add(4));
                    m.sys_access.set(prev_sys);
                    let ty = ((hi >> 8) & 0x1F) as u8;
                    if ty != 0x09 {
                        panic!("LTR: not an available 32bit TSS (type {ty:#04x})");
                    }
                    m.cpu.tr_sel = sel;
                    m.cpu.tr_base = (lo >> 16) | ((hi & 0xFF) << 16) | (hi & 0xFF00_0000);
                    m.cpu.tr_limit = (lo & 0xFFFF) | (hi & 0x000F_0000);
                }
                // VERR/VERW: そのセレクタを読める/書けるか (ZFで答える)。
                // 現代カーネルはMDS緩和の「CPUバッファ掃除」としてVERWを撃つ —
                // 副作用の方が本体になった珍しい命令
                4 | 5 => {
                    let sel = read_op16(m, &rm);
                    let ok = if sel & !0x7 == 0 {
                        false // ヌルセレクタは常に不成立
                    } else {
                        let off = (sel & !0x7) as u32;
                        let a = m.cpu.gdtr_base.wrapping_add(off);
                        let prev_sys = m.sys_access.replace(true);
                        let hi = m.read32(a.wrapping_add(4));
                        m.sys_access.set(prev_sys);
                        let access = ((hi >> 8) & 0xFF) as u8;
                        let present = access & 0x80 != 0;
                        let is_data = access & 0x18 == 0x10;
                        if reg == 5 {
                            // VERW: 書けるのは data かつ writable
                            present && is_data && access & 0x02 != 0
                        } else {
                            // VERR: data は常に読める、code は readable ビット
                            present
                                && (is_data || (access & 0x18 == 0x18 && access & 0x02 != 0))
                        }
                    };
                    m.cpu.set_flag(super::ZF, ok);
                }
                _ => {
                    m.cpu.ip = start_ip; // 巻き戻して現場を保存
                    m.trap(format!("unimplemented 0f 00 /{reg}"));
                }
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
                // INVLPG: TLBの1エントリを無効化。TLBを持たないので何もしない
                (7, Operand::Mem { .. }) => {}
                _ => panic!(
                    "unimplemented 0f 01 /{reg} at {:04x}:{:04x}",
                    m.cpu.sregs[CS], start_ip
                ),
            }
        }
        // MOV r32, DRn / MOV DRn, r32。CR系と同じく常にレジスタ形式。
        // ハードウェアブレークは持たないので、器として保持するだけ
        0x21 | 0x23 => {
            let mrm = fetch8(m);
            let dr = ((mrm >> 3) & 7) as usize;
            let r = (mrm & 7) as usize;
            if op2 == 0x21 {
                m.cpu.regs[r] = m.cpu.dr[dr];
            } else {
                m.cpu.dr[dr] = m.cpu.regs[r];
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
                    4 => m.cpu.cr4,
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
                    4 => m.cpu.cr4 = v,
                    _ => panic!("unimplemented write of CR{cr}"),
                }
            }
        }
        // CMOVcc (P6/i686〜): 条件が真ならmov。**読みは条件に関わらず行う**
        // (偽でもメモリオペランドのフォールトは起きる、が実機の仕様)。
        // Alpineのユーザーランドはi686ビルドで、CPUIDを見ずにこれを使う
        0x40..=0x4F => {
            let w = d.opsize32;
            let (reg, rm) = modrm(m, d);
            let v = read_op_w(m, &rm, w);
            if super::alu::condition(&m.cpu, op2 & 0xF) {
                m.cpu.set_reg_w(reg, v, w);
            }
        }
        // Jcc near (386〜): 0F 80..0F 8F。短いJcc (0x70..) の32bit変位版。
        // 変位はオペランドサイズで rel16/rel32。条件は下位4bitで同じ表を引く
        0x80..=0x8F => {
            let rel = super::fetch_rel_w(m, d.opsize32);
            if super::alu::condition(&m.cpu, op2 & 0xF) {
                let ip = m.cpu.ip.wrapping_add(rel);
                m.cpu.set_ip(ip);
            }
        }
        // SETcc: 条件が真なら r/m8 に 1、偽なら 0
        0x90..=0x9F => {
            let (_, rm) = modrm(m, d);
            let v = u8::from(super::alu::condition(&m.cpu, op2 & 0xF));
            super::operand::write_op8(m, &rm, v);
        }
        // CLTS: CR0.TS (task switched) を下ろす。FPUの遅延切替に使う
        0x06 => m.cpu.cr0 &= !0x8,
        // WBINVD: キャッシュ書き戻し+無効化。キャッシュを持たないので何もしない
        0x09 => {}
        // RDMSR/WRMSR: MSRは1本も名乗っていない (CPUID.EDXのMSRビット=0)。
        // 実機のMSR無しCPUと同じく **#GP** を返す — カーネルの rdmsr_safe は
        // #GPを例外表 (fixup) で受けて「無い」と理解する設計になっている
        0x30 | 0x32 => {
            m.cpu.ip = start_ip; // フォールトは命令の先頭で配送
            super::interrupt::interrupt_protected_err(m, 13, Some(0));
        }
        // RDTSC: タイムスタンプカウンタを EDX:EAX へ
        0x31 => {
            let t = m.cpu.tsc;
            m.cpu.regs[AX] = t as u32;
            m.cpu.regs[DX] = (t >> 32) as u32;
        }
        // CPUID: 「この石は何者か」に答える (i586世代のみ)。
        // 名乗りはマシン構成 (MachineProfile) の管轄
        0xA2 => {
            if !m.profile.has_cpuid {
                m.trap("CPUID on a machine without it".into());
                return;
            }
            match m.cpu.regs[AX] {
                0 => {
                    // 最大リーフと、ベンダ文字列 "GenuineIntel" (EBX-EDX-ECX)
                    m.cpu.regs[AX] = 1;
                    m.cpu.regs[BX] = 0x756e_6547; // "Genu"
                    m.cpu.regs[DX] = 0x4965_6e69; // "ineI"
                    m.cpu.regs[CX] = 0x6c65_746e; // "ntel"
                }
                1 => {
                    // family 6 (i686/PentiumPro相当)。機能は FPU + TSC + CX8 + CMOV
                    // だけ名乗る — 名乗った分は全部実装が要る (MSR/APIC/PSEは名乗らない)。
                    // i686を名乗るのはユーザーランド (Alpine) がi686ビルドだから
                    m.cpu.regs[AX] = 0x0633;
                    m.cpu.regs[BX] = 0;
                    m.cpu.regs[CX] = 0;
                    // FPU|TSC|CX8|CMOV
                    m.cpu.regs[DX] = (1 << 0) | (1 << 4) | (1 << 8) | (1 << 15);
                }
                _ => {
                    m.cpu.regs[AX] = 0;
                    m.cpu.regs[BX] = 0;
                    m.cpu.regs[CX] = 0;
                    m.cpu.regs[DX] = 0;
                }
            }
        }
        // CMPXCHG (486〜): アキュムレータと比較して、等しければ交換。
        // ロックフリーの心臓部。カーネルはi586ビルドなら無条件で使う
        0xB0 => {
            let (reg, rm) = modrm(m, d);
            let dst = super::operand::read_op8(m, &rm);
            let acc = m.cpu.reg8(0);
            super::alu::alu8(&mut m.cpu, 7, acc, dst); // CMPと同じフラグ
            if acc == dst {
                let v = m.cpu.reg8(reg);
                super::operand::write_op8(m, &rm, v);
            } else {
                m.cpu.set_reg8(0, dst);
            }
        }
        0xB1 => {
            let (reg, rm) = modrm(m, d);
            let w = d.opsize32;
            let dst = read_op_w(m, &rm, w);
            let acc = m.cpu.reg_w(AX, w);
            super::alu::alu_w(&mut m.cpu, 7, acc, dst, w);
            if acc == dst {
                let v = m.cpu.reg_w(reg, w);
                super::operand::write_op_w(m, &rm, v, w);
            } else {
                m.cpu.set_reg_w(AX, dst, w);
            }
        }
        // XADD (486〜): 交換してから加算。fetch_add の正体
        0xC0 => {
            let (reg, rm) = modrm(m, d);
            let dst = super::operand::read_op8(m, &rm);
            let src = m.cpu.reg8(reg);
            let sum = super::alu::alu8(&mut m.cpu, 0, dst, src);
            m.cpu.set_reg8(reg, dst);
            super::operand::write_op8(m, &rm, sum);
        }
        0xC1 => {
            let (reg, rm) = modrm(m, d);
            let w = d.opsize32;
            let dst = read_op_w(m, &rm, w);
            let src = m.cpu.reg_w(reg, w);
            let sum = super::alu::alu_w(&mut m.cpu, 0, dst, src, w);
            m.cpu.set_reg_w(reg, dst, w);
            super::operand::write_op_w(m, &rm, sum, w);
        }
        // CMPXCHG8B (Pentium〜): 64bit版。EDX:EAX と比較、一致なら ECX:EBX を書く
        0xC7 => {
            let (reg, rm) = modrm(m, d);
            let addr = match (reg, &rm) {
                (1, Operand::Mem { addr, .. }) => *addr,
                _ => {
                    m.trap(format!("0f c7 /{reg} (not cmpxchg8b m64)"));
                    return;
                }
            };
            let mem = (m.read32(addr) as u64) | ((m.read32(addr.wrapping_add(4)) as u64) << 32);
            let acc = (m.cpu.regs[AX] as u64) | ((m.cpu.regs[DX] as u64) << 32);
            if mem == acc {
                m.write32(addr, m.cpu.regs[BX]);
                m.write32(addr.wrapping_add(4), m.cpu.regs[CX]);
                m.cpu.set_flag(super::ZF, true);
            } else {
                m.cpu.regs[AX] = mem as u32;
                m.cpu.regs[DX] = (mem >> 32) as u32;
                m.cpu.set_flag(super::ZF, false);
            }
        }
        // BSWAP (486〜): バイト順の反転。ネットワークバイト順との往復
        0xC8..=0xCF => {
            let r = (op2 & 7) as usize;
            m.cpu.regs[r] = m.cpu.regs[r].swap_bytes();
        }
        // BSF/BSR (386〜): 最下位/最上位の立っているビットの位置。
        // ソースが0ならZF=1で結果は未定義 (実機は保存が多いが、書かない)
        0xBC | 0xBD => {
            let w = d.opsize32;
            let (reg, rm) = modrm(m, d);
            let v = read_op_w(m, &rm, w);
            if v == 0 {
                m.cpu.set_flag(super::ZF, true);
            } else {
                m.cpu.set_flag(super::ZF, false);
                let pos = if op2 == 0xBC {
                    v.trailing_zeros()
                } else {
                    31 - v.leading_zeros()
                };
                m.cpu.set_reg_w(reg, pos, w);
            }
        }
        // IMUL r, r/m (2オペランド形、386〜)。フラグの意味は 0x69/0x6B と同じ
        0xAF => {
            let w = d.opsize32;
            let (reg, rm) = modrm(m, d);
            let (a, b) = if w {
                (
                    m.cpu.reg_w(reg, true) as i32 as i64,
                    read_op_w(m, &rm, true) as i32 as i64,
                )
            } else {
                (
                    m.cpu.reg16(reg) as i16 as i64,
                    read_op16(m, &rm) as i16 as i64,
                )
            };
            let r = a * b;
            let ext = if w {
                m.cpu.set_reg32(reg, r as u32);
                (r as i32 as i64) != r
            } else {
                m.cpu.set_reg16(reg, r as u16);
                (r as i16 as i64) != r
            };
            m.cpu.set_flag(super::CF, ext);
            m.cpu.set_flag(super::OF, ext);
        }
        // MOVZX/MOVSX (386〜): 小さい値をゼロ拡張/符号拡張して広いレジスタへ。
        // Cコンパイラが u8/i8/u16/i16 → int の変換で山ほど出す
        0xB6 | 0xB7 | 0xBE | 0xBF => {
            let (reg, rm) = modrm(m, d);
            let sign = op2 & 0x08 != 0; // BE/BF = MOVSX
            let from16 = op2 & 0x01 != 0; // B7/BF = 16bit元
            let v = if from16 {
                let x = read_op16(m, &rm);
                if sign {
                    x as i16 as i32 as u32
                } else {
                    x as u32
                }
            } else {
                let x = super::operand::read_op8(m, &rm);
                if sign {
                    x as i8 as i32 as u32
                } else {
                    x as u32
                }
            };
            m.cpu.set_reg_w(reg, v, d.opsize32);
        }
        // PUSH/POP FS・GS (386〜)。1バイト空間に席が無かったのでここに居る。
        // スタックの刻みは 0x06/0x0E 系と同じくオペランドサイズ
        0xA0 | 0xA8 => {
            let s = if op2 == 0xA0 { FS } else { GS };
            let v = m.cpu.sregs[s];
            super::operand::push_w(m, v as u32, d.opsize32);
        }
        0xA1 | 0xA9 => {
            let s = if op2 == 0xA1 { FS } else { GS };
            let v = super::operand::pop_w(m, d.opsize32) as u16;
            super::load_seg(m, s, v);
        }
        // SHLD/SHRD (386〜): 倍精度シフト。隣のレジスタから溢れたビットを
        // 継ぎ足しながらずらす — 64bit値を32bitレジスタ2本でずらすための命令
        0xA4 | 0xA5 | 0xAC | 0xAD => {
            let (reg, rm) = modrm(m, d);
            let count = if op2 & 1 == 0 {
                fetch8(m)
            } else {
                m.cpu.reg8(1) // CL
            } & 0x1F;
            if count == 0 {
                return;
            }
            let bits: u32 = if d.opsize32 { 32 } else { 16 };
            let dst = read_op_w(m, &rm, d.opsize32);
            let src = m.cpu.reg_w(reg, d.opsize32);
            let n = count as u32 % bits;
            let (r, cf) = if op2 & 0x08 == 0 {
                // SHLD: 左へ。srcの上位ビットが右から入る
                if n == 0 {
                    (dst, m.cpu.flag(super::CF))
                } else {
                    let r = (dst << n) | (src >> (bits - n));
                    (r, (dst >> (bits - n)) & 1 != 0)
                }
            } else {
                // SHRD: 右へ。srcの下位ビットが左から入る
                if n == 0 {
                    (dst, m.cpu.flag(super::CF))
                } else {
                    let r = (dst >> n) | (src << (bits - n));
                    (r, (dst >> (n - 1)) & 1 != 0)
                }
            };
            super::operand::write_op_w(m, &rm, r, d.opsize32);
            m.cpu.set_flag(super::CF, cf);
            super::alu::set_szp_w(&mut m.cpu, r, d.opsize32);
        }
        // BT/BTS/BTR/BTC (386〜): ビット単位の test/set/reset/complement。
        // カーネルのビットマップ (cpumask, ページフラグ) の主役。
        // **メモリ相手だとビットオフセットは番地を符号付きではみ出して進む**
        // (bit 100 なら +12バイト先، bit -1 なら手前のバイト) — レジスタ相手の
        // 「幅で折り返し」とは別物なので注意
        0xA3 | 0xAB | 0xB3 | 0xBB => {
            let (reg, rm) = modrm(m, d);
            let off = m.cpu.reg_w(reg, d.opsize32) as i32;
            let kind = (op2 >> 3) & 3; // A3=0(BT) AB=1(BTS) B3=2(BTR) BB=3(BTC)
            bit_op(m, d, &rm, off, kind);
        }
        // Group 8: BTx r/m, imm8
        0xBA => {
            let (reg, rm) = modrm(m, d);
            let imm = fetch8(m) as i32;
            match reg {
                4..=7 => bit_op(m, d, &rm, imm, (reg - 4) as u8),
                _ => m.trap(format!("0f ba /{reg} (undefined encoding)")),
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
