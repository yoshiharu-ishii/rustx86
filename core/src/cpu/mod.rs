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

use alu::{alu16, alu8, alu_w, condition};
use operand::{
    fetch16, fetch32, fetch8, fetch_w, modrm, pop16, pop32, pop_w, push16, push32, push_w,
    read_op16, read_op8, read_op_w, write_op16, write_op8, write_op_w, Operand,
};
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
    /// ES CS SS DS FS GS — **見える部分 (セレクタ)**。
    /// 保護モードでは番地の材料ではなく、GDTの行番号になる
    pub sregs: [u16; 6],
    /// EIP。**16bitコードでは下位16bitだけが意味を持ち、64Kで折り返す**。
    /// 幅を決めるのはモードとCSのDビットで、レジスタ自体は最初から32bit
    /// (regsをu32で持っているのと同じ判断)
    pub ip: u32,
    pub flags: u32,
    /// CR0。bit0 = PE (Protection Enable)。これが立つと sregs の意味が変わる
    pub cr0: u32,
    /// GDTR (LGDTで積む)。記述子表の場所と大きさ
    pub gdtr_base: u32,
    pub gdtr_limit: u16,
    /// IDTR (LIDTで積む)。保護モードの割り込みはIVTではなくこの表を引く
    pub idtr_base: u32,
    pub idtr_limit: u16,
    /// TR (LTRで積む)。TSSの場所 — リング3→0の瞬間に使うスタックの置き場。
    /// **リングで唯一、本当に新しい部品** (docs/registers.md)
    pub tr_sel: u16,
    pub tr_base: u32,
    pub tr_limit: u32,
    /// セグメントの**隠しレジスタ** (実機と同じ構造)。
    ///
    /// 実機の386は、セグメントレジスタをロードした瞬間に記述子の中身
    /// (base/limit/属性) をCPU内に写し取り、以後のアクセスは**この写しだけ**を
    /// 見る。GDTを後から書き換えても、ロードし直すまで反映されない。
    /// リアルモードは「写しに常に sel×16 が入っている」特殊ケースにすぎない —
    /// この統一がプロテクトモードの正体である
    pub hidden: [SegHidden; 6],
}

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
    fn real(sel: u16) -> Self {
        Self {
            base: (sel as u32) << 4,
            limit: 0xFFFF,
            access: 0x93, // present, data, writable相当
            big: false,
        }
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: [0; 8],
            sregs: [0; 6],
            ip: 0,
            flags: 0x0002, // bit1は常に1
            cr0: 0,
            gdtr_base: 0,
            gdtr_limit: 0,
            idtr_base: 0,
            idtr_limit: 0,
            tr_sel: 0,
            tr_base: 0,
            tr_limit: 0,
            hidden: [SegHidden::real(0); 6],
        }
    }

    /// プロテクトモードか (CR0.PE)
    pub fn pe(&self) -> bool {
        self.cr0 & 1 != 0
    }

    /// CPL (現在特権レベル)。独立したレジスタではない —
    /// **いま走っているCSセレクタの下位2bit**がそのまま現在特権である。
    /// リアルモードは常に0 (全能)
    pub fn cpl(&self) -> u8 {
        if self.pe() {
            (self.sregs[CS] & 3) as u8
        } else {
            0
        }
    }

    /// セグメントのbase。
    ///
    /// リアルモードでは**セレクタから毎回計算する** (sel×16)。写しを読まないのは、
    /// テストや既存コードが `sregs[i] = x` と直接書いても嘘にならないようにするため。
    /// 保護モードに入った瞬間に写しへ切り替わる (実機はこの瞬間も写しを見続けて
    /// いるが、入る側で写しを初期化するので観測できる差は無い)
    pub fn seg_base(&self, i: usize) -> u32 {
        if self.pe() {
            self.hidden[i].base
        } else {
            (self.sregs[i] as u32) << 4
        }
    }

    /// そのセグメントのDビット (32bitか)。リアルモードでは常に16bit
    pub fn seg_is32(&self, i: usize) -> bool {
        self.pe() && self.hidden[i].big
    }

    /// いまのコードの「IPの幅」。16bitコードでは64Kで折り返す。
    /// 折り返しの判断を呼び出し側に散らばらせない — **ここだけが幅を知る**
    pub fn ip_mask(&self) -> u32 {
        if self.seg_is32(CS) {
            0xFFFF_FFFF
        } else {
            0xFFFF
        }
    }

    /// IPを据える (幅で折り返す)
    pub fn set_ip(&mut self, v: u32) {
        self.ip = v & self.ip_mask();
    }

    /// IPを進める (幅で折り返す)。フェッチの1バイトごとに呼ばれる熱い経路だが、
    /// ANDが1個増えるだけである
    pub fn advance_ip(&mut self, n: u32) {
        self.ip = self.ip.wrapping_add(n) & self.ip_mask();
    }

    /// セグメント適用後のリニアアドレス。
    /// リアルモードは1MBで折り返す (8086のアドレスバスが20本だったため)
    pub fn lin(&self, seg: usize, off: u32) -> u32 {
        let a = self.seg_base(seg).wrapping_add(off);
        if self.pe() {
            a
        } else {
            a & 0xF_FFFF
        }
    }

    pub fn set_cs_ip(&mut self, cs: u16, ip: u16) {
        self.sregs[CS] = cs;
        self.ip = ip as u32;
    }

    fn reg16(&self, r: usize) -> u16 {
        self.regs[r] as u16
    }

    fn set_reg16(&mut self, r: usize, v: u16) {
        self.regs[r] = (self.regs[r] & 0xFFFF_0000) | v as u32;
    }

    /// 32bitレジスタ (EAX〜EDI)。
    ///
    /// **レジスタは最初から `u32` で持っている**ので、386拡張のために
    /// 器を作り直す必要は無かった。16bit命令が上位16bitを保存するのも
    /// [`set_reg16`](Self::set_reg16) がそう書いてあるからで、
    /// 実機と同じ「同じ器の下半分を見ている」関係がそのまま出ている
    fn reg32(&self, r: usize) -> u32 {
        self.regs[r]
    }

    /// 32bit書き込みは**上位も含めて全部置き換える** (16bitのような保存はしない)
    fn set_reg32(&mut self, r: usize, v: u32) {
        self.regs[r] = v;
    }

    /// 幅を実行時に選ぶレジスタ読み出し
    fn reg_w(&self, r: usize, wide: bool) -> u32 {
        if wide {
            self.reg32(r)
        } else {
            self.reg16(r) as u32
        }
    }

    /// 幅を実行時に選ぶレジスタ書き込み
    fn set_reg_w(&mut self, r: usize, v: u32, wide: bool) {
        if wide {
            self.set_reg32(r, v)
        } else {
            self.set_reg16(r, v as u16)
        }
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
    /// `0x66` が付いていた。**オペランドの幅が16bitと32bitで入れ替わる**。
    ///
    /// 「32bitモード」ではなく「既定の幅をひっくり返す」プレフィクスである点が要点で、
    /// リアルモード (既定16bit) では 32bit に、プロテクトモードの32bitセグメント
    /// (既定32bit) では逆に 16bit になる。**モードとは独立している**ので、
    /// プロテクトモードを実装しなくてもこれだけ先に動く
    pub opsize32: bool,
    /// `0x67` が付いていた。実効アドレスの計算が16bit形式と32bit形式で入れ替わる
    pub addrsize32: bool,
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

/// 相対分岐の飛び幅を読む。16bitなら符号拡張して32bitへ
fn fetch_rel_w(m: &mut Machine, wide: bool) -> u32 {
    if wide {
        fetch32(m)
    } else {
        fetch16(m) as i16 as i32 as u32
    }
}

pub fn step(m: &mut Machine) {
    let start_ip = m.cpu.ip;
    // 既定の幅は**いま走っているコードセグメントのDビット**が決める。
    // 0x66/0x67 は「反転」なので、32bitセグメントでは逆に16bitへ倒す
    let cs32 = m.cpu.seg_is32(CS);
    let mut d = Decoder {
        seg_override: None,
        rep: None,
        opsize32: cs32,
        addrsize32: cs32,
    };

    // プレフィクスループ
    let op = loop {
        let b = fetch8(m);
        match b {
            0x26 => d.seg_override = Some(ES),
            0x2E => d.seg_override = Some(CS),
            0x36 => d.seg_override = Some(SS),
            0x3E => d.seg_override = Some(DS),
            0xF0 => {}                    // LOCK: シングルコアなので無視
            0x66 => d.opsize32 = !cs32,   // オペランドサイズの**反転** (386〜)
            0x67 => d.addrsize32 = !cs32, // アドレスサイズの反転 (386〜)
            0xF2 | 0xF3 => d.rep = Some(b),
            _ => break b,
        }
    };

    if d.opsize32 {
        m.prefixed_ops.insert(op);
    }

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
                    if kind != 7 {
                        write_op8(m, &rm, r);
                    }
                }
                1 => {
                    // r/m16,r16 または r/m32,r32 (`0x66` が付いていれば後者)
                    let (reg, rm) = modrm(m, &d);
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
                    let (reg, rm) = modrm(m, &d);
                    let a = m.cpu.reg8(reg);
                    let b = read_op8(m, &rm);
                    let r = alu8(&mut m.cpu, kind, a, b);
                    if kind != 7 {
                        m.cpu.set_reg8(reg, r);
                    }
                }
                3 => {
                    let (reg, rm) = modrm(m, &d);
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
            let (kind, rm) = modrm(m, &d);
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
            let (reg, rm) = modrm(m, &d);
            let v = m.cpu.reg8(reg);
            write_op8(m, &rm, v);
        }
        0x89 => {
            let (reg, rm) = modrm(m, &d);
            let w = d.opsize32;
            let v = m.cpu.reg_w(reg, w);
            write_op_w(m, &rm, v, w);
        }
        0x8A => {
            let (reg, rm) = modrm(m, &d);
            let v = read_op8(m, &rm);
            m.cpu.set_reg8(reg, v);
        }
        0x8B => {
            let (reg, rm) = modrm(m, &d);
            let w = d.opsize32;
            let v = read_op_w(m, &rm, w);
            m.cpu.set_reg_w(reg, v, w);
        }
        0x8C => {
            let (reg, rm) = modrm(m, &d);
            let v = m.cpu.sregs[reg & 3];
            write_op16(m, &rm, v);
        }
        0x8E => {
            let (reg, rm) = modrm(m, &d);
            let v = read_op16(m, &rm);
            // 保護モードではGDTから隠しレジスタへ写す。リアルモードなら従来どおり
            load_seg(m, reg & 3, v);
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
            let (_, rm) = modrm(m, &d);
            let v = fetch8(m);
            write_op8(m, &rm, v);
        }
        0xC7 => {
            let (_, rm) = modrm(m, &d);
            let w = d.opsize32;
            let v = fetch_w(m, w);
            write_op_w(m, &rm, v, w);
        }
        0xA0 => {
            let off = fetch16(m);
            let seg = d.seg_override.unwrap_or(DS);
            let v = m.read8(m.cpu.lin(seg, off as u32));
            m.cpu.set_reg8(0, v);
        }
        0xA1 => {
            let off = fetch16(m);
            let seg = d.seg_override.unwrap_or(DS);
            let a = m.cpu.lin(seg, off as u32);
            let w = d.opsize32;
            let v = if w { m.read32(a) } else { m.read16(a) as u32 };
            m.cpu.set_reg_w(AX, v, w);
        }
        0xA2 => {
            let off = fetch16(m);
            let seg = d.seg_override.unwrap_or(DS);
            let v = m.cpu.reg8(0);
            m.write8(m.cpu.lin(seg, off as u32), v);
        }
        0xA3 => {
            let off = fetch16(m);
            let seg = d.seg_override.unwrap_or(DS);
            let a = m.cpu.lin(seg, off as u32);
            let w = d.opsize32;
            let v = m.cpu.reg_w(AX, w);
            if w {
                m.write32(a, v)
            } else {
                m.write16(a, v as u16)
            }
        }

        // --- INC/DEC r16 ---
        0x40..=0x47 => {
            let (r, w) = ((op & 7) as usize, d.opsize32);
            let a = m.cpu.reg_w(r, w);
            let v = if w {
                a.wrapping_add(1)
            } else {
                (a as u16).wrapping_add(1) as u32
            };
            m.cpu.set_reg_w(r, v, w);
            // **CFは触らない** — INC/DECがADD/SUBと違う唯一の点で、
            // 多倍長の加算ループでキャリーを壊さないための配慮である
            m.cpu
                .set_flag(OF, a == if w { 0x7FFF_FFFF } else { 0x7FFF });
            m.cpu.set_flag(AF, a & 0xF == 0xF);
            alu::set_szp_w(&mut m.cpu, v, w);
        }
        0x48..=0x4F => {
            let (r, w) = ((op & 7) as usize, d.opsize32);
            let a = m.cpu.reg_w(r, w);
            let v = if w {
                a.wrapping_sub(1)
            } else {
                (a as u16).wrapping_sub(1) as u32
            };
            m.cpu.set_reg_w(r, v, w);
            m.cpu
                .set_flag(OF, a == if w { 0x8000_0000 } else { 0x8000 });
            m.cpu.set_flag(AF, a & 0xF == 0);
            alu::set_szp_w(&mut m.cpu, v, w);
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
        // ES/CS/SS/DS の番号になっている (0x06,0x0E,0x16,0x1E)
        0x06 | 0x0E | 0x16 | 0x1E => {
            let v = m.cpu.sregs[(op >> 3) as usize & 3];
            push16(m, v);
        }
        // POP CS (0x0F) は8086にしか無く、186以降は2バイト命令の導入符になった。
        // ここでは実装しない (Tier 3 で 0x0F を二バイト空間として使う)
        0x07 | 0x17 | 0x1F => {
            let v = pop16(m);
            m.cpu.sregs[(op >> 3) as usize & 3] = v;
        }

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
        // 3オペランドの乗算。CF/OFは「結果が16bitに収まらなかったか」だけを表し、
        // SF/ZF/AF/PF は未定義
        0x69 | 0x6B => {
            let (reg, rm) = modrm(m, &d);
            let a = read_op16(m, &rm) as i16 as i32;
            let b = if op == 0x69 {
                fetch16(m) as i16 as i32
            } else {
                fetch8(m) as i8 as i32
            };
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
                    let v = m.read16(m.cpu.lin(SS, b as u32));
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
            let seg = d.seg_override.unwrap_or(DS);
            let off = m.cpu.reg16(BX).wrapping_add(m.cpu.reg8(0) as u16);
            let v = m.read8(m.cpu.lin(seg, off as u32));
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
            match (out, wide) {
                (false, false) => {
                    let v = m.io_read8(port);
                    m.cpu.set_reg8(0, v);
                }
                (false, true) => {
                    let v = m.io_read16(port);
                    m.cpu.set_reg16(AX, v);
                }
                (true, false) => {
                    let v = m.cpu.reg8(0);
                    m.io_write8(port, v);
                }
                (true, true) => {
                    let v = m.cpu.reg16(AX);
                    m.io_write16(port, v);
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
            // 積まれて初めて32bitコードが始まる
            load_seg(m, CS, seg);
            m.cpu.set_ip(off);
        }
        0x9A => {
            let off = fetch16(m);
            let seg = fetch16(m);
            let cs = m.cpu.sregs[CS];
            push16(m, cs);
            let ret = m.cpu.ip as u16;
            push16(m, ret);
            m.cpu.sregs[CS] = seg;
            m.cpu.set_ip(off as u32);
        }
        0xCB => {
            let ip = pop16(m) as u32;
            m.cpu.sregs[CS] = pop16(m);
            m.cpu.set_ip(ip);
        }
        0xCA => {
            let n = fetch16(m);
            let ip = pop16(m) as u32;
            m.cpu.sregs[CS] = pop16(m);
            m.cpu.set_ip(ip);
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
            software_int(m, n);
        }
        0xCC => software_int(m, 3), // INT3 (デバッガのブレークポイント)
        0xCE => {
            // INTO: OFが立っているときだけ割り込み4。立っていなければ何もしない
            if m.cpu.flag(OF) {
                interrupt(m, 4);
            }
        }

        // --- フラグ/制御 ---
        0xF4 => m.halted = true,
        0xF5 => {
            let c = m.cpu.flag(CF);
            m.cpu.set_flag(CF, !c);
        } // CMC
        0xF8 => m.cpu.set_flag(CF, false), // CLC
        0xF9 => m.cpu.set_flag(CF, true),  // STC
        0xFA => m.cpu.set_flag(IF, false), // CLI
        0xFB => m.cpu.set_flag(IF, true),  // STI
        0xFC => m.cpu.set_flag(DF, false), // CLD
        0xFD => m.cpu.set_flag(DF, true),  // STD

        // --- TEST / XCHG / LEA ---
        0x84 => {
            let (reg, rm) = modrm(m, &d);
            let a = read_op8(m, &rm);
            let b = m.cpu.reg8(reg);
            alu8(&mut m.cpu, 4, a, b);
        }
        0x85 => {
            let (reg, rm) = modrm(m, &d);
            let w = d.opsize32;
            let a = read_op_w(m, &rm, w);
            let b = m.cpu.reg_w(reg, w);
            alu_w(&mut m.cpu, 4, a, b, w);
        }
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
                // オフセットの幅はオペランドサイズに従う (32bitなら EAX 等へ全桁)
                Operand::Mem { off, .. } => m.cpu.set_reg_w(reg, off, d.opsize32),
                Operand::Reg(_) => panic!("LEA with register operand"),
            }
        }
        0x8F => {
            let (_, rm) = modrm(m, &d);
            let v = pop16(m);
            write_op16(m, &rm, v);
        }
        0x90..=0x97 => {
            let r = (op & 7) as usize;
            let a = m.cpu.reg16(AX);
            let b = m.cpu.reg16(r);
            m.cpu.set_reg16(AX, b);
            m.cpu.set_reg16(r, a);
        }
        0x98 => {
            let v = m.cpu.reg8(0) as i8 as i16 as u16;
            m.cpu.set_reg16(AX, v);
        }
        0x99 => {
            let v = if m.cpu.reg16(AX) & 0x8000 != 0 {
                0xFFFF
            } else {
                0
            };
            m.cpu.set_reg16(DX, v);
        }
        // PUSHF / PUSHFD。**FreeDOSの386判定はここから始まる** (`66 9C`)。
        //
        // 386判定の常套手段は「EFLAGSのbit18 (AC) を立てて書き戻し、
        // 読み直して残っているか見る」というもので、そのためには
        // 32bit幅でフラグを出し入れできる必要がある
        0x9C => {
            let f = m.cpu.flags | 0x0002;
            if d.opsize32 {
                // 上位2bit (VM/RF) はPUSHFDでは常に0で出る
                push_w(m, f & 0x00FC_FFFF, true);
            } else {
                push_w(m, (f as u16 | 0xF002) as u32, false);
            }
        }
        0x9D => {
            let f = pop_w(m, d.opsize32);
            // 書き換えられるビットだけ受け取る。
            //
            // **AC (bit18) は受け付けない。** これは486で入ったフラグで、
            // 「立てて書き戻し、残っていれば486以上」という判定に使われる。
            // 一度これを通してしまったところ、FreeDOSが486と判断して
            // `CMOVcc` (Pentium Pro) を使い始めた。
            //
            // 幅を32bitにできる = 386、ではあるが**486ではない**。
            // 名乗るものを1ビット間違えるだけで、相手は別の道を歩き出す。
            m.cpu.flags = (m.cpu.flags & !0x0FD5) | (f & 0x0FD5) | 0x0002;
        }
        0x9E => {
            // SAHF: AHの下位バイトをフラグへ
            let ah = m.cpu.reg8(4) as u32;
            m.cpu.flags = (m.cpu.flags & !0xD5) | (ah & 0xD5) | 0x0002;
        }
        0x9F => {
            let f = (m.cpu.flags as u8 & 0xD5) | 0x02;
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
                0 | 1 => {
                    let b = fetch8(m);
                    alu8(&mut m.cpu, 4, a, b);
                }
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
                    if a == 0 {
                        return divide_error(m, start_ip);
                    }
                    let q = ax / a as u16;
                    if q > 0xFF {
                        return divide_error(m, start_ip);
                    }
                    m.cpu.set_reg8(0, q as u8);
                    m.cpu.set_reg8(4, (ax % a as u16) as u8);
                }
                _ => {
                    let ax = m.cpu.reg16(AX) as i16;
                    let b = a as i8 as i16;
                    if b == 0 {
                        return divide_error(m, start_ip);
                    }
                    let q = ax / b;
                    if !(-128..=127).contains(&q) {
                        return divide_error(m, start_ip);
                    }
                    m.cpu.set_reg8(0, q as u8);
                    m.cpu.set_reg8(4, (ax % b) as u8);
                }
            }
        }
        0xF7 => {
            let (kind, rm) = modrm(m, &d);
            let a = read_op16(m, &rm);
            match kind {
                0 | 1 => {
                    let b = fetch16(m);
                    alu16(&mut m.cpu, 4, a, b);
                }
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
                    if a == 0 {
                        return divide_error(m, start_ip);
                    }
                    let q = n / a as u32;
                    if q > 0xFFFF {
                        return divide_error(m, start_ip);
                    }
                    m.cpu.set_reg16(AX, q as u16);
                    m.cpu.set_reg16(DX, (n % a as u32) as u16);
                }
                _ => {
                    let n = (((m.cpu.reg16(DX) as u32) << 16) | m.cpu.reg16(AX) as u32) as i32;
                    let b = a as i16 as i32;
                    if b == 0 {
                        return divide_error(m, start_ip);
                    }
                    let q = n / b;
                    if !(-32768..=32767).contains(&q) {
                        return divide_error(m, start_ip);
                    }
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
                2 => {
                    let t = read_op_w(m, &rm, d.opsize32);
                    let ret = m.cpu.ip;
                    push_w(m, ret, d.opsize32);
                    m.cpu.set_ip(t);
                }
                4 => {
                    let t = read_op_w(m, &rm, d.opsize32);
                    m.cpu.set_ip(t);
                }
                6 => {
                    let v = read_op16(m, &rm);
                    push16(m, v);
                }
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
                        let ret = m.cpu.ip as u16;
                        push16(m, ret);
                    }
                    m.cpu.sregs[CS] = seg;
                    m.cpu.set_ip(off as u32);
                }
                _ => panic!(
                    "GRP5 /{kind} not implemented at {:04x}:{:04x}",
                    m.cpu.sregs[CS], start_ip
                ),
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
            if m.cpu.reg16(CX) == 0 {
                let ip = m.cpu.ip.wrapping_add(rel as i32 as u32);
                m.cpu.set_ip(ip);
            }
        }
        0xC2 => {
            let n = fetch16(m);
            let ip = pop_w(m, d.opsize32);
            m.cpu.set_ip(ip);
            let sp = m.cpu.reg16(SP).wrapping_add(n);
            m.cpu.set_reg16(SP, sp);
        }

        // --- x87 (0xD8-0xDF): コプロセッサは載せない ---
        //
        // **何もしないのが正しい。** 8087を挿していない8086では、ESC命令は
        // 実効アドレスを計算してダミーの読み出しをするだけで、メモリも
        // レジスタも書き換えない (本来は隣に座った8087がバスを盗み見る)。
        //
        // FPUの有無を調べるコードは、番兵を置いた場所に `FNSTSW` で書かせて
        // **書き換わらなかったこと**で不在を知る。だからここで気を利かせて
        // 何か書くと、逆に「FPUが在る」と誤認させてしまう。
        //
        // ModRMだけは読む — IPを正しく進めないと次の命令がずれる。
        0xD8..=0xDF => {
            let _ = modrm(m, &d);
        }

        // --- ストリング命令 (REP対応) ---
        0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF => {
            string::exec(m, &d, op)
        }

        // --- 二バイト命令空間 (386〜) ---
        //
        // 8086では `POP CS` だった 0x0F が、186以降で**逃げ道**になった。
        // 1バイトの256席が埋まったので、もう1バイト読んで席を増やす方式である。
        0x0F => {
            let op2 = fetch8(m);
            match op2 {
                // LLDT/LTR系。ModRMのreg欄が「何をするか」を選ぶ
                0x00 => {
                    let (reg, rm) = modrm(m, &d);
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
                    let (reg, rm) = modrm(m, &d);
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
                    if cr != 0 {
                        panic!("unimplemented control register CR{cr}");
                    }
                    if op2 == 0x20 {
                        m.cpu.regs[r] = m.cpu.cr0;
                    } else {
                        let was_pe = m.cpu.pe();
                        m.cpu.cr0 = m.cpu.regs[r];
                        if m.cpu.cr0 & 0x8000_0000 != 0 {
                            panic!("CR0.PG (paging) is not implemented yet");
                        }
                        // PEが立った瞬間、写しを「今のリアルモードの姿」で初期化する。
                        // 実機ではロード時から写しがあるので何も起きない場面だが、
                        // こちらはリアルモードで写しを持たない (遅延) ため、境界で作る
                        if !was_pe && m.cpu.pe() {
                            for i in 0..6 {
                                m.cpu.hidden[i] = SegHidden::real(m.cpu.sregs[i]);
                            }
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
                _ => panic!(
                    "unimplemented opcode 0x0f {op2:#04x} at {:04x}:{:04x}",
                    m.cpu.sregs[CS], start_ip
                ),
            }
        }

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

    // ここからモードで作法が分かれる。
    if m.cpu.pe() {
        interrupt_protected(m, n);
        return;
    }

    // --- リアルモード: IVT (0番地の 4バイト×256) を引く ---
    let f = (m.cpu.flags as u16) | 0xF002;
    push16(m, f);
    // ハンドラ実行中は多重割り込みとシングルステップを止める。
    // 必要ならハンドラ側が STI で開け直す (これが「割り込み禁止区間」の正体)
    m.cpu.set_flag(IF, false);
    m.cpu.set_flag(TF, false);
    let cs = m.cpu.sregs[CS];
    push16(m, cs);
    let ip = m.cpu.ip as u16;
    push16(m, ip);
    // IVTは 0x0000 から 4バイト × 256個。n番目に [オフセット, セグメント] が並ぶ。
    // **OSはここを自分のハンドラで書き換えて割り込みを乗っ取る**
    let vec = n as u32 * 4;
    m.cpu.ip = m.read16(vec) as u32;
    m.cpu.sregs[CS] = m.read16(vec + 2);
}

/// ソフトウェア INT n の入口。**門のDPLがCPLより浅ければ通さない** —
/// リング3が好きなベクタを叩けたら、保護は成立しない。
/// ハードウェア割り込みと例外はこのチェックを受けない (CPU自身が起こすため)
pub(crate) fn software_int(m: &mut Machine, n: u8) {
    if m.cpu.pe() {
        let off = n as u32 * 8;
        if off + 7 <= m.cpu.idtr_limit as u32 {
            let hi = m.read32(m.cpu.idtr_base.wrapping_add(off).wrapping_add(4));
            let gate_dpl = ((hi >> 13) & 3) as u8;
            if gate_dpl < m.cpu.cpl() {
                panic!(
                    "int {n:#04x} from CPL{}: gate DPL={gate_dpl} —                      general protection (まだ#GP配送は無いのでpanic)",
                    m.cpu.cpl()
                );
            }
        }
    }
    interrupt(m, n);
}

/// 保護モードの割り込み配送。IVTではなく**IDTのゲート記述子**を引く。
///
/// ゲートは「どのセグメントの、どこへ、どの作法で」を全部言う8バイト:
///
/// ```text
///   dw offset[15:0]   dw selector   db 0   db type   dw offset[31:16]
/// ```
///
/// type 0xE = 割り込みゲート (IFを落として入る) / 0xF = トラップゲート
/// (IFはそのまま)。この1bitの違いが「割り込みハンドラは再入しない」を
/// ハードウェアで作っている。
///
/// まだやらないこと (リング0だけの世界なので恒真):
/// DPLチェック、スタック切り替え (TSS)、エラーコードのpush
fn interrupt_protected(m: &mut Machine, n: u8) {
    let off = n as u32 * 8;
    if off + 7 > m.cpu.idtr_limit as u32 {
        panic!(
            "vector {n:#04x} is beyond IDT limit {:#06x}",
            m.cpu.idtr_limit
        );
    }
    let a = m.cpu.idtr_base.wrapping_add(off);
    let lo = m.read32(a);
    let hi = m.read32(a.wrapping_add(4));
    let ty = ((hi >> 8) & 0x1F) as u8;
    if hi & 0x8000 == 0 {
        panic!("vector {n:#04x}: gate not present");
    }
    let (sel, dest) = ((lo >> 16) as u16, (lo & 0xFFFF) | (hi & 0xFFFF_0000));
    match ty {
        0x0E | 0x0F => {}
        _ => panic!("vector {n:#04x}: unimplemented gate type {ty:#04x}"),
    }

    // 受け手のコードセグメントのDPLが、いまより深ければ**リングが変わる**
    let old_cpl = m.cpu.cpl();
    let target_dpl = {
        let off = (sel & !0x7) as u32;
        let a = m.cpu.gdtr_base.wrapping_add(off);
        ((m.read32(a.wrapping_add(4)) >> 13) & 3) as u8
    };

    if target_dpl < old_cpl {
        // ---- リング遷移 (3→0など): スタックを差し替えてから積む ----
        //
        // ここが**TSSの存在理由のすべて**である。リング3のスタックを
        // カーネルが信用するわけにはいかない (ユーザーが好きな場所を
        // 指させられる) ので、落ちた瞬間に使うスタックはTSSが決めておく。
        // 元の SS:ESP は新しいスタックに積んで、帰り道 (iretd) が拾う
        let old_ss = m.cpu.sregs[SS] as u32;
        let old_esp = m.cpu.regs[SP];
        // 32bit TSS: +4 = ESP0, +8 = SS0 (リング0に落ちる場合)
        let esp0 = m.read32(m.cpu.tr_base.wrapping_add(4));
        let ss0 = m.read16(m.cpu.tr_base.wrapping_add(8));
        load_seg_raw(m, SS, ss0);
        m.cpu.regs[SP] = esp0;
        push32(m, old_ss);
        push32(m, old_esp);
    }

    // EFLAGS, CS, EIP を32bitで積む (32bitゲート)
    push32(m, m.cpu.flags);
    push32(m, m.cpu.sregs[CS] as u32);
    push32(m, m.cpu.ip);
    if ty == 0x0E {
        // 割り込みゲートだけがIFを落とす
        m.cpu.set_flag(IF, false);
    }
    m.cpu.set_flag(TF, false);
    load_seg_raw(m, CS, sel);
    m.cpu.set_ip(dest);
}

/// 割り込みからの復帰。IP・CS・FLAGS をこの順で取り出す
pub fn iret(m: &mut Machine) {
    // 保護モード (32bitゲートで入った) なら EIP, CS, EFLAGS を32bitで取り出す。
    // 積む側 (interrupt_protected) と対でなければスタックが腐る
    if m.cpu.pe() {
        let ip = pop32(m);
        let sel = pop32(m) as u16;
        let f = pop32(m);
        // 戻り先のRPLがいまのCPLより浅い (数字が大きい) なら**外側リングへの
        // 復帰**で、ESPとSSもスタックから取り出す。積む側 (リング遷移) と対。
        // 「行ったことのない場所へ戻る」— リング3への降下もこの経路を使う
        let to_outer = ((sel & 3) as u8) > m.cpu.cpl();
        load_seg_raw(m, CS, sel);
        m.cpu.set_ip(ip);
        // 復元するフラグの範囲はリアルモードと同じ (AC等の上位はまだ持たない)
        m.cpu.flags = (f & 0x0FD5) | 0x0002;
        if to_outer {
            let esp = pop32(m);
            let ss = pop32(m) as u16;
            load_seg_raw(m, SS, ss);
            m.cpu.regs[SP] = esp;
        }
        return;
    }
    m.cpu.ip = pop16(m) as u32;
    m.cpu.sregs[CS] = pop16(m);
    let f = pop16(m);
    m.cpu.flags = (f as u32 & 0x0FD5) | 0x0002;
}

/// ゼロ除算・商オーバーフローで上がる #DE (INT 0)。
///
/// **フォールトなので、積むのは「失敗した命令の先頭」**である。次の命令ではない。
/// ハンドラが原因を直して `IRET` すれば同じ除算をやり直せる、という設計。
/// (8086は次の命令を積む実装だったが、286以降で今の形に直された)
fn divide_error(m: &mut Machine, start_ip: u32) {
    m.cpu.ip = start_ip;
    if m.first_fault.is_none() {
        m.first_fault = Some((0, m.cpu.sregs[CS], start_ip));
    }
    interrupt(m, 0);
}
