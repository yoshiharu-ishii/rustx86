//! 1バイト命令空間の振り分け表。
//!
//! `step()` (mod.rs) がプレフィクスを解決した後、オペコード1バイト目の
//! 行き先を決めるのがここ。**伸びる場所を1ファイルに隔離する**のは
//! [`twobyte`] (0F空間) と同じ判断で、mod.rs には機械の骨格
//! (Cpu・Decoder・step の流れ) だけを残す。
//!
//! 中身は意図的に「巨大なmatch」のままにしてある。表引きや関数ポインタに
//! 崩すのは見た目の整理にはなるが、実行の形が変わる — 速さの改造は
//! デコード済み命令キャッシュ ([`dcache`]) の仕事で、ここは**意味の原本**である。

use super::*;

/// プレフィクス解決済みの `op` を実行する。`start_ip` は命令の先頭
/// (未実装トラップとフォールト配送が「犯行現場」を指すために使う)
pub(crate) fn exec(m: &mut Machine, d: &Decoder, op: u8, start_ip: u32) {
    match op {
        // --- ALUグリッド: 0x00-0x3D (演算3bit x 形式3bit) ---
        0x00..=0x3F if op & 7 <= 5 && (op & 0x27) != 0x26 && (op & 0x27) != 0x27 => {
            let kind = (op >> 3) & 7;
            match op & 7 {
                0 => {
                    // r/m8, r8
                    let (reg, rm) = modrm(m, d);
                    let a = read_op8(m, &rm);
                    let b = m.cpu.reg8(reg);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        write_op8(m, &rm, r);
                    }
                }
                1 => {
                    // r/m16,r16 または r/m32,r32 (`0x66` が付いていれば後者)
                    let (reg, rm) = modrm(m, d);
                    let w = d.opsize32;
                    let a = read_op_w(m, &rm, w);
                    let b = m.cpu.reg_w(reg, w);
                    let r = alu_w(&mut m.cpu, kind, a, b, w);
                    if kind != 7 {
                        write_op_w(m, &rm, r, w);
                    }
                }
                2 => {
                    // r8, r/m8
                    let (reg, rm) = modrm(m, d);
                    let a = m.cpu.reg8(reg);
                    let b = read_op8(m, &rm);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        m.cpu.set_reg8(reg, r);
                    }
                }
                3 => {
                    let (reg, rm) = modrm(m, d);
                    let w = d.opsize32;
                    let a = m.cpu.reg_w(reg, w);
                    let b = read_op_w(m, &rm, w);
                    let r = alu_w(&mut m.cpu, kind, a, b, w);
                    if kind != 7 {
                        m.cpu.set_reg_w(reg, r, w);
                    }
                }
                4 => {
                    // AL, imm8
                    let b = fetch8(m);
                    let a = m.cpu.reg8(0);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        m.cpu.set_reg8(0, r);
                    }
                }
                _ => {
                    // AX, imm16 / EAX, imm32。**FreeDOSの386判定はここを通る**
                    // (`66 0D 00 00 04 00` = OR EAX, 0x00040000 で ACフラグを立てる)
                    let w = d.opsize32;
                    let b = fetch_w(m, w);
                    let a = m.cpu.reg_w(AX, w);
                    let r = alu_w(&mut m.cpu, kind, a, b, w);
                    if kind != 7 {
                        m.cpu.set_reg_w(AX, r, w);
                    }
                }
            }
        }

        // --- GRP1: ALU r/m, imm ---
        0x80 | 0x81 | 0x83 => {
            let (kind, rm) = modrm(m, d);
            let kind = kind as u8;
            if op == 0x80 {
                let a = read_op8(m, &rm);
                let b = fetch8(m);
                let r = alu8(&mut m.cpu, kind, a, b);
                if kind != 7 {
                    write_op8(m, &rm, r);
                }
            } else {
                let w = d.opsize32;
                let a = read_op_w(m, &rm, w);
                // 0x83 は**符号拡張された8bit即値**。32bitなら32bitまで伸びる
                let b = if op == 0x81 {
                    fetch_w(m, w)
                } else if w {
                    fetch8(m) as i8 as i32 as u32
                } else {
                    fetch8(m) as i8 as u16 as u32
                };
                let r = alu_w(&mut m.cpu, kind, a, b, w);
                if kind != 7 {
                    write_op_w(m, &rm, r, w);
                }
            }
        }

        // --- MOV ---
        0x88 => {
            let (reg, rm) = modrm(m, d);
            let v = m.cpu.reg8(reg);
            write_op8(m, &rm, v);
        }
        0x89 => {
            let (reg, rm) = modrm(m, d);
            let w = d.opsize32;
            let v = m.cpu.reg_w(reg, w);
            write_op_w(m, &rm, v, w);
        }
        0x8A => {
            let (reg, rm) = modrm(m, d);
            let v = read_op8(m, &rm);
            m.cpu.set_reg8(reg, v);
        }
        0x8B => {
            let (reg, rm) = modrm(m, d);
            let w = d.opsize32;
            let v = read_op_w(m, &rm, w);
            m.cpu.set_reg_w(reg, v, w);
        }
        0x8C => {
            let (reg, rm) = modrm(m, d);
            // ModRM.reg でセグメントを選ぶ: 0=ES 1=CS 2=SS 3=DS 4=FS 5=GS。
            // 昔は4本 (reg&3) で足りたが FS/GS を足したので **6本ぶん見る**。
            // ここを &3 のままにしていて mov gs が cs を書き、Linux で墜落した
            if reg > 5 {
                m.trap(format!("mov r/m16, sreg with reg={reg} (reserved)"));
                return;
            }
            let v = m.cpu.sregs[reg];
            write_op16(m, &rm, v);
        }
        0x8E => {
            let (reg, rm) = modrm(m, d);
            // mov cs, r/m は 386 では #UD。まだ何も書いていないので
            // IPを命令頭へ戻して INT 6 を配送する (test386 が実挙動を要求する)
            if reg == 1 {
                m.cpu.set_ip(start_ip);
                interrupt(m, 6);
                return;
            }
            if reg > 5 {
                m.trap(format!("mov sreg, r/m16 with reg={reg} (reserved)"));
                return;
            }
            let v = read_op16(m, &rm);
            // 保護モードではGDTから隠しレジスタへ写す。リアルモードなら従来どおり。
            // 失敗 (#GP/#NP/#SS) はload_seg内で配送済み — この命令はここで終わり
            let _ = load_seg(m, reg, v);
        }
        0xB0..=0xB7 => {
            let v = fetch8(m);
            m.cpu.set_reg8((op & 7) as usize, v);
        }
        0xB8..=0xBF => {
            let w = d.opsize32;
            let v = fetch_w(m, w);
            m.cpu.set_reg_w((op & 7) as usize, v, w);
        }
        0xC6 => {
            let (_, rm) = modrm(m, d);
            let v = fetch8(m);
            write_op8(m, &rm, v);
        }
        0xC7 => {
            let (_, rm) = modrm(m, d);
            let w = d.opsize32;
            let v = fetch_w(m, w);
            write_op_w(m, &rm, v, w);
        }
        // MOV AL/AX/EAX <-> moffs。**オフセットの幅はアドレスサイズが決める**。
        // 16bit固定にしていたため、32bitコードで moffs32 を2バイトしか読まず、
        // 以降がずれた (ページングのテストで露見。pm_hello では直後がHLTで
        // 結果が偶然合い、見逃していた)
        0xA0 => {
            let off = fetch_addr(m, d.addrsize32);
            let seg = d.seg_override.unwrap_or(DS);
            let v = m.read8(m.data_addr(seg, off, 1, false));
            m.cpu.set_reg8(0, v);
        }
        0xA1 => {
            let off = fetch_addr(m, d.addrsize32);
            let seg = d.seg_override.unwrap_or(DS);
            let w = d.opsize32;
            let a = m.data_addr(seg, off, if w { 4 } else { 2 }, false);
            let v = if w { m.read32(a) } else { m.read16(a) as u32 };
            m.cpu.set_reg_w(AX, v, w);
        }
        0xA2 => {
            let off = fetch_addr(m, d.addrsize32);
            let seg = d.seg_override.unwrap_or(DS);
            let v = m.cpu.reg8(0);
            let a = m.data_addr(seg, off, 1, true);
            m.write8(a, v);
        }
        0xA3 => {
            let off = fetch_addr(m, d.addrsize32);
            let seg = d.seg_override.unwrap_or(DS);
            let w = d.opsize32;
            let a = m.data_addr(seg, off, if w { 4 } else { 2 }, true);
            let v = m.cpu.reg_w(AX, w);
            if w {
                m.write32(a, v)
            } else {
                m.write16(a, v as u16)
            }
        }

        // --- INC/DEC r16 ---
        // **CFは触らない** — INC/DECがADD/SUBと違う唯一の点で、
        // 多倍長の加算ループでキャリーを壊さないための配慮である
        0x40..=0x4F => {
            let (r, w) = ((op & 7) as usize, d.opsize32);
            let a = m.cpu.reg_w(r, w);
            let v = alu::inc_dec_w(&mut m.cpu, a, op >= 0x48, w);
            m.cpu.set_reg_w(r, v, w);
        }

        // --- PUSH/POP ---
        // **FreeDOSの386判定はここも通る** (`66 50` = PUSH EAX / `66 58` = POP EAX)
        0x50..=0x57 => {
            let w = d.opsize32;
            let v = m.cpu.reg_w((op & 7) as usize, w);
            push_w(m, v, w);
        }
        0x58..=0x5F => {
            let w = d.opsize32;
            let v = pop_w(m, w);
            m.cpu.set_reg_w((op & 7) as usize, v, w);
        }

        // セグメントレジスタのPUSH/POP。オペコードのbit3-4がそのまま
        // ES/CS/SS/DS の番号になっている (0x06,0x0E,0x16,0x1E)。
        // **32bitコードではdword1個ぶんの席を使う** — 値は16bitでも、
        // スタックの刻みは4バイト。ここを2バイトで積むとESPがずれ、
        // カーネルの sync_core (pushf; push %cs; push $1f; iret) が腐った
        0x06 | 0x0E | 0x16 | 0x1E => {
            let v = m.cpu.sregs[(op >> 3) as usize & 3];
            push_w(m, v as u32, d.opsize32);
        }
        // POP CS (0x0F) は8086にしか無く、186以降は2バイト命令の導入符になった。
        // ここでは実装しない (Tier 3 で 0x0F を二バイト空間として使う)
        0x07 | 0x17 | 0x1F => {
            let v = pop_w(m, d.opsize32) as u16;
            // 保護モードでは記述子を読み直して隠しレジスタも更新する。
            // POPはもう起きた (SPは進んだ) 上でのセグメント検査 — 実CPUも同順
            let _ = load_seg(m, (op >> 3) as usize & 3, v);
        }

        // --- PUSHA/POPA (186)。オペランドサイズ32ならPUSHAD/POPAD ---
        0x60 => {
            let w = d.opsize32;
            let sp = m.cpu.reg_w(SP, w); // 退避するのは「PUSHA開始時点の」SP
            for r in [AX, CX, DX, BX] {
                let v = m.cpu.reg_w(r, w);
                push_w(m, v, w);
            }
            push_w(m, sp, w);
            for r in [BP, SI, DI] {
                let v = m.cpu.reg_w(r, w);
                push_w(m, v, w);
            }
        }
        0x61 => {
            let w = d.opsize32;
            for r in [DI, SI, BP] {
                let v = pop_w(m, w);
                m.cpu.set_reg_w(r, v, w);
            }
            pop_w(m, w); // 積んだSPは捨てる (POPAの結果SPは自然に元へ戻る)
            for r in [BX, DX, CX, AX] {
                let v = pop_w(m, w);
                m.cpu.set_reg_w(r, v, w);
            }
        }

        // BOUND r, m (186): 添字が [下限, 上限] (メモリ上の対) の外なら
        // #BR (INT 5)。境界はフォールト扱い — 積むIPは命令の頭
        0x62 => {
            let (reg, rm) = modrm(m, d);
            let Operand::Mem { off, seg, .. } = rm else {
                // レジスタ形は未定義 — #UD
                m.cpu.set_ip(start_ip);
                interrupt(m, 6);
                return;
            };
            let w = d.opsize32;
            let (idx, lo, hi) = if w {
                let a = m.data_addr(seg, off, 8, false);
                (
                    m.cpu.reg32(reg) as i32 as i64,
                    m.read32(a) as i32 as i64,
                    m.read32(a.wrapping_add(4)) as i32 as i64,
                )
            } else {
                let a = m.data_addr(seg, off, 4, false);
                (
                    m.cpu.reg16(reg) as i16 as i64,
                    m.read16(a) as i16 as i64,
                    m.read16(a.wrapping_add(2)) as i16 as i64,
                )
            };
            if idx < lo || idx > hi {
                m.cpu.set_ip(start_ip);
                interrupt(m, 5);
            }
        }

        // ARPL r/m16, r16 (286〜、保護モード専用 — リアル/V86では#UD)。
        // 「ユーザーから預かったセレクタのRPLを、呼び出し元の権限まで弱める」
        // ためのOS向け命令: dstのRPLがsrcより強ければ弱めてZF=1
        0x63 => {
            if !m.cpu.pe() || m.cpu.vm86() {
                m.cpu.set_ip(start_ip);
                interrupt(m, 6);
                return;
            }
            let (reg, rm) = modrm(m, d);
            let dst = read_op16(m, &rm);
            let src = m.cpu.reg16(reg);
            if (dst & 3) < (src & 3) {
                m.cpu.set_flag(ZF, true);
                write_op16(m, &rm, (dst & !3) | (src & 3));
            } else {
                m.cpu.set_flag(ZF, false);
            }
        }

        // --- PUSH imm (186) ---
        // PUSH imm — 積む幅はオペランドサイズに従う。
        // 16bit固定にしていたため、32bitコードで `push dword` が2バイトしか
        // 積まず、スタックが1語ずつずれて iretd が化けた (特権リングのテストで
        // 発覚。**倒れた場所は犯行現場ではない**を、間接分岐の谷で再演した)
        0x68 => {
            let v = fetch_w(m, d.opsize32);
            push_w(m, v, d.opsize32);
        }
        0x6A => {
            let v = fetch8(m) as i8 as i32 as u32;
            push_w(m, v, d.opsize32);
        }

        // --- IMUL r16, r/m16, imm (186) ---
        // 3オペランドの乗算。CF/OFは「結果が幅に収まらなかったか」だけを表し、
        // SF/ZF/AF/PF は未定義。**即値の幅もオペランドサイズ** —
        // 0x69 は32bitコードで6バイト命令になる (16bit読みしてEIPがずれ、
        // 即値の中を命令として食い始める事故を実際に踏んだ)
        0x69 | 0x6B => {
            let w = d.opsize32;
            let (reg, rm) = modrm(m, d);
            let a = if w {
                read_op_w(m, &rm, true) as i32 as i64
            } else {
                read_op16(m, &rm) as i16 as i64
            };
            let b = if op == 0x69 {
                if w {
                    fetch32(m) as i32 as i64
                } else {
                    fetch16(m) as i16 as i64
                }
            } else {
                fetch8(m) as i8 as i64
            };
            let r = a * b;
            if w {
                m.cpu.set_reg32(reg, r as u32);
                let ext = (r as i32 as i64) != r;
                m.cpu.set_flag(CF, ext);
                m.cpu.set_flag(OF, ext);
            } else {
                m.cpu.set_reg16(reg, r as u16);
                let ext = (r as i16 as i64) != r;
                m.cpu.set_flag(CF, ext);
                m.cpu.set_flag(OF, ext);
            }
        }

        // --- ENTER/LEAVE (186): スタックフレームの作成と破棄 ---
        0xC8 => {
            // 幅の軸が2本ある: **積む値の幅はオペランドサイズ、BP/SPを動かす幅は
            // SSのBフラグ**。o32 enter を16bitスタックで打つと「4バイトずつ積むが
            // ポインタは16bitで回り、EBPの上位16bitは温存」になる (test386 POST 1A)
            let w = d.opsize32;
            let sw = m.cpu.seg_is32(SS);
            let step = if w { 4u32 } else { 2 };
            let size = fetch16(m) as u32;
            let level = fetch8(m) & 0x1F;
            let bp = m.cpu.reg_w(BP, w);
            push_w(m, bp, w);
            // frame tempは**ESPレジスタの生の値**。16bitスタックではpushが
            // SPしか動かさないので上位16bitが残り、o32で積むとその姿
            // (例 0x0001FFFC) がそのまま見える (test386 POST 1Aが検査する)
            let frame = m.cpu.regs[SP];
            if level > 0 {
                // ネストした手続きの表示 (display) を積む。Pascal系言語のための機構で、
                // Cしか使わない現代では level=0 しか出てこない
                for _ in 1..level {
                    let b = m.cpu.reg_w(BP, sw).wrapping_sub(step);
                    m.cpu.set_reg_w(BP, b, sw);
                    let v = if w {
                        m.read32(m.cpu.lin(SS, b))
                    } else {
                        m.read16(m.cpu.lin(SS, b)) as u32
                    };
                    push_w(m, v, w);
                }
                push_w(m, frame, w);
            }
            m.cpu.set_reg_w(BP, frame, sw);
            // 最後のSP調整は「今のSP」から引く。Intel SDMの疑似コードは
            // `SP <- BP - Size` と書いているが、これが正しいのは level=0 のときだけで、
            // level>0 では display を積んだ分が抜け落ちる。
            // AMDのマニュアルとQEMUの実装は現在のSPから引いており、そちらが実挙動。
            // co-simがこの差を捕まえた
            let sp = sp_read(m).wrapping_sub(size);
            // ENTERは**最終SPでの書き込み可否を検分**する — 実際には書かないのに、
            // 書いたら起きるはずの #PF を起こす (実CPUの仕様。test386 POST 1Aは
            // pushの当たらないページを監督者専用にしてこれだけを狙い撃つ)
            let wrapped = if m.cpu.seg_is32(SS) { sp } else { sp & 0xFFFF };
            if let Err(f) = m.translate_for(m.cpu.lin(SS, wrapped), true) {
                m.note_fault(f);
            }
            sp_write(m, sp);
        }
        0xC9 => {
            // LEAVE: SP←BP、そしてBPをpop。SP←BPの幅はSSのBフラグ側、
            // popの幅はオペランドサイズ (ENTERと同じ2軸)
            let sw = m.cpu.seg_is32(SS);
            let bp = m.cpu.reg_w(BP, sw);
            sp_write(m, bp);
            let v = pop_w(m, d.opsize32);
            m.cpu.set_reg_w(BP, v, d.opsize32);
        }

        // --- LES/LDS: メモリから「オフセットとセグメント」を一度に取る ---
        // far ポインタ (4バイト) を読み、下位2バイトを汎用レジスタへ、
        // 上位2バイトをセグメントレジスタへ入れる
        0xC4 | 0xC5 => {
            // オフセットの幅はオペランドサイズ (o32ならoff32+seg16の6バイト)
            let (reg, rm) = modrm(m, d);
            let addr = match rm {
                Operand::Mem { addr, .. } => addr,
                Operand::Reg(_) => panic!("LES/LDS with register operand"),
            };
            let off = if d.opsize32 {
                m.read32(addr)
            } else {
                m.read16(addr) as u32
            };
            let seg = m.read16(addr.wrapping_add(if d.opsize32 { 4 } else { 2 }));
            // セグメントを先に検査してからレジスタを書く (失敗なら何も変えない)
            if load_seg(m, if op == 0xC4 { ES } else { DS }, seg) {
                m.cpu.set_reg_w(reg, off, d.opsize32);
            }
        }

        // --- XLAT: AL = [BX + AL] ---
        // 256バイトの変換テーブルを1命令で引く。文字コード変換のための命令
        0xD7 => {
            let seg = d.seg_override.unwrap_or(DS);
            // 基底の幅はアドレスサイズ (32bitでは EBX+AL)
            let off = if d.addrsize32 {
                m.cpu.regs[BX].wrapping_add(m.cpu.reg8(0) as u32)
            } else {
                m.cpu.reg16(BX).wrapping_add(m.cpu.reg8(0) as u16) as u32
            };
            let v = m.read8(m.cpu.lin(seg, off));
            m.cpu.set_reg8(0, v);
        }

        // --- IN/OUT: I/Oポート空間へのアクセス ---
        // オペコードのビットがそのまま形式を表す:
        //   bit0 = 幅 (0:8bit 1:16bit)  bit1 = 向き (0:IN 1:OUT)  bit3 = ポート指定 (0:imm8 1:DX)
        0xE4..=0xE7 | 0xEC..=0xEF => {
            let port = if op & 8 != 0 {
                m.cpu.reg16(DX)
            } else {
                fetch8(m) as u16
            };
            let wide = op & 1 != 0;
            let out = op & 2 != 0;
            // I/O特権: CPL > IOPL (V86では常に) はTSSのI/O許可ビットマップが
            // 最後の砦。不許可は #GP(0) — gp_faultが命令の頭へ巻き戻すので、
            // 先にfetchしたimm8ポートも無かったことになる
            let bytes = if !wide {
                1
            } else if d.opsize32 {
                4
            } else {
                2
            };
            if !super::interrupt::io_permitted(m, port, bytes) {
                gp_fault(m, start_ip, 0);
                return;
            }
            // 幅つき (bit0=1) はオペランドサイズで 16/32 が割れる。
            // inl/outl はPCIコンフィグ (0xCF8/0xCFC) が32bitで叩いてくる
            match (out, wide) {
                (false, false) => {
                    let v = m.io_read8(port);
                    m.cpu.set_reg8(0, v);
                }
                (false, true) => {
                    if d.opsize32 {
                        let v = m.io_read32(port);
                        m.cpu.set_reg32(AX, v);
                    } else {
                        let v = m.io_read16(port);
                        m.cpu.set_reg16(AX, v);
                    }
                }
                (true, false) => {
                    let v = m.cpu.reg8(0);
                    m.io_write8(port, v);
                }
                (true, true) => {
                    if d.opsize32 {
                        let v = m.cpu.reg32(AX);
                        m.io_write32(port, v);
                    } else {
                        let v = m.cpu.reg16(AX);
                        m.io_write16(port, v);
                    }
                }
            }
        }

        // WAIT: コプロセッサ待ち。FPUが無いので何もしない
        0x9B => {}

        // --- ジャンプ/コール ---
        0x70..=0x7F => {
            let rel = fetch8(m) as i8;
            if condition(&m.cpu, op & 0xF) {
                // rel8はIPの幅へ符号拡張。折り返しは set_ip が知っている
                let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
                m.cpu.set_ip(ip);
            }
        }
        0xE8 => {
            // 相対値の幅はオペランドサイズ。16bitのrelは符号拡張して足す
            let rel = fetch_rel_w(m, d.opsize32);
            let ret = m.cpu.ip;
            push_w(m, ret, d.opsize32);
            m.cpu.set_ip(ret.wrapping_add(rel));
        }
        0xE9 => {
            let rel = fetch_rel_w(m, d.opsize32);
            let ip = m.cpu.ip.wrapping_add(rel);
            m.cpu.set_ip(ip);
        }
        0xEB => {
            let rel = fetch8(m) as i8;
            let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
            m.cpu.set_ip(ip);
        }
        0xC3 => {
            let ip = pop_w(m, d.opsize32);
            m.cpu.set_ip(ip);
        }

        // --- far転送: CSごと移る ---
        // リアルモードでは「CSに値を代入する」だけだが、プロテクトモードでは
        // 同じ命令がディスクリプタ引きと特権チェックに化ける (Tier 3)。
        0xEA => {
            // オフセットの幅はオペランドサイズに従う (16bitコードなら off16)
            let off = fetch_w(m, d.opsize32);
            let seg = fetch16(m);
            // 保護モードではこれが**遷移を完成させる**一撃になる。
            // PE=1にしただけではまだ16bitのまま走っていて、CSに記述子が
            // 積まれて初めて32bitコードが始まる (CS検査に落ちたらIPは据えない)
            if load_seg(m, CS, seg) {
                m.cpu.set_ip(off);
            }
        }
        // far call / far ret も**オフセットとpush/popの幅はオペランドサイズ**。
        // 16bit固定にしていると `o32 call dword seg:off32` が off32 の上位16bitを
        // セグメントとして食い、CS=0000 の空RAMへ飛ぶ (test386 POST 05 が暴いた)
        0x9A => {
            let off = fetch_w(m, d.opsize32);
            let seg = fetch16(m);
            // 保護モードではコールゲート経由のリング遷移になり得る — 共通経路へ
            segment::far_call(m, seg, off, d.opsize32);
        }
        0xCB => segment::far_ret(m, d.opsize32, 0),
        0xCA => {
            let n = fetch16(m) as u32;
            segment::far_ret(m, d.opsize32, n);
        }
        // IRET: 割り込みハンドラからの復帰。CALLと違いFLAGSも戻す。
        // 割り込み中に変わったIF/DFを呼び出し前の値へ戻すのが要点。
        0xCF => iret(m, d.opsize32),
        0xE2 => {
            // LOOP。カウンタの幅は**アドレスサイズ** (32bitではECX)
            let rel = fetch8(m) as i8;
            let a32 = d.addrsize32;
            let cx = m.cpu.reg_w(CX, a32).wrapping_sub(1);
            m.cpu.set_reg_w(CX, cx, a32);
            let cx = if a32 { cx } else { cx & 0xFFFF };
            if cx != 0 {
                let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
                m.cpu.set_ip(ip);
            }
        }

        // --- 割り込み (BIOS HLE) ---
        // Tier 1d でここを実IVTディスパッチに置き換える。
        // OSは起動時にIVTを自分のハンドラで書き換えるので、
        // ホスト関数へ横流しする今の方式ではOSが動かない。
        0xCD => {
            let n = fetch8(m);
            // V86のINT nはIOPL=3が入場券 (#GP(0))。INT3/INTOは対象外 —
            // IOPL検査があるのは即値形だけ (実CPUの仕様)
            if m.cpu.vm86() && m.cpu.iopl() < 3 {
                gp_fault(m, start_ip, 0);
                return;
            }
            software_int(m, n, start_ip);
        }
        0xCC => software_int(m, 3, start_ip), // INT3 (デバッガのブレークポイント)
        0xCE => {
            // INTO: OFが立っているときだけ割り込み4。立っていなければ何もしない
            if m.cpu.flag(OF) {
                interrupt(m, 4);
            }
        }

        // --- フラグ/制御 ---
        // HLT — 特権命令 (CPL0限定、IOPLは関係ない)。ring3のhltは #GP(0)
        0xF4 => {
            if m.cpu.pe() && m.cpu.cpl() != 0 {
                gp_fault(m, start_ip, 0);
                return;
            }
            m.halted = true;
        }
        0xF5 => {
            let c = m.cpu.flag(CF);
            m.cpu.set_flag(CF, !c);
        } // CMC
        0xF8 => m.cpu.set_flag(CF, false), // CLC
        0xF9 => m.cpu.set_flag(CF, true),  // STC
        // CLI/STI — IOPL特権命令。CPL > IOPL のリングが割り込みを握るのは
        // #GP(0) (リング3が cli でカーネルを止められたら保護にならない)
        0xFA | 0xFB => {
            if m.cpu.pe() && m.cpu.cpl() > m.cpu.iopl() {
                gp_fault(m, start_ip, 0);
                return;
            }
            m.cpu.set_flag(IF, op == 0xFB);
        }
        0xFC => m.cpu.set_flag(DF, false), // CLD
        0xFD => m.cpu.set_flag(DF, true),  // STD

        // --- TEST / XCHG / LEA ---
        0x84 => {
            let (reg, rm) = modrm(m, d);
            let a = read_op8(m, &rm);
            let b = m.cpu.reg8(reg);
            alu8(&mut m.cpu, 4, a, b);
        }
        0x85 => {
            let (reg, rm) = modrm(m, d);
            let w = d.opsize32;
            let a = read_op_w(m, &rm, w);
            let b = m.cpu.reg_w(reg, w);
            alu_w(&mut m.cpu, 4, a, b, w);
        }
        0x86 => {
            let (reg, rm) = modrm(m, d);
            let a = read_op8(m, &rm);
            let b = m.cpu.reg8(reg);
            write_op8(m, &rm, b);
            m.cpu.set_reg8(reg, a);
        }
        0x87 => {
            // XCHG r/m, r。カーネルのアトミック交換 (xchgl) の正体なので
            // 幅を間違えると同期構造が静かに腐る
            let w = d.opsize32;
            let (reg, rm) = modrm(m, d);
            let a = read_op_w(m, &rm, w);
            let b = m.cpu.reg_w(reg, w);
            write_op_w(m, &rm, b, w);
            m.cpu.set_reg_w(reg, a, w);
        }
        0x8D => {
            // LEA: セグメントを適用しない実効オフセットを取る
            let (reg, rm) = modrm(m, d);
            match rm {
                // オフセットの幅はオペランドサイズに従う (32bitなら EAX 等へ全桁)
                Operand::Mem { off, .. } => m.cpu.set_reg_w(reg, off, d.opsize32),
                Operand::Reg(_) => panic!("LEA with register operand"),
            }
        }
        0x8F => {
            // POP r/m。幅はオペランドサイズ (16bit固定にしていて、
            // Linuxの popl がESPを+2しかせず整列が壊れた)
            let v = pop_w(m, d.opsize32);
            let (_, rm) = modrm(m, d);
            write_op_w(m, &rm, v, d.opsize32);
        }
        // XCHG (E)AX, reg。0x90 は xchg (e)ax,(e)ax = NOP
        0x90..=0x97 => {
            let r = (op & 7) as usize;
            let w = d.opsize32;
            let a = m.cpu.reg_w(AX, w);
            let b = m.cpu.reg_w(r, w);
            m.cpu.set_reg_w(AX, b, w);
            m.cpu.set_reg_w(r, a, w);
        }
        // CBW/CWDE (0x98)・CWD/CDQ (0x99)。**幅で別の命令になる**:
        // 16bitでは AL→AX / AX→DX:AX、32bitでは AX→EAX / EAX→EDX:EAX。
        // ここが16bit固定だと cdq がEDX上位を残し、直後の idiv が
        // ゴミ被除数で #DE を起こす (Linuxの register_refined_jiffies で実際に鳴った)
        0x98 => {
            if d.opsize32 {
                let v = m.cpu.reg16(AX) as i16 as i32 as u32;
                m.cpu.set_reg32(AX, v);
            } else {
                let v = m.cpu.reg8(0) as i8 as i16 as u16;
                m.cpu.set_reg16(AX, v);
            }
        }
        0x99 => {
            if d.opsize32 {
                let v = if m.cpu.reg32(AX) & 0x8000_0000 != 0 {
                    0xFFFF_FFFF
                } else {
                    0
                };
                m.cpu.set_reg32(DX, v);
            } else {
                let v = if m.cpu.reg16(AX) & 0x8000 != 0 {
                    0xFFFF
                } else {
                    0
                };
                m.cpu.set_reg16(DX, v);
            }
        }
        // PUSHF / PUSHFD。**FreeDOSの386判定はここから始まる** (`66 9C`)。
        //
        // 386判定の常套手段は「EFLAGSのbit18 (AC) を立てて書き戻し、
        // 読み直して残っているか見る」というもので、そのためには
        // 32bit幅でフラグを出し入れできる必要がある
        0x9C => {
            // V86のPUSHFもIOPL=3が入場券 (#GP(0)) — フラグの出し入れは
            // 檻の監視者 (VMM) が割り込んで面倒を見る前提の設計
            if m.cpu.vm86() && m.cpu.iopl() < 3 {
                gp_fault(m, start_ip, 0);
                return;
            }
            let f = m.cpu.eflags() | 0x0002;
            if d.opsize32 {
                // 上位2bit (VM/RF) はPUSHFDでは常に0で出る
                push_w(m, f & 0x00FC_FFFF, true);
            } else if m.profile.has_fpu {
                // 386以降のPUSHF(16bit形): bit15は0、IOPL/NTは実値のまま。
                // (0xF000強制は8086だけ — test386のPOST 09が0xF603/0x0603の
                //  差として暴いた)
                push_w(m, (f as u16 & 0x7FFF | 0x0002) as u32, false);
            } else {
                // 8086: 上位4bit (15-12) は常に1で出る。ソフトはこれで世代を知る
                push_w(m, (f as u16 | 0xF002) as u32, false);
            }
        }
        0x9D => {
            // V86のPOPFもIOPL=3が入場券 (#GP(0)、PUSHFと対)
            if m.cpu.vm86() && m.cpu.iopl() < 3 {
                gp_fault(m, start_ip, 0);
                return;
            }
            let f = pop_w(m, d.opsize32);
            // 書き換えられるビットだけ受け取る。**どこまで受けるかは世代**:
            //
            // 16bit機 (8086) は AC (bit18) も ID (bit21) も受け付けない。
            // 「立てて書き戻し、残っていれば486以上/CPUID有り」という判定に
            // 使われるフラグで、一度ACを通してしまったところ、FreeDOSが486と
            // 判断して `CMOVcc` (Pentium Pro) を使い始めた。
            // 名乗るものを1ビット間違えるだけで、相手は別の道を歩き出す。
            //
            // 32bit機 (i586相当) は逆に**受けるのが正しい** — Linuxは
            // IDが書き換わることでCPUIDの存在を知り、先の初期化へ進む
            let mut mask: u32 = if m.profile.has_cpuid {
                if d.opsize32 {
                    0x0024_7FD5 // 標準 + IOPL/NT + AC + ID (VMは対象外 — iretだけ)
                } else {
                    0x7FD5
                }
            } else {
                0x0FD5
            };
            // 特権で書けるビットが変わる (実CPUのPOPFは**黙って**落とす、#GPしない):
            // IOPLを書けるのはリング0だけ。IFを書けるのは CPL <= IOPL のリングだけ
            // (V86のIOPL=3もこの規則に呑まれる — IOPL不可・IF可)
            if m.cpu.pe() {
                if m.cpu.cpl() > 0 {
                    mask &= !0x3000;
                }
                if m.cpu.cpl() > m.cpu.iopl() {
                    mask &= !IF;
                }
            }
            let cur = m.cpu.eflags();
            m.cpu.set_eflags((cur & !mask) | (f & mask) | 0x0002);
        }
        0x9E => {
            // SAHF: AHの下位バイトをフラグへ
            let ah = m.cpu.reg8(4) as u32;
            let cur = m.cpu.eflags();
            m.cpu.set_eflags((cur & !0xD5) | (ah & 0xD5) | 0x0002);
        }
        0x9F => {
            let f = (m.cpu.eflags() as u8 & 0xD5) | 0x02;
            m.cpu.set_reg8(4, f);
        }
        0xA8 => {
            let b = fetch8(m);
            let a = m.cpu.reg8(0);
            alu8(&mut m.cpu, 4, a, b);
        }
        // TEST AX,imm16 / EAX,imm32。
        // **即値の長さが幅で変わる**ので、ここを16bit固定にしていると
        // `0x66` が付いた瞬間にIPが2バイトずれ、以後はデータを命令として食う。
        // 実際それで遠くの番地まで暴走した (0x66 の記録がそれを教えてくれた)
        0xA9 => {
            let w = d.opsize32;
            let b = fetch_w(m, w);
            let a = m.cpu.reg_w(AX, w);
            alu_w(&mut m.cpu, 4, a, b, w);
        }

        // --- GRP2: シフト/回転 ---
        0xC0 | 0xC1 | 0xD0 | 0xD1 | 0xD2 | 0xD3 => group::grp2(m, d, op),

        // --- GRP3: TEST/NOT/NEG/MUL/IMUL/DIV/IDIV ---
        0xF6 => group::grp3_byte(m, d, start_ip),
        0xF7 => group::grp3_word(m, d, start_ip),

        // --- GRP4/GRP5 ---
        0xFE => group::grp4(m, d),
        0xFF => group::grp5(m, d, start_ip),

        // --- 十進補正 ---
        0x27 | 0x2F => decimal::daa_das(m, op),
        0x37 | 0x3F => decimal::aaa_aas(m, op),
        0xD4 => decimal::aam(m),
        0xD5 => decimal::aad(m),

        // --- ループ/条件ジャンプの残り ---
        // カウンタの幅は**アドレスサイズ**が選ぶ (0x67付きは16bitコードでもECX)。
        // E2 (LOOP) だけ直してこの2つがCX固定のままだった — test386のPOST 01
        // (JECXZがECX=0x10000で飛んでしまう) が最初に暴いた
        0xE0 | 0xE1 => {
            let rel = fetch8(m) as i8;
            let a32 = d.addrsize32;
            let cx = m.cpu.reg_w(CX, a32).wrapping_sub(1);
            m.cpu.set_reg_w(CX, cx, a32);
            let cx = if a32 { cx } else { cx & 0xFFFF };
            let zcond = if op == 0xE1 {
                m.cpu.flag(ZF)
            } else {
                !m.cpu.flag(ZF)
            };
            if cx != 0 && zcond {
                let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
                m.cpu.set_ip(ip);
            }
        }
        0xE3 => {
            let rel = fetch8(m) as i8;
            let cx = if d.addrsize32 {
                m.cpu.regs[CX]
            } else {
                m.cpu.reg16(CX) as u32
            };
            if cx == 0 {
                let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
                m.cpu.set_ip(ip);
            }
        }
        0xC2 => {
            let n = fetch16(m) as u32;
            let ip = pop_w(m, d.opsize32);
            m.cpu.set_ip(ip);
            let sp = sp_read(m).wrapping_add(n);
            sp_write(m, sp);
        }

        // --- x87 (0xD8-0xDF): 有無はマシン構成 (MachineProfile) が決める ---
        //
        // **16bit機は挿していない。** 8087を挿していない8086では、ESC命令は
        // 実効アドレスを計算してダミーの読み出しをするだけで、メモリも
        // レジスタも書き換えない (本来は隣に座った8087がバスを盗み見る)。
        // FPUの有無を調べるコードは、番兵を置いた場所に `FNSTSW` で書かせて
        // **書き換わらなかったこと**で不在を知る。だから16bit機で気を利かせて
        // 書くと、逆に「FPUが在る」と誤認させてしまう。
        //
        // **32bit機は挿している。** f64裏打ちの実装 ([`super::fpu`])。
        // muslのstrtodはx87の長倍精度で計算するので、ここが無いと
        // 「sleep 3」の3が化ける (実際に化けてpingが洪水になった)。
        //
        // どちらでもModRMは読む — IPを正しく進めないと次の命令がずれる。
        0xD8..=0xDF => {
            let (reg, rm) = modrm(m, d);
            if m.profile.has_fpu {
                super::fpu::exec(m, op, reg, &rm);
            }
        }

        // --- ストリング命令 (REP対応) ---
        0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF => {
            string::exec(m, d, op)
        }

        // --- INS/OUTS (186): ストリングI/O。実装はREPの機構ごとstringに間借り ---
        0x6C..=0x6F => string::exec(m, d, op),

        // --- 二バイト命令空間 (386〜) ---
        //
        // 8086では `POP CS` だった 0x0F が、186以降で**逃げ道**になった。
        // 1バイトの256席が埋まったので、もう1バイト読んで席を増やす方式である。
        0x0F => twobyte::step_0f(m, d, start_ip),

        // 未実装は panic せず**巻き戻せる停止**にする。機械は生きたまま止まり、
        // レジスタもスタックも覗ける (Linux起動のデバッグループの生命線)
        _ => {
            let _ = start_ip;
            m.trap(format!("unimplemented opcode {op:#04x}"));
        }
    }
}
