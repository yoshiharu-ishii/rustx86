//! 8086 リアルモードCPU。
//!
//! デコード方針: x86のオペコードは規則的な「グリッド」を持つ部分が大きい。
//! 例えばALU演算は 0x00-0x3D が (演算種別3bit) x (形式3bit) の格子になっており、
//! 48命令を1つのハンドラで処理できる。個別実装は格子から外れるものだけ。
//! 未実装オペコードは即panicして正体を報告する (静かに壊れない)。

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

    fn flag(&self, mask: u32) -> bool {
        self.flags & mask != 0
    }

    fn set_flag(&mut self, mask: u32, on: bool) {
        if on {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }
}

/// リニアアドレス変換 (リアルモード: seg*16 + off、20bitでラップ)
fn linear(seg: u16, off: u16) -> u32 {
    ((seg as u32) << 4).wrapping_add(off as u32) & 0xF_FFFF
}

/// ModRMのデコード結果
enum Operand {
    Reg(usize),
    Mem(u32),
}

struct Decoder {
    seg_override: Option<usize>,
    rep: Option<u8>,
}

fn fetch8(m: &mut Machine) -> u8 {
    let v = m.read8(linear(m.cpu.sregs[CS], m.cpu.ip));
    m.cpu.ip = m.cpu.ip.wrapping_add(1);
    v
}

fn fetch16(m: &mut Machine) -> u16 {
    let lo = fetch8(m) as u16;
    let hi = fetch8(m) as u16;
    hi << 8 | lo
}

/// ModRMバイトを読み、(reg番号, 実効オペランド) を返す (16bitアドレッシング)
fn modrm(m: &mut Machine, d: &Decoder) -> (usize, Operand) {
    let b = fetch8(m);
    let md = b >> 6;
    let reg = ((b >> 3) & 7) as usize;
    let rm = (b & 7) as usize;
    if md == 3 {
        return (reg, Operand::Reg(rm));
    }
    let c = &m.cpu;
    // 16bit実効アドレスの基底 (rm=6かつmod=0はdisp16直接)
    let (base, default_seg) = match rm {
        0 => (c.reg16(BX).wrapping_add(c.reg16(SI)), DS),
        1 => (c.reg16(BX).wrapping_add(c.reg16(DI)), DS),
        2 => (c.reg16(BP).wrapping_add(c.reg16(SI)), SS),
        3 => (c.reg16(BP).wrapping_add(c.reg16(DI)), SS),
        4 => (c.reg16(SI), DS),
        5 => (c.reg16(DI), DS),
        6 => {
            if md == 0 {
                (0, DS) // disp16のみ
            } else {
                (c.reg16(BP), SS)
            }
        }
        _ => (c.reg16(BX), DS),
    };
    let disp = match md {
        0 => {
            if rm == 6 {
                fetch16(m)
            } else {
                0
            }
        }
        1 => fetch8(m) as i8 as u16,
        _ => fetch16(m),
    };
    let off = base.wrapping_add(disp);
    let seg = m.cpu.sregs[d.seg_override.unwrap_or(default_seg)];
    (reg, Operand::Mem(linear(seg, off)))
}

fn read_op8(m: &Machine, op: &Operand) -> u8 {
    match *op {
        Operand::Reg(r) => m.cpu.reg8(r),
        Operand::Mem(a) => m.read8(a),
    }
}

fn write_op8(m: &mut Machine, op: &Operand, v: u8) {
    match *op {
        Operand::Reg(r) => m.cpu.set_reg8(r, v),
        Operand::Mem(a) => m.write8(a, v),
    }
}

fn read_op16(m: &Machine, op: &Operand) -> u16 {
    match *op {
        Operand::Reg(r) => m.cpu.reg16(r),
        Operand::Mem(a) => m.read16(a),
    }
}

fn write_op16(m: &mut Machine, op: &Operand, v: u16) {
    match *op {
        Operand::Reg(r) => m.cpu.set_reg16(r, v),
        Operand::Mem(a) => m.write16(a, v),
    }
}

// --- ALU (8種の演算: ADD OR ADC SBB AND SUB XOR CMP) ---

fn alu8(c: &mut Cpu, op: u8, a: u8, b: u8) -> u8 {
    let carry = c.flag(CF) as u16;
    let (r, cf, of, af) = match op {
        0 => {
            let r = a as u16 + b as u16;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) > 0xF)
        }
        1 => ((a | b) as u16, false, false, false),
        2 => {
            let r = a as u16 + b as u16 + carry;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) + carry as u8 > 0xF)
        }
        3 => {
            let r = (a as u16).wrapping_sub(b as u16).wrapping_sub(carry);
            (r, (a as u16) < b as u16 + carry, ((a ^ b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) < (b & 0xF) + carry as u8)
        }
        4 => ((a & b) as u16, false, false, false),
        5 | 7 => {
            let r = (a as u16).wrapping_sub(b as u16);
            (r, (a as u16) < b as u16, ((a ^ b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) < (b & 0xF))
        }
        _ => ((a ^ b) as u16, false, false, false), // 6 = XOR
    };
    let r8 = r as u8;
    c.set_flag(CF, cf);
    c.set_flag(OF, of);
    c.set_flag(AF, af);
    set_szp8(c, r8);
    if op == 7 { a } else { r8 } // CMPは結果を書き戻さない
}

fn alu16(c: &mut Cpu, op: u8, a: u16, b: u16) -> u16 {
    let carry = c.flag(CF) as u32;
    let (r, cf, of, af) = match op {
        0 => {
            let r = a as u32 + b as u32;
            (r, r > 0xFFFF, ((a ^ !b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) + (b & 0xF) > 0xF)
        }
        1 => ((a | b) as u32, false, false, false),
        2 => {
            let r = a as u32 + b as u32 + carry;
            (r, r > 0xFFFF, ((a ^ !b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) + (b & 0xF) + carry as u16 > 0xF)
        }
        3 => {
            let r = (a as u32).wrapping_sub(b as u32).wrapping_sub(carry);
            (r, (a as u32) < b as u32 + carry, ((a ^ b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) < (b & 0xF) + carry as u16)
        }
        4 => ((a & b) as u32, false, false, false),
        5 | 7 => {
            let r = (a as u32).wrapping_sub(b as u32);
            (r, (a as u32) < b as u32, ((a ^ b) & (a ^ r as u16)) & 0x8000 != 0, (a & 0xF) < (b & 0xF))
        }
        _ => ((a ^ b) as u32, false, false, false),
    };
    let r16 = r as u16;
    c.set_flag(CF, cf);
    c.set_flag(OF, of);
    c.set_flag(AF, af);
    set_szp16(c, r16);
    if op == 7 { a } else { r16 }
}

fn set_szp8(c: &mut Cpu, v: u8) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x80 != 0);
    c.set_flag(PF, v.count_ones() % 2 == 0);
}

fn set_szp16(c: &mut Cpu, v: u16) {
    c.set_flag(ZF, v == 0);
    c.set_flag(SF, v & 0x8000 != 0);
    c.set_flag(PF, (v as u8).count_ones() % 2 == 0); // PFは下位8bitのみ
}

/// Jcc条件 (cc = オペコード下位4bit)
fn condition(c: &Cpu, cc: u8) -> bool {
    let r = match cc >> 1 {
        0 => c.flag(OF),
        1 => c.flag(CF),
        2 => c.flag(ZF),
        3 => c.flag(CF) || c.flag(ZF),
        4 => c.flag(SF),
        5 => c.flag(PF),
        6 => c.flag(SF) != c.flag(OF),
        _ => c.flag(ZF) || (c.flag(SF) != c.flag(OF)),
    };
    if cc & 1 != 0 { !r } else { r }
}

fn push16(m: &mut Machine, v: u16) {
    let sp = m.cpu.reg16(SP).wrapping_sub(2);
    m.cpu.set_reg16(SP, sp);
    let addr = linear(m.cpu.sregs[SS], sp);
    m.write16(addr, v);
}

fn pop16(m: &mut Machine) -> u16 {
    let sp = m.cpu.reg16(SP);
    let v = m.read16(linear(m.cpu.sregs[SS], sp));
    m.cpu.set_reg16(SP, sp.wrapping_add(2));
    v
}

/// 1命令の実行
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

        // --- ストリング命令 ---
        0xAC => {
            // LODSB
            let seg = m.cpu.sregs[d.seg_override.unwrap_or(DS)];
            let si = m.cpu.reg16(SI);
            let v = m.read8(linear(seg, si));
            m.cpu.set_reg8(0, v);
            let delta = if m.cpu.flag(DF) { si.wrapping_sub(1) } else { si.wrapping_add(1) };
            m.cpu.set_reg16(SI, delta);
        }
        0xAA => {
            // STOSB
            let di = m.cpu.reg16(DI);
            let v = m.cpu.reg8(0);
            m.write8(linear(m.cpu.sregs[ES], di), v);
            let delta = if m.cpu.flag(DF) { di.wrapping_sub(1) } else { di.wrapping_add(1) };
            m.cpu.set_reg16(DI, delta);
        }

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
        0xCD => {
            let n = fetch8(m);
            m.bios_interrupt(n);
        }

        // --- フラグ/制御 ---
        0x90 => {} // NOP
        0xF4 => m.halted = true,
        0xF5 => { let c = m.cpu.flag(CF); m.cpu.set_flag(CF, !c); } // CMC
        0xF8 => m.cpu.set_flag(CF, false), // CLC
        0xF9 => m.cpu.set_flag(CF, true),  // STC
        0xFA => m.cpu.set_flag(IF, false), // CLI
        0xFB => m.cpu.set_flag(IF, true),  // STI
        0xFC => m.cpu.set_flag(DF, false), // CLD
        0xFD => m.cpu.set_flag(DF, true),  // STD

        _ => panic!(
            "unimplemented opcode {op:#04x} at {:04x}:{:04x}",
            m.cpu.sregs[CS], start_ip
        ),
    }
}
