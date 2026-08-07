//! 8086 リアルモードCPU。
//!
//! このファイルは**振り分け表**に徹する。オペコードを読み、どの処理に
//! 渡すかを決めるところまでが仕事で、実際の計算は各モジュールが持つ:
//!
//! - [`operand`] — ModRM解決、オペランド読み書き、アドレス変換、スタック
//! - [`alu`] — 8種の演算とフラグ計算 (AF/OFの意味論)
//! - [`shift`] — シフトと回転
//! - [`string`] — ストリング命令とREP
//! - [`decimal`] — 十進補正 (BCD)
//!
//! デコード方針: x86のオペコードは規則的な「グリッド」を持つ部分が大きい。
//! 例えばALU演算は 0x00-0x3D が (演算種別3bit) x (形式3bit) の格子になっており、
//! 48命令を1つのハンドラで処理できる。個別実装は格子から外れるものだけ。
//! 未実装オペコードは即panicして正体を報告する (静かに壊れない)。

pub mod alu;
pub mod decimal;
pub mod operand;
pub mod shift;
pub mod string;

use alu::{alu16, alu8, condition, set_szp16};
use operand::{fetch16, fetch8, linear, modrm, pop16, push16, read_op16, read_op8, write_op16, write_op8, Operand};
use shift::shift_rot;

use crate::Machine;

// レジスタ番号 (x86エンコーディング準拠)
pub const AX: usize = 0;
pub const CX: usize = 1;
pub const DX: usize = 2;
pub const BX: usize = 3;
pub const SP: usize = 4;
pub const BP: usize = 5;
pub const SI: usize = 6;
pub const DI: usize = 7;

// セグメントレジスタ番号
pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;

// FLAGS
pub const CF: u32 = 1 << 0;
pub const PF: u32 = 1 << 2;
pub const AF: u32 = 1 << 4;
pub const ZF: u32 = 1 << 6;
pub const SF: u32 = 1 << 7;
pub const IF: u32 = 1 << 9;
pub const DF: u32 = 1 << 10;
pub const OF: u32 = 1 << 11;
/// トラップフラグ。立っていると1命令ごとに INT 1 が起きる。
/// デバッガのシングルステップはこれで実現されている
pub const TF: u32 = 1 << 8;

pub struct Cpu {
    /// AX CX DX BX SP BP SI DI (将来の32bit拡張を見据えてu32で保持)
    pub regs: [u32; 8],
    /// ES CS SS DS
    pub sregs: [u16; 6],
    pub ip: u16,
    pub flags: u32,
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: [0; 8],
            sregs: [0; 6],
            ip: 0,
            flags: 0x0002, // bit1は常に1
        }
    }

    pub fn set_cs_ip(&mut self, cs: u16, ip: u16) {
        self.sregs[CS] = cs;
        self.ip = ip;
    }

    fn reg16(&self, r: usize) -> u16 {
        self.regs[r] as u16
    }

    fn set_reg16(&mut self, r: usize, v: u16) {
        self.regs[r] = (self.regs[r] & 0xFFFF_0000) | v as u32;
    }

    /// 8bitレジスタ: 0-3 = AL CL DL BL, 4-7 = AH CH DH BH
    fn reg8(&self, r: usize) -> u8 {
        if r < 4 {
            self.regs[r] as u8
        } else {
            (self.regs[r - 4] >> 8) as u8
        }
    }

    fn set_reg8(&mut self, r: usize, v: u8) {
        if r < 4 {
            self.regs[r] = (self.regs[r] & !0xFF) | v as u32;
        } else {
            self.regs[r - 4] = (self.regs[r - 4] & !0xFF00) | (v as u32) << 8;
        }
    }

    pub fn flag(&self, mask: u32) -> bool {
        self.flags & mask != 0
    }

    /// BIOS HLE が「成功/失敗」を返すのに使う。x86のBIOSは慣例として
    /// キャリーフラグで成否を返す
    pub fn set_flag_cf(&mut self, on: bool) {
        self.set_flag(CF, on);
    }

    pub fn set_flag(&mut self, mask: u32, on: bool) {
        if on {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }
}
/// プレフィクスの解析結果
pub struct Decoder {
    pub seg_override: Option<usize>,
    pub rep: Option<u8>,
}

pub fn step(m: &mut Machine) {
    let start_ip = m.cpu.ip;
    let mut d = Decoder { seg_override: None, rep: None };

    // プレフィクスループ
    let op = loop {
        let b = fetch8(m);
        match b {
            0x26 => d.seg_override = Some(ES),
            0x2E => d.seg_override = Some(CS),
            0x36 => d.seg_override = Some(SS),
            0x3E => d.seg_override = Some(DS),
            0xF0 => {} // LOCK: シングルコアなので無視
            0xF2 | 0xF3 => d.rep = Some(b),
            _ => break b,
        }
    };

    match op {
        // --- ALUグリッド: 0x00-0x3D (演算3bit x 形式3bit) ---
        0x00..=0x3F if op & 7 <= 5 && (op & 0x27) != 0x26 && (op & 0x27) != 0x27 => {
            let kind = (op >> 3) & 7;
            match op & 7 {
                0 => {
                    // r/m8, r8
                    let (reg, rm) = modrm(m, &d);
                    let a = read_op8(m, &rm);
                    let b = m.cpu.reg8(reg);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 { write_op8(m, &rm, r); }
                }
                1 => {
                    let (reg, rm) = modrm(m, &d);
                    let a = read_op16(m, &rm);
                    let b = m.cpu.reg16(reg);
                    let r = alu16(&mut m.cpu, kind, a, b);
                    if kind != 7 { write_op16(m, &rm, r); }
                }
                2 => {
                    // r8, r/m8
                    let (reg, rm) = modrm(m, &d);
                    let a = m.cpu.reg8(reg);
                    let b = read_op8(m, &rm);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 { m.cpu.set_reg8(reg, r); }
                }
                3 => {
                    let (reg, rm) = modrm(m, &d);
                    let a = m.cpu.reg16(reg);
                    let b = read_op16(m, &rm);
                    let r = alu16(&mut m.cpu, kind, a, b);
                    if kind != 7 { m.cpu.set_reg16(reg, r); }
                }
                4 => {
                    // AL, imm8
                    let b = fetch8(m);
                    let a = m.cpu.reg8(0);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 { m.cpu.set_reg8(0, r); }
                }
                _ => {
                    // AX, imm16
                    let b = fetch16(m);
                    let a = m.cpu.reg16(AX);
                    let r = alu16(&mut m.cpu, kind, a, b);
                    if kind != 7 { m.cpu.set_reg16(AX, r); }
                }
            }
        }

        // --- GRP1: ALU r/m, imm ---
        0x80 | 0x81 | 0x83 => {
            let (kind, rm) = modrm(m, &d);
            let kind = kind as u8;
            if op == 0x80 {
                let a = read_op8(m, &rm);
                let b = fetch8(m);
                let r = alu8(&mut m.cpu, kind, a, b);
                if kind != 7 { write_op8(m, &rm, r); }
            } else {
                let a = read_op16(m, &rm);
                let b = if op == 0x81 { fetch16(m) } else { fetch8(m) as i8 as u16 };
                let r = alu16(&mut m.cpu, kind, a, b);
                if kind != 7 { write_op16(m, &rm, r); }
            }
        }

        // --- MOV ---
        0x88 => { let (reg, rm) = modrm(m, &d); let v = m.cpu.reg8(reg); write_op8(m, &rm, v); }
        0x89 => { let (reg, rm) = modrm(m, &d); let v = m.cpu.reg16(reg); write_op16(m, &rm, v); }
        0x8A => { let (reg, rm) = modrm(m, &d); let v = read_op8(m, &rm); m.cpu.set_reg8(reg, v); }
        0x8B => { let (reg, rm) = modrm(m, &d); let v = read_op16(m, &rm); m.cpu.set_reg16(reg, v); }
        0x8C => { let (reg, rm) = modrm(m, &d); let v = m.cpu.sregs[reg & 3]; write_op16(m, &rm, v); }
        0x8E => { let (reg, rm) = modrm(m, &d); let v = read_op16(m, &rm); m.cpu.sregs[reg & 3] = v; }
        0xB0..=0xB7 => { let v = fetch8(m); m.cpu.set_reg8((op & 7) as usize, v); }
        0xB8..=0xBF => { let v = fetch16(m); m.cpu.set_reg16((op & 7) as usize, v); }
        0xC6 => { let (_, rm) = modrm(m, &d); let v = fetch8(m); write_op8(m, &rm, v); }
        0xC7 => { let (_, rm) = modrm(m, &d); let v = fetch16(m); write_op16(m, &rm, v); }
        0xA0 => { let off = fetch16(m); let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)]; let v = m.read8(linear(seg, off)); m.cpu.set_reg8(0, v); }
        0xA1 => { let off = fetch16(m); let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)]; let v = m.read16(linear(seg, off)); m.cpu.set_reg16(AX, v); }
        0xA2 => { let off = fetch16(m); let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)]; let v = m.cpu.reg8(0); m.write8(linear(seg, off), v); }
        0xA3 => { let off = fetch16(m); let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)]; let v = m.cpu.reg16(AX); m.write16(linear(seg, off), v); }

        // --- INC/DEC r16 ---
        0x40..=0x47 => {
            let r = (op & 7) as usize;
            let a = m.cpu.reg16(r);
            let v = a.wrapping_add(1);
            m.cpu.set_reg16(r, v);
            m.cpu.set_flag(OF, a == 0x7FFF);
            m.cpu.set_flag(AF, a & 0xF == 0xF);
            set_szp16(&mut m.cpu, v);
        }
        0x48..=0x4F => {
            let r = (op & 7) as usize;
            let a = m.cpu.reg16(r);
            let v = a.wrapping_sub(1);
            m.cpu.set_reg16(r, v);
            m.cpu.set_flag(OF, a == 0x8000);
            m.cpu.set_flag(AF, a & 0xF == 0);
            set_szp16(&mut m.cpu, v);
        }

        // --- PUSH/POP ---
        0x50..=0x57 => { let v = m.cpu.reg16((op & 7) as usize); push16(m, v); }
        0x58..=0x5F => { let v = pop16(m); m.cpu.set_reg16((op & 7) as usize, v); }

        // セグメントレジスタのPUSH/POP。オペコードのbit3-4がそのまま
        // ES/CS/SS/DS の番号になっている (0x06,0x0E,0x16,0x1E)
        0x06 | 0x0E | 0x16 | 0x1E => { let v = m.cpu.sregs[(op >> 3) as usize & 3]; push16(m, v); }
        // POP CS (0x0F) は8086にしか無く、186以降は2バイト命令の導入符になった。
        // ここでは実装しない (Tier 4 で 0x0F を二バイト空間として使う)
        0x07 | 0x17 | 0x1F => { let v = pop16(m); m.cpu.sregs[(op >> 3) as usize & 3] = v; }

        // --- PUSHA/POPA (186) ---
        0x60 => {
            let sp = m.cpu.reg16(SP); // 退避するのは「PUSHA開始時点の」SP
            for r in [AX, CX, DX, BX] {
                let v = m.cpu.reg16(r);
                push16(m, v);
            }
            push16(m, sp);
            for r in [BP, SI, DI] {
                let v = m.cpu.reg16(r);
                push16(m, v);
            }
        }
        0x61 => {
            for r in [DI, SI, BP] {
                let v = pop16(m);
                m.cpu.set_reg16(r, v);
            }
            pop16(m); // 積んだSPは捨てる (POPAの結果SPは自然に元へ戻る)
            for r in [BX, DX, CX, AX] {
                let v = pop16(m);
                m.cpu.set_reg16(r, v);
            }
        }

        // --- PUSH imm (186) ---
        0x68 => { let v = fetch16(m); push16(m, v); }
        0x6A => { let v = fetch8(m) as i8 as u16; push16(m, v); }

        // --- IMUL r16, r/m16, imm (186) ---
        // 3オペランドの乗算。CF/OFは「結果が16bitに収まらなかったか」だけを表し、
        // SF/ZF/AF/PF は未定義
        0x69 | 0x6B => {
            let (reg, rm) = modrm(m, &d);
            let a = read_op16(m, &rm) as i16 as i32;
            let b = if op == 0x69 { fetch16(m) as i16 as i32 } else { fetch8(m) as i8 as i32 };
            let r = a * b;
            m.cpu.set_reg16(reg, r as u16);
            let ext = (r as i16 as i32) != r;
            m.cpu.set_flag(CF, ext);
            m.cpu.set_flag(OF, ext);
        }

        // --- ENTER/LEAVE (186): スタックフレームの作成と破棄 ---
        0xC8 => {
            let size = fetch16(m);
            let level = fetch8(m) & 0x1F;
            let bp = m.cpu.reg16(BP);
            push16(m, bp);
            let frame = m.cpu.reg16(SP);
            if level > 0 {
                // ネストした手続きの表示 (display) を積む。Pascal系言語のための機構で、
                // Cしか使わない現代では level=0 しか出てこない
                for _ in 1..level {
                    let b = m.cpu.reg16(BP).wrapping_sub(2);
                    m.cpu.set_reg16(BP, b);
                    let v = m.read16(linear(m.cpu.sregs[SS], b));
                    push16(m, v);
                }
                push16(m, frame);
            }
            m.cpu.set_reg16(BP, frame);
            // 最後のSP調整は「今のSP」から引く。Intel SDMの疑似コードは
            // `SP <- BP - Size` と書いているが、これが正しいのは level=0 のときだけで、
            // level>0 では display を積んだ分 (level*2バイト) が抜け落ちる。
            // AMDのマニュアルとQEMUの実装は現在のSPから引いており、そちらが実挙動。
            // co-simがこの差を捕まえた
            let sp = m.cpu.reg16(SP).wrapping_sub(size);
            m.cpu.set_reg16(SP, sp);
        }
        0xC9 => {
            let bp = m.cpu.reg16(BP);
            m.cpu.set_reg16(SP, bp);
            let v = pop16(m);
            m.cpu.set_reg16(BP, v);
        }

        // --- LES/LDS: メモリから「オフセットとセグメント」を一度に取る ---
        // far ポインタ (4バイト) を読み、下位2バイトを汎用レジスタへ、
        // 上位2バイトをセグメントレジスタへ入れる
        0xC4 | 0xC5 => {
            let (reg, rm) = modrm(m, &d);
            let addr = match rm {
                Operand::Mem { addr, .. } => addr,
                Operand::Reg(_) => panic!("LES/LDS with register operand"),
            };
            let off = m.read16(addr);
            let seg = m.read16(addr.wrapping_add(2));
            m.cpu.set_reg16(reg, off);
            m.cpu.sregs[if op == 0xC4 { ES } else { DS }] = seg;
        }

        // --- XLAT: AL = [BX + AL] ---
        // 256バイトの変換テーブルを1命令で引く。文字コード変換のための命令
        0xD7 => {
            let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)];
            let off = m.cpu.reg16(BX).wrapping_add(m.cpu.reg8(0) as u16);
            let v = m.read8(linear(seg, off));
            m.cpu.set_reg8(0, v);
        }

        // --- IN/OUT: I/Oポート空間へのアクセス ---
        // オペコードのビットがそのまま形式を表す:
        //   bit0 = 幅 (0:8bit 1:16bit)  bit1 = 向き (0:IN 1:OUT)  bit3 = ポート指定 (0:imm8 1:DX)
        0xE4..=0xE7 | 0xEC..=0xEF => {
            let port = if op & 8 != 0 { m.cpu.reg16(DX) } else { fetch8(m) as u16 };
            let wide = op & 1 != 0;
            let out = op & 2 != 0;
            match (out, wide) {
                (false, false) => { let v = m.io_read8(port); m.cpu.set_reg8(0, v); }
                (false, true) => { let v = m.io_read16(port); m.cpu.set_reg16(AX, v); }
                (true, false) => { let v = m.cpu.reg8(0); m.io_write8(port, v); }
                (true, true) => { let v = m.cpu.reg16(AX); m.io_write16(port, v); }
            }
        }

        // WAIT: コプロセッサ待ち。FPUが無いので何もしない
        0x9B => {}

        // --- ジャンプ/コール ---
        0x70..=0x7F => {
            let rel = fetch8(m) as i8;
            if condition(&m.cpu, op & 0xF) {
                m.cpu.ip = m.cpu.ip.wrapping_add(rel as u16);
            }
        }
        0xE8 => { let rel = fetch16(m); let ret = m.cpu.ip; push16(m, ret); m.cpu.ip = ret.wrapping_add(rel); }
        0xE9 => { let rel = fetch16(m); m.cpu.ip = m.cpu.ip.wrapping_add(rel); }
        0xEB => { let rel = fetch8(m) as i8; m.cpu.ip = m.cpu.ip.wrapping_add(rel as u16); }
        0xC3 => { m.cpu.ip = pop16(m); }

        // --- far転送: CSごと移る ---
        // リアルモードでは「CSに値を代入する」だけだが、プロテクトモードでは
        // 同じ命令がディスクリプタ引きと特権チェックに化ける (Tier 4)。
        0xEA => { let off = fetch16(m); let seg = fetch16(m); m.cpu.sregs[CS] = seg; m.cpu.ip = off; }
        0x9A => {
            let off = fetch16(m);
            let seg = fetch16(m);
            let cs = m.cpu.sregs[CS];
            push16(m, cs);
            let ret = m.cpu.ip;
            push16(m, ret);
            m.cpu.sregs[CS] = seg;
            m.cpu.ip = off;
        }
        0xCB => { m.cpu.ip = pop16(m); m.cpu.sregs[CS] = pop16(m); }
        0xCA => {
            let n = fetch16(m);
            m.cpu.ip = pop16(m);
            m.cpu.sregs[CS] = pop16(m);
            let sp = m.cpu.reg16(SP).wrapping_add(n);
            m.cpu.set_reg16(SP, sp);
        }
        // IRET: 割り込みハンドラからの復帰。CALLと違いFLAGSも戻す。
        // 割り込み中に変わったIF/DFを呼び出し前の値へ戻すのが要点。
        0xCF => iret(m),
        0xE2 => {
            // LOOP
            let rel = fetch8(m) as i8;
            let cx = m.cpu.reg16(CX).wrapping_sub(1);
            m.cpu.set_reg16(CX, cx);
            if cx != 0 {
                m.cpu.ip = m.cpu.ip.wrapping_add(rel as u16);
            }
        }

        // --- 割り込み (BIOS HLE) ---
        // Tier 1d でここを実IVTディスパッチに置き換える。
        // OSは起動時にIVTを自分のハンドラで書き換えるので、
        // ホスト関数へ横流しする今の方式ではOSが動かない。
        0xCD => { let n = fetch8(m); interrupt(m, n); }
        0xCC => interrupt(m, 3), // INT3 (デバッガのブレークポイント)
        0xCE => {
            // INTO: OFが立っているときだけ割り込み4。立っていなければ何もしない
            if m.cpu.flag(OF) {
                interrupt(m, 4);
            }
        }

        // --- フラグ/制御 ---
        0xF4 => m.halted = true,
        0xF5 => { let c = m.cpu.flag(CF); m.cpu.set_flag(CF, !c); } // CMC
        0xF8 => m.cpu.set_flag(CF, false), // CLC
        0xF9 => m.cpu.set_flag(CF, true),  // STC
        0xFA => m.cpu.set_flag(IF, false), // CLI
        0xFB => m.cpu.set_flag(IF, true),  // STI
        0xFC => m.cpu.set_flag(DF, false), // CLD
        0xFD => m.cpu.set_flag(DF, true),  // STD


        // --- TEST / XCHG / LEA ---
        0x84 => { let (reg, rm) = modrm(m, &d); let a = read_op8(m, &rm); let b = m.cpu.reg8(reg); alu8(&mut m.cpu, 4, a, b); }
        0x85 => { let (reg, rm) = modrm(m, &d); let a = read_op16(m, &rm); let b = m.cpu.reg16(reg); alu16(&mut m.cpu, 4, a, b); }
        0x86 => {
            let (reg, rm) = modrm(m, &d);
            let a = read_op8(m, &rm);
            let b = m.cpu.reg8(reg);
            write_op8(m, &rm, b);
            m.cpu.set_reg8(reg, a);
        }
        0x87 => {
            let (reg, rm) = modrm(m, &d);
            let a = read_op16(m, &rm);
            let b = m.cpu.reg16(reg);
            write_op16(m, &rm, b);
            m.cpu.set_reg16(reg, a);
        }
        0x8D => {
            // LEA: セグメントを適用しない実効オフセットを取る
            let (reg, rm) = modrm(m, &d);
            match rm {
                Operand::Mem { off, .. } => m.cpu.set_reg16(reg, off),
                Operand::Reg(_) => panic!("LEA with register operand"),
            }
        }
        0x8F => { let (_, rm) = modrm(m, &d); let v = pop16(m); write_op16(m, &rm, v); }
        0x90..=0x97 => {
            let r = (op & 7) as usize;
            let a = m.cpu.reg16(AX);
            let b = m.cpu.reg16(r);
            m.cpu.set_reg16(AX, b);
            m.cpu.set_reg16(r, a);
        }
        0x98 => { let v = m.cpu.reg8(0) as i8 as i16 as u16; m.cpu.set_reg16(AX, v); }
        0x99 => { let v = if m.cpu.reg16(AX) & 0x8000 != 0 { 0xFFFF } else { 0 }; m.cpu.set_reg16(DX, v); }
        0x9C => { let f = (m.cpu.flags as u16) | 0xF002; push16(m, f); }
        0x9D => { let f = pop16(m); m.cpu.flags = (f as u32 & 0x0FD5) | 0x0002; }
        0x9E => {
            // SAHF: AHの下位バイトをフラグへ
            let ah = m.cpu.reg8(4) as u32;
            m.cpu.flags = (m.cpu.flags & !0xD5) | (ah & 0xD5) | 0x0002;
        }
        0x9F => { let f = (m.cpu.flags as u8 & 0xD5) | 0x02; m.cpu.set_reg8(4, f); }
        0xA8 => { let b = fetch8(m); let a = m.cpu.reg8(0); alu8(&mut m.cpu, 4, a, b); }
        0xA9 => { let b = fetch16(m); let a = m.cpu.reg16(AX); alu16(&mut m.cpu, 4, a, b); }

        // --- GRP2: シフト/回転 ---
        0xC0 | 0xC1 | 0xD0 | 0xD1 | 0xD2 | 0xD3 => {
            let (kind, rm) = modrm(m, &d);
            let count = match op {
                0xC0 | 0xC1 => fetch8(m),
                0xD0 | 0xD1 => 1,
                _ => m.cpu.reg8(1), // CL
            };
            if op & 1 == 0 {
                let a = read_op8(m, &rm) as u32;
                let r = shift_rot(&mut m.cpu, kind as u8, a, count, 8);
                write_op8(m, &rm, r as u8);
            } else {
                let a = read_op16(m, &rm) as u32;
                let r = shift_rot(&mut m.cpu, kind as u8, a, count, 16);
                write_op16(m, &rm, r as u16);
            }
        }

        // --- GRP3: TEST/NOT/NEG/MUL/IMUL/DIV/IDIV ---
        0xF6 => {
            let (kind, rm) = modrm(m, &d);
            let a = read_op8(m, &rm);
            match kind {
                0 | 1 => { let b = fetch8(m); alu8(&mut m.cpu, 4, a, b); }
                2 => write_op8(m, &rm, !a),
                3 => {
                    let r = alu8(&mut m.cpu, 5, 0, a);
                    m.cpu.set_flag(CF, a != 0);
                    write_op8(m, &rm, r);
                }
                4 => {
                    let r = m.cpu.reg8(0) as u16 * a as u16;
                    m.cpu.set_reg16(AX, r);
                    let hi = r >> 8 != 0;
                    m.cpu.set_flag(CF, hi);
                    m.cpu.set_flag(OF, hi);
                }
                5 => {
                    let r = (m.cpu.reg8(0) as i8 as i16) * (a as i8 as i16);
                    m.cpu.set_reg16(AX, r as u16);
                    let ext = (r as i8 as i16) != r;
                    m.cpu.set_flag(CF, ext);
                    m.cpu.set_flag(OF, ext);
                }
                6 => {
                    let ax = m.cpu.reg16(AX);
                    if a == 0 { return divide_error(m, start_ip); }
                    let q = ax / a as u16;
                    if q > 0xFF { return divide_error(m, start_ip); }
                    m.cpu.set_reg8(0, q as u8);
                    m.cpu.set_reg8(4, (ax % a as u16) as u8);
                }
                _ => {
                    let ax = m.cpu.reg16(AX) as i16;
                    let b = a as i8 as i16;
                    if b == 0 { return divide_error(m, start_ip); }
                    let q = ax / b;
                    if q > 127 || q < -128 { return divide_error(m, start_ip); }
                    m.cpu.set_reg8(0, q as u8);
                    m.cpu.set_reg8(4, (ax % b) as u8);
                }
            }
        }
        0xF7 => {
            let (kind, rm) = modrm(m, &d);
            let a = read_op16(m, &rm);
            match kind {
                0 | 1 => { let b = fetch16(m); alu16(&mut m.cpu, 4, a, b); }
                2 => write_op16(m, &rm, !a),
                3 => {
                    let r = alu16(&mut m.cpu, 5, 0, a);
                    m.cpu.set_flag(CF, a != 0);
                    write_op16(m, &rm, r);
                }
                4 => {
                    let r = m.cpu.reg16(AX) as u32 * a as u32;
                    m.cpu.set_reg16(AX, r as u16);
                    m.cpu.set_reg16(DX, (r >> 16) as u16);
                    let hi = r >> 16 != 0;
                    m.cpu.set_flag(CF, hi);
                    m.cpu.set_flag(OF, hi);
                }
                5 => {
                    let r = (m.cpu.reg16(AX) as i16 as i32) * (a as i16 as i32);
                    m.cpu.set_reg16(AX, r as u16);
                    m.cpu.set_reg16(DX, (r >> 16) as u16);
                    let ext = (r as i16 as i32) != r;
                    m.cpu.set_flag(CF, ext);
                    m.cpu.set_flag(OF, ext);
                }
                6 => {
                    let n = ((m.cpu.reg16(DX) as u32) << 16) | m.cpu.reg16(AX) as u32;
                    if a == 0 { return divide_error(m, start_ip); }
                    let q = n / a as u32;
                    if q > 0xFFFF { return divide_error(m, start_ip); }
                    m.cpu.set_reg16(AX, q as u16);
                    m.cpu.set_reg16(DX, (n % a as u32) as u16);
                }
                _ => {
                    let n = (((m.cpu.reg16(DX) as u32) << 16) | m.cpu.reg16(AX) as u32) as i32;
                    let b = a as i16 as i32;
                    if b == 0 { return divide_error(m, start_ip); }
                    let q = n / b;
                    if q > 32767 || q < -32768 { return divide_error(m, start_ip); }
                    m.cpu.set_reg16(AX, q as u16);
                    m.cpu.set_reg16(DX, (n % b) as u16);
                }
            }
        }

        // --- GRP4/GRP5 ---
        0xFE => {
            let (kind, rm) = modrm(m, &d);
            let a = read_op8(m, &rm);
            let cf = m.cpu.flag(CF);
            let r = alu8(&mut m.cpu, if kind == 0 { 0 } else { 5 }, a, 1);
            m.cpu.set_flag(CF, cf); // INC/DECはCFを変更しない
            write_op8(m, &rm, r);
        }
        0xFF => {
            let (kind, rm) = modrm(m, &d);
            match kind {
                0 | 1 => {
                    let a = read_op16(m, &rm);
                    let cf = m.cpu.flag(CF);
                    let r = alu16(&mut m.cpu, if kind == 0 { 0 } else { 5 }, a, 1);
                    m.cpu.set_flag(CF, cf);
                    write_op16(m, &rm, r);
                }
                2 => { let t = read_op16(m, &rm); let ret = m.cpu.ip; push16(m, ret); m.cpu.ip = t; }
                4 => { let t = read_op16(m, &rm); m.cpu.ip = t; }
                6 => { let v = read_op16(m, &rm); push16(m, v); }
                // /3 CALL far、/5 JMP far: メモリ上の4バイト far ポインタを読んで飛ぶ
                3 | 5 => {
                    let addr = match rm {
                        Operand::Mem { addr, .. } => addr,
                        Operand::Reg(_) => panic!("far call/jmp with register operand"),
                    };
                    let off = m.read16(addr);
                    let seg = m.read16(addr.wrapping_add(2));
                    if kind == 3 {
                        let cs = m.cpu.sregs[CS];
                        push16(m, cs);
                        let ret = m.cpu.ip;
                        push16(m, ret);
                    }
                    m.cpu.sregs[CS] = seg;
                    m.cpu.ip = off;
                }
                _ => panic!("GRP5 /{kind} not implemented"),
            }
        }

        // --- 十進補正 ---
        0x27 | 0x2F => decimal::daa_das(m, op),
        0x37 | 0x3F => decimal::aaa_aas(m, op),
        0xD4 => decimal::aam(m),
        0xD5 => decimal::aad(m),

        // --- ループ/条件ジャンプの残り ---
        0xE0 | 0xE1 => {
            let rel = fetch8(m) as i8;
            let cx = m.cpu.reg16(CX).wrapping_sub(1);
            m.cpu.set_reg16(CX, cx);
            let zcond = if op == 0xE1 { m.cpu.flag(ZF) } else { !m.cpu.flag(ZF) };
            if cx != 0 && zcond {
                m.cpu.ip = m.cpu.ip.wrapping_add(rel as u16);
            }
        }
        0xE3 => {
            let rel = fetch8(m) as i8;
            if m.cpu.reg16(CX) == 0 {
                m.cpu.ip = m.cpu.ip.wrapping_add(rel as u16);
            }
        }
        0xC2 => { let n = fetch16(m); m.cpu.ip = pop16(m); let sp = m.cpu.reg16(SP).wrapping_add(n); m.cpu.set_reg16(SP, sp); }

        // --- ストリング命令 (REP対応) ---
        0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF => string::exec(m, &d, op),

        _ => panic!(
            "unimplemented opcode {op:#04x} at {:04x}:{:04x}",
            m.cpu.sregs[CS], start_ip
        ),
    }
}

/// 割り込み・例外の共通入口。**実IVTを引いてハンドラへ飛ぶ**。
///
/// ソフトウェア割り込み (`INT n`)、例外 (ゼロ除算など)、ハードウェア割り込み
/// (PICからのIRQ) は入口が違うだけで、ここから先は同じ道を通る。
///
/// 積む順序が `CALL far` と違う点に注意: **FLAGSを先に積む**。`IRET` が
/// 逆順に取り出すので、ハンドラ実行中に変わったIF/DFが呼び出し前へ戻る。
pub fn interrupt(m: &mut Machine, n: u8) {
    let (cs, ip) = (m.cpu.sregs[CS], m.cpu.ip);
    let i = n as usize;
    if m.int_counts[i] == 0 {
        m.int_first[i] = (cs, ip);
    }
    m.int_counts[i] += 1;
    if m.int_recent.len() == 32 {
        m.int_recent.pop_front();
    }
    m.int_recent.push_back((n, cs, ip));
    let f = (m.cpu.flags as u16) | 0xF002;
    push16(m, f);
    // ハンドラ実行中は多重割り込みとシングルステップを止める。
    // 必要ならハンドラ側が STI で開け直す (これが「割り込み禁止区間」の正体)
    m.cpu.set_flag(IF, false);
    m.cpu.set_flag(TF, false);
    let cs = m.cpu.sregs[CS];
    push16(m, cs);
    let ip = m.cpu.ip;
    push16(m, ip);
    // IVTは 0x0000 から 4バイト × 256個。n番目に [オフセット, セグメント] が並ぶ。
    // **OSはここを自分のハンドラで書き換えて割り込みを乗っ取る**
    let vec = n as u32 * 4;
    m.cpu.ip = m.read16(vec);
    m.cpu.sregs[CS] = m.read16(vec + 2);
}

/// 割り込みからの復帰。IP・CS・FLAGS をこの順で取り出す
pub fn iret(m: &mut Machine) {
    m.cpu.ip = pop16(m);
    m.cpu.sregs[CS] = pop16(m);
    let f = pop16(m);
    m.cpu.flags = (f as u32 & 0x0FD5) | 0x0002;
}

/// ゼロ除算・商オーバーフローで上がる #DE (INT 0)。
///
/// **フォールトなので、積むのは「失敗した命令の先頭」**である。次の命令ではない。
/// ハンドラが原因を直して `IRET` すれば同じ除算をやり直せる、という設計。
/// (8086は次の命令を積む実装だったが、286以降で今の形に直された)
fn divide_error(m: &mut Machine, start_ip: u16) {
    m.cpu.ip = start_ip;
    if m.first_fault.is_none() {
        m.first_fault = Some((0, m.cpu.sregs[CS], start_ip));
    }
    interrupt(m, 0);
}
