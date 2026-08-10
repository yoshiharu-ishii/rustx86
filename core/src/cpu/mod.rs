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
//!
//! ## ファイル分割は実CPUのデコード階層に沿う
//!
//! `step()` は**1バイトのオペコードマップ**に徹し、Intel SDM 付録Aと同じ階層で
//! 各区画へ渡す。256席が1画面に見えるのが教材の核なので、ここは畳まない:
//!
//! - [`twobyte`] — `0F` の二バイトエスケープ (将来いちばん伸びる区画)
//! - [`group`] — GRP2-5 (1オペコードを ModRM.reg で再分岐する族)
//! - [`segment`] — セグメンテーション (隠しレジスタ、記述子ロード)
//! - [`interrupt`] — 割り込み・例外の配送、リング遷移
//!
//! この階層のおかげで、変更の爆風は区画に収まる — SSEは twobyte、
//! セグメントは segment、というふうに。

pub mod alu;
pub(crate) mod dcache;
pub mod decimal;
pub mod group;
pub mod interrupt;
pub(crate) mod onebyte;
pub mod operand;
pub mod segment;
pub mod shift;
pub mod sse;
pub mod string;
pub mod twobyte;

use alu::{alu8, alu_w, condition};
use operand::{
    fetch16, fetch32, fetch8, fetch_w, modrm, pop16, pop_w, push16, push_w, read_op16, read_op8,
    read_op_w, sp_read, sp_write, write_op16, write_op8, write_op_w, Operand,
};
use shift::shift_rot;

use crate::Machine;
// 制御の流れ (割り込み・iret) は interrupt.rs へ。呼び出し元 (lib.rs) の
// `cpu::interrupt` / `cpu::iret` をそのまま保つため再エクスポートする
pub use interrupt::{interrupt, iret, page_fault};
// セグメンテーションは segment.rs へ。step() と interrupt.rs が使う
pub(crate) use interrupt::{divide_error, software_int};
pub(crate) use segment::{load_seg, load_seg_raw, SegHidden};

/// lib.rs (bzImageロード) から GDT 経由でセグメントを積むための公開口
pub fn load_seg_pub(m: &mut Machine, idx: usize, sel: u16) {
    load_seg(m, idx, sel);
}

/// 最小のx87 — 検出と初期化に答えるだけ。
/// 実装するのは fninit / fnstsw / fnstcw / fldcw。それ以外のESCは黙って流す
/// (演算が要るコードに出会ったら、そのとき trap で知らせる形に格上げする)
fn fpu_min(m: &mut Machine, op: u8, reg: usize, rm: &Operand) {
    match (op, reg, rm) {
        // DB E3: FNINIT — 状態を既定へ
        (0xDB, 4, Operand::Reg(3)) => m.cpu.fpu_cw = 0x037F,
        // D9 /5 m16: FLDCW / D9 /7 m16: FNSTCW
        (0xD9, 5, Operand::Mem { addr, .. }) => m.cpu.fpu_cw = m.read16(*addr),
        (0xD9, 7, Operand::Mem { addr, .. }) => {
            let cw = m.cpu.fpu_cw;
            m.write16(*addr, cw);
        }
        // DD /7 m16: FNSTSW — 例外もスタック使用も無いので常に0
        (0xDD, 7, Operand::Mem { addr, .. }) => m.write16(*addr, 0),
        // DF E0: FNSTSW AX
        (0xDF, 4, Operand::Reg(0)) => m.cpu.set_reg16(AX, 0),
        _ => {}
    }
}

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
pub const FS: usize = 4;
pub const GS: usize = 5;

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

#[derive(Clone)]
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
    /// CR0。bit0 = PE (Protection Enable) / bit31 = PG (Paging)
    pub cr0: u32,
    /// CR2。**フォールトした線形アドレス** (まだ#PF配送はしないので観測用)
    pub cr2: u32,
    /// CR3。ページディレクトリの物理番地 (下位12bitはフラグ)
    pub cr3: u32,
    /// CR4 (486〜)。PSE/PAE等の機能ビット。**保持するだけ** — 名乗っていない
    /// 機能のビットが立ってもうちの動作は変わらない (カーネルはCPUIDで
    /// 名乗った分しか立てない)
    pub cr4: u32,
    /// GDTR (LGDTで積む)。記述子表の場所と大きさ
    pub gdtr_base: u32,
    pub gdtr_limit: u16,
    /// IDTR (LIDTで積む)。保護モードの割り込みはIVTではなくこの表を引く
    pub idtr_base: u32,
    pub idtr_limit: u16,
    /// x87 の制御語 (FLDCW/FNSTCWで読み書き)。
    /// 演算はまだ持たない — カーネルのFPU検出と初期化に答えるための最小構成
    pub fpu_cw: u16,
    /// タイムスタンプカウンタ (RDTSC)。1命令=1カウントで刻む。
    /// 実機のような周波数の意味は無いが、カーネルの較正は
    /// 「PITと突き合わせて比率を測る」だけなので、単調に増えれば成立する
    pub tsc: u64,
    /// デバッグレジスタ DR0-DR7。**保持のみ** — ハードウェアブレークは
    /// 実装しない (カーネルが初期化で触るのに答えるため)。
    /// DR6/DR7 はリセット値に意味がある (それぞれ 0xFFFF0FF0 / 0x400)
    pub dr: [u32; 8],
    /// LDTR (LLDTで積む)。プロセス別セグメント表のセレクタ。
    /// カーネルはブートでヌル(0)を積むだけなので、**保持のみ**で表は引かない
    /// (TIビット付きセレクタが実際に来たら、そのとき実装する)
    pub ldtr_sel: u16,
    /// TR (LTRで積む)。TSSの場所 — リング3→0の瞬間に使うスタックの置き場。
    /// **リングで唯一、本当に新しい部品** (docs/registers.md)
    pub tr_sel: u16,
    pub tr_base: u32,
    pub tr_limit: u32,
    /// XMMレジスタ (SSE/SSE2)。整数演算の128bit器として使われるのが
    /// 現代の主用途 — memcpyもstrlenも、gccはここで束ねて回す
    pub xmm: [u128; 8],
    /// MXCSR (SSEの制御/状態)。丸めモード等。既定 0x1F80
    pub mxcsr: u32,
    /// セグメントの**隠しレジスタ** (実機と同じ構造)。
    ///
    /// 実機の386は、セグメントレジスタをロードした瞬間に記述子の中身
    /// (base/limit/属性) をCPU内に写し取り、以後のアクセスは**この写しだけ**を
    /// 見る。GDTを後から書き換えても、ロードし直すまで反映されない。
    /// リアルモードは「写しに常に sel×16 が入っている」特殊ケースにすぎない —
    /// この統一がプロテクトモードの正体である
    pub hidden: [SegHidden; 6],
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
            cr2: 0,
            cr3: 0,
            cr4: 0,
            fpu_cw: 0x037F, // FNINIT後の既定値
            tsc: 0,
            gdtr_base: 0,
            gdtr_limit: 0,
            idtr_base: 0,
            idtr_limit: 0,
            dr: [0, 0, 0, 0, 0, 0, 0xFFFF_0FF0, 0x400],
            ldtr_sel: 0,
            tr_sel: 0,
            tr_base: 0,
            tr_limit: 0,
            xmm: [0; 8],
            mxcsr: 0x1F80,
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

/// moffs のオフセットを読む。アドレスサイズが幅を決める (符号なし)
fn fetch_addr(m: &mut Machine, wide: bool) -> u32 {
    if wide {
        fetch32(m)
    } else {
        fetch16(m) as u32
    }
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
    let mut saw_66 = false;
    let op = loop {
        let b = fetch8(m);
        match b {
            0x26 => d.seg_override = Some(ES),
            0x2E => d.seg_override = Some(CS),
            0x36 => d.seg_override = Some(SS),
            0x3E => d.seg_override = Some(DS),
            // FS/GS上書き (386〜)。Linuxはper-CPUデータを %fs で引く
            0x64 => d.seg_override = Some(FS),
            0x65 => d.seg_override = Some(GS),
            0xF0 => {} // LOCK: シングルコアなので無視
            0x66 => {
                // オペランドサイズの**反転** (386〜)
                d.opsize32 = !cs32;
                saw_66 = true;
            }
            0x67 => d.addrsize32 = !cs32, // アドレスサイズの反転 (386〜)
            0xF2 | 0xF3 => d.rep = Some(b),
            _ => break b,
        }
    };

    // 0x66 が**実際に付いた**命令を控える (幅対応を忘れた命令は静かに壊れるため)。
    //
    // 以前は `d.opsize32` を条件にしていたが、それは32bitセグメントでは
    // **プレフィクス無しでも常に真**である。つまり32bitコードの全命令が
    // ここで BTreeSet::insert を踏んでいて、プロファイルの27%を占めていた —
    // 診断のつもりの1行が、実行そのものより高くついていた。
    // 記録済みかどうかは配列1発で見る (木を歩かない)
    if saw_66 && !m.prefixed_seen[op as usize] {
        m.prefixed_seen[op as usize] = true;
        m.prefixed_ops.insert(op);
    }

    if cfg!(feature = "opstats") {
        m.op_counts[op as usize] += 1;
    }
    onebyte::exec(m, &d, op, start_ip);
}
