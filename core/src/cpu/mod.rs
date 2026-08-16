//! x86 CPU の骨格 — 8086から386 (プロテクトモード・ページング) まで。
//!
//! 名前が「リアルモードCPU」だったのは Tier 1 の頃の話で、いまは同じ機械が
//! 32bit Linux を起動する。**モードでファイルを分けない**のは設計判断である:
//! リアルモードは「隠しレジスタに sel×16 が写っている特殊ケース」にすぎず
//! (ADR-0006)、ほとんどの命令はモードを知らずに動く。モードで割ると
//! 同じ命令が二重になる。
//!
//! このファイルは**機械の骨格**に徹する: Cpu (レジスタと隠しレジスタ)、
//! Decoder (プレフィクスの解決)、step (1命令の流れ)。仕事は区画が持つ:
//!
//! ## ファイル分割は実CPUのデコード階層と関心に沿う
//!
//! - [`onebyte`] — 1バイト空間の振り分け表 (**意味の原本**。Intel SDM 付録Aの階層)
//! - [`twobyte`] — `0F` の二バイトエスケープ (いちばん伸びる区画)
//! - [`group`] — GRP2-5 (1オペコードを ModRM.reg で再分岐する族)
//! - [`operand`] — ModRM解決、オペランド読み書き、アドレス変換、スタック
//! - [`alu`] — 8種の演算とフラグ計算 (AF/OFの意味論)
//! - [`shift`] — シフトと回転 / [`string`] — ストリング命令とREP /
//!   [`decimal`] — 十進補正 (BCD) / [`sse`] — SSEの整数用途
//! - [`segment`] — セグメンテーション (隠しレジスタ、記述子ロード)
//! - [`interrupt`] — 割り込み・例外の配送、リング遷移
//! - [`dcache`] — デコード済み命令キャッシュ (**速い写し**。最適化は全部ここ)
//!
//! この階層のおかげで、変更の爆風は区画に収まる — SSEは twobyte、
//! セグメントは segment、速さは dcache、というふうに。

pub mod alu;
pub(crate) mod dcache;
pub mod decimal;
pub mod group;
pub mod interrupt;
pub mod mmx;
pub(crate) mod onebyte;
pub mod operand;
pub mod segment;
pub mod shift;
pub mod sse;
pub mod string;
pub mod twobyte;

use alu::{alu8, alu_w, condition};
use operand::{
    fetch16, fetch32, fetch8, fetch_w, modrm, pop_w, push_w, read_op16, read_op8, read_op_w,
    sp_read, sp_write, write_op16, write_op8, write_op_w, Operand,
};
use shift::shift_rot;

use crate::Machine;
// 制御の流れ (割り込み・iret) は interrupt.rs へ。呼び出し元 (lib.rs) の
// `cpu::interrupt` / `cpu::iret` をそのまま保つため再エクスポートする
pub use interrupt::{interrupt, iret, page_fault};
// セグメンテーションは segment.rs へ。step() と interrupt.rs が使う
pub(crate) use interrupt::{divide_error, gp_fault, seg_fault_err, software_int};
pub(crate) use segment::{load_seg, load_seg_raw, SegHidden};

/// lib.rs (bzImageロード) から GDT 経由でセグメントを積むための公開口
pub fn load_seg_pub(m: &mut Machine, idx: usize, sel: u16) {
    let _ = load_seg(m, idx, sel);
}

pub mod fpu;

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
/// 仮想8086モード (EFLAGS bit 17)。**モードがフラグに入っている**唯一の例 —
/// 保護モードのまま「リアルモードのふりをした檻」を作る。立てられるのは
/// iret/タスクスイッチだけで、POPFでは書き換わらない
pub const VM: u32 = 1 << 17;

/// ALUが書く6フラグ (CF PF AF ZF SF OF) — 遅延評価の対象。
/// IF/DF/TF等の**制御フラグは対象外** (ALUは触らないので flags フィールドが常に真実)
const CC_MASK: u32 = CF | PF | AF | ZF | SF | OF;
/// cc_op の「遅延なし」— flags フィールドが6フラグ含めて真実
const CC_NONE: u8 = 0xFF;
/// cc_op のINC/DEC (CFだけは flags フィールド側に保存されている)
const CC_INC: u8 = 8;
const CC_DEC: u8 = 9;

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
    /// EFLAGS — ただし**ALUの6フラグ (CC_MASK) は遅延評価**で、cc_op が
    /// CC_NONE でない間は cc_* の材料が真実である。外から読むときは
    /// [`eflags`](Self::eflags)、書くときは [`set_eflags`](Self::set_eflags) を
    /// 通すこと (privateなのはこの規律をコンパイラに守らせるため)。
    ///
    /// なぜ遅延か: フラグは**書かれる回数 >> 読まれる回数**。ADD/SUB/CMPの
    /// たびに6フラグを合成しても、大半は次のALU命令に上書きされて誰にも
    /// 読まれない。そこで演算の材料 (op, a, b, r) だけ控えて、読まれた瞬間に
    /// 必要な1ビットだけ計算する (QEMUの cc_op/cc_src/cc_dst と同じ考え方)
    flags: u32,
    /// 遅延フラグの材料: 最後にフラグを書いたALU演算の種別。
    /// 0..=7 = alu.rs の op (7=CMPはSUBと同じ)、CC_INC/CC_DEC、CC_NONE
    cc_op: u8,
    /// 幅: 0=8bit / 1=16bit / 2=32bit
    cc_w: u8,
    /// 材料: オペランドa・b (幅でマスク済み)、キャリー入力 (ADC/SBB)、結果
    cc_a: u32,
    cc_b: u32,
    cc_cin: u32,
    cc_r: u32,
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
    /// SSE側 (FXSAVE) も触るので、[`fpu::Fpu`] の外に居る
    pub fpu_cw: u16,
    /// x87 のレジスタスタックと状態 (f64裏打ちの実装は [`fpu`])
    pub fpu: fpu::Fpu,
    /// タイムスタンプカウンタ (RDTSC)。1命令=1カウントで刻む。
    /// 実機のような周波数の意味は無いが、カーネルの較正は
    /// 「PITと突き合わせて比率を測る」だけなので、単調に増えれば成立する
    pub tsc: u64,
    /// デバッグレジスタ DR0-DR7。**保持のみ** — ハードウェアブレークは
    /// 実装しない (カーネルが初期化で触るのに答えるため)。
    /// DR6/DR7 はリセット値に意味がある (それぞれ 0xFFFF0FF0 / 0x400)
    pub dr: [u32; 8],
    /// LDTR (LLDTで積む)。プロセス別セグメント表の所在。
    /// TIビット (bit2) の立ったセレクタはGDTでなくこの表を引く。
    /// base/limit はLLDT時にGDTの記述子から写す (セグメントの隠しレジスタと同型)
    pub ldtr_sel: u16,
    pub ldtr_base: u32,
    pub ldtr_limit: u32,
    /// TR (LTRで積む)。TSSの場所 — リング3→0の瞬間に使うスタックの置き場。
    /// **リングで唯一、本当に新しい部品** (docs/reference/registers.md)
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
            cc_op: CC_NONE,
            cc_w: 0,
            cc_a: 0,
            cc_b: 0,
            cc_cin: 0,
            cc_r: 0,
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            fpu_cw: 0x037F, // FNINIT後の既定値
            fpu: fpu::Fpu::default(),
            tsc: 0,
            gdtr_base: 0,
            gdtr_limit: 0,
            idtr_base: 0,
            idtr_limit: 0,
            dr: [0, 0, 0, 0, 0, 0, 0xFFFF_0FF0, 0x400],
            ldtr_sel: 0,
            ldtr_base: 0,
            ldtr_limit: 0,
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

    /// ページング有効か (CR0.PG)
    pub fn pg(&self) -> bool {
        self.cr0 & (1 << 31) != 0
    }

    /// 仮想8086モードか (PE=1 かつ EFLAGS.VM)。
    /// VMは遅延6フラグの外なので flags を直に読める (IOPLと同じ)
    pub fn vm86(&self) -> bool {
        self.pe() && self.flags & VM != 0
    }

    /// CPL (現在特権レベル)。独立したレジスタではない —
    /// **いま走っているCSセレクタの下位2bit**がそのまま現在特権である。
    /// リアルモードは常に0 (全能)。V86はCSがリアル風の値 (下位2bitは無意味)
    /// なので**常に3** — 檻の中は最弱権限で走る
    pub fn cpl(&self) -> u8 {
        if self.vm86() {
            3
        } else if self.pe() {
            (self.sregs[CS] & 3) as u8
        } else {
            0
        }
    }

    /// IOPL (EFLAGS bit 12-13)。CLI/STI/IN/OUT が「どのリングまで許すか」の閾値。
    /// IOPLは遅延フラグの外なので flags を直に読める
    pub fn iopl(&self) -> u8 {
        ((self.flags >> 12) & 3) as u8
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

    /// 32bit CS確定経路 (step_cached) 専用のIP更新。ip_mask()のセグメント幅
    /// 判定 (loadと分岐) を払わない — CSの幅を変える命令は語彙外なので、
    /// 入口の seg_is32 検査がブロック全体に効く
    #[inline(always)]
    pub(crate) fn advance_ip32(&mut self, n: u32) {
        self.ip = self.ip.wrapping_add(n);
    }

    /// set_ip の32bit CS確定版 (advance_ip32と同じ論拠 — C12)。
    /// 制御uop (全命令の15〜20%) は ip → 次の照合番地 の鎖上に居るので、
    /// ip_mask() の cr0/CSロード+分岐をここから剥がす
    #[inline(always)]
    pub(crate) fn set_ip32(&mut self, v: u32) {
        self.ip = v;
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

    // ---------- 遅延フラグ (lazy flags) ----------
    //
    // 読み書きの規律: 6フラグ (CC_MASK) は cc_op が立っている間 cc_* から
    // 計算する。それ以外 (IF/DF/TF...) は常に flags フィールドが真実。
    // set_flag で6フラグのどれかを**部分的に**書く命令 (シフト・MUL・BT等) は
    // 先に materialize してから書く — 意味論は従来と1bitも変わらない。

    /// ALU演算の材料を控える (フラグはまだ計算しない)。alu.rs だけが呼ぶ
    #[inline]
    pub(super) fn set_cc(&mut self, op: u8, w: u8, a: u32, b: u32, cin: u32, r: u32) {
        self.cc_op = op;
        self.cc_w = w;
        self.cc_a = a;
        self.cc_b = b;
        self.cc_cin = cin;
        self.cc_r = r;
    }

    /// INC/DEC用: CFだけは前の値を引き継ぐので、遅延状態を上書きする**前に**
    /// CFを計算して flags のビットへ退避する
    #[inline]
    pub(super) fn set_cc_incdec(&mut self, op: u8, w: u8, a: u32, r: u32) {
        let cf = self.flag(CF);
        self.set_cc(op, w, a, 1, 0, r);
        if cf {
            self.flags |= CF;
        } else {
            self.flags &= !CF;
        }
    }

    /// 幅の符号ビット (0x80 / 0x8000 / 0x8000_0000)
    #[inline]
    fn cc_sign(&self) -> u32 {
        1u32 << ((8 << self.cc_w) - 1)
    }

    #[inline]
    fn cc_cf(&self) -> bool {
        let (a, b, cin) = (self.cc_a as u64, self.cc_b as u64, self.cc_cin as u64);
        match self.cc_op {
            0 | 2 => a + b + cin > (self.cc_sign() as u64 * 2 - 1), // ADD/ADC: 幅を溢れた
            3 | 5 | 7 => a < b + cin,                               // SBB/SUB/CMP: 借りた
            CC_INC | CC_DEC => self.flags & CF != 0,                // 不変 (flagsへ退避済み)
            _ => false,                                             // 論理演算はCF=0
        }
    }

    #[inline]
    fn cc_of(&self) -> bool {
        let (a, b, r, s) = (self.cc_a, self.cc_b, self.cc_r, self.cc_sign());
        match self.cc_op {
            0 | 2 => (a ^ !b) & (a ^ r) & s != 0, // 同符号を足して符号が変わった
            3 | 5 | 7 => (a ^ b) & (a ^ r) & s != 0,
            CC_INC => a == s - 1, // 0x7F.. → 0x80..
            CC_DEC => a == s,     // 0x80.. → 0x7F..
            _ => false,
        }
    }

    #[inline]
    fn cc_af(&self) -> bool {
        let (a, b, cin) = (self.cc_a, self.cc_b, self.cc_cin);
        match self.cc_op {
            0 | 2 => (a & 0xF) + (b & 0xF) + cin > 0xF,
            3 | 5 | 7 => (a & 0xF) < (b & 0xF) + cin,
            CC_INC => a & 0xF == 0xF,
            CC_DEC => a & 0xF == 0,
            _ => false,
        }
    }

    /// 遅延中の6フラグをまとめて計算する (materialize と eflags の共通部)
    fn cc_compute(&self) -> u32 {
        let mut f = 0;
        if self.cc_cf() {
            f |= CF;
        }
        if self.cc_of() {
            f |= OF;
        }
        if self.cc_af() {
            f |= AF;
        }
        if self.cc_r == 0 {
            f |= ZF;
        }
        if self.cc_r & self.cc_sign() != 0 {
            f |= SF;
        }
        if (self.cc_r as u8).count_ones().is_multiple_of(2) {
            f |= PF;
        }
        f
    }

    /// EFLAGS全体の**具現化された値** (純粋 — 状態は変えない)。
    /// PUSHF・割り込み配送・スナップショット・cosim照合はここを通る
    pub fn eflags(&self) -> u32 {
        if self.cc_op == CC_NONE {
            self.flags
        } else {
            (self.flags & !CC_MASK) | self.cc_compute()
        }
    }

    /// EFLAGS全体を書く (POPF/IRET/スナップショット復元)。遅延状態は捨てる
    pub fn set_eflags(&mut self, v: u32) {
        self.flags = v;
        self.cc_op = CC_NONE;
    }

    /// 遅延分を flags フィールドへ畳み込む。以後 flags が真実に戻る
    fn materialize(&mut self) {
        self.flags = self.eflags();
        self.cc_op = CC_NONE;
    }

    /// #PF巻き戻し用の**薄い控え** ([`crate::Machine::guard_save`] の速い相棒)。
    ///
    /// キャッシュ済みuop (dcache) が書き得るのは 汎用レジスタ・IP・フラグ
    /// (遅延材料含む) だけ — sregs/hidden/CR/xmm/dr/gdtr はuopの語彙に無い。
    /// Cpu丸ごと (xmm 128B + hidden 72B + …) を毎回複写するのをやめ、
    /// 書き得る ~76B だけ控える。フォールバック経路は何でも起こせるので
    /// 従来どおり丸ごとcloneを使う (控えの種類は Machine 側が覚える)
    /// save_slim の「ipだけ指定値」版 (translate-first F1c-d5)。
    /// exec内 (advance_ip後) から控えるとき、巻き戻し先は**命令頭**でなければ
    /// ならない — #PF後の再実行が次の命令へ飛ぶバグの教訓 (2026-08-13)
    pub(crate) fn save_slim_at(&self, s: &mut SlimSave, ip: u32) {
        self.save_slim(s);
        s.ip = ip;
    }

    pub(crate) fn save_slim(&self, s: &mut SlimSave) {
        s.regs = self.regs;
        s.ip = self.ip;
        s.flags = self.flags;
        s.cc_op = self.cc_op;
        s.cc_w = self.cc_w;
        s.cc_a = self.cc_a;
        s.cc_b = self.cc_b;
        s.cc_cin = self.cc_cin;
        s.cc_r = self.cc_r;
    }

    pub(crate) fn restore_slim(&mut self, s: &SlimSave) {
        self.regs = s.regs;
        self.ip = s.ip;
        self.flags = s.flags;
        self.cc_op = s.cc_op;
        self.cc_w = s.cc_w;
        self.cc_a = s.cc_a;
        self.cc_b = s.cc_b;
        self.cc_cin = s.cc_cin;
        self.cc_r = s.cc_r;
    }

    pub fn flag(&self, mask: u32) -> bool {
        // 遅延中でも、対象外のフラグ (IF/DF等) は flags を直接見る。
        // このifは分岐予測が当たり続けるので、eager時代のコストとほぼ同じ
        if self.cc_op == CC_NONE || mask & CC_MASK == 0 {
            return self.flags & mask != 0;
        }
        match mask {
            CF => self.cc_cf(),
            ZF => self.cc_r == 0,
            SF => self.cc_r & self.cc_sign() != 0,
            OF => self.cc_of(),
            PF => (self.cc_r as u8).count_ones().is_multiple_of(2),
            AF => self.cc_af(),
            _ => self.eflags() & mask != 0, // 複数ビットまとめての問い合わせ
        }
    }

    /// BIOS HLE が「成功/失敗」を返すのに使う。x86のBIOSは慣例として
    /// キャリーフラグで成否を返す
    pub fn set_flag_cf(&mut self, on: bool) {
        self.set_flag(CF, on);
    }

    pub fn set_flag(&mut self, mask: u32, on: bool) {
        // 6フラグの一部だけ書く命令 (シフト・MUL・BT・SETcc後の补正等) は、
        // 書かない残りが遅延材料に残っていると食い違う — 先に具現化する
        if self.cc_op != CC_NONE && mask & CC_MASK != 0 {
            self.materialize();
        }
        if on {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }
}
/// [`Cpu::save_slim`] の器。中身はキャッシュ済みuopが書き得る部分だけ
#[derive(Default)]
pub(crate) struct SlimSave {
    regs: [u32; 8],
    ip: u32,
    flags: u32,
    cc_op: u8,
    cc_w: u8,
    cc_a: u32,
    cc_b: u32,
    cc_cin: u32,
    cc_r: u32,
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
    /// `0x66` が**実際に付いていた** (opsize32は既定幅との合成なので別物)。
    /// SSE/MMXの命令選択子は生のプレフィクスで決まる — 16bitコードでも
    /// `0F 6F` はMMX、`66 0F 6F` はSSE2
    pub p66: bool,
    /// `0xF0` (LOCK) が付いていた。バス占有はシングルコアでは意味を持たないが、
    /// **付けてよい命令かの検査**には要る (読んで書く命令以外は #UD)
    pub lock: bool,
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

/// LOCKプレフィクスを許す1バイトオペコード (読んで書くALU/ビット系)。
/// r/m,r形式のADD/OR/ADC/SBB/AND/SUB/XOR/CMPなし、GRP1、XCHG、GRP3-5、0F空間
fn lockable(op: u8) -> bool {
    matches!(
        op,
        0x00 | 0x01
            | 0x08
            | 0x09
            | 0x10
            | 0x11
            | 0x18
            | 0x19
            | 0x20
            | 0x21
            | 0x28
            | 0x29
            | 0x30
            | 0x31
            | 0x80..=0x83 | 0x86 | 0x87 | 0xF6 | 0xF7 | 0xFE | 0xFF | 0x0F
    )
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
        p66: false,
        lock: false,
    };

    // プレフィクスループ
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
            0xF0 => d.lock = true, // LOCK: 実行は無視、可否の検査だけ受ける
            0x66 => {
                // オペランドサイズの**反転** (386〜)
                d.opsize32 = !cs32;
                d.p66 = true;
            }
            0x67 => d.addrsize32 = !cs32, // アドレスサイズの反転 (386〜)
            0xF2 | 0xF3 => d.rep = Some(b),
            _ => break b,
        }
    };

    // LOCKは「読んで書く」命令にしか付けられない — それ以外は #UD (test386
    // POST 12)。判定はオペコード段の白リスト (0F空間とグループ細分は緩め —
    // 誤って#UDにしない側に倒す。実CPUより通しすぎる分は台帳の未了)
    if d.lock && !lockable(op) {
        m.cpu.set_ip(start_ip);
        interrupt(m, 6);
        return;
    }

    // 0x66 が**実際に付いた**命令を控える (幅対応を忘れた命令は静かに壊れるため)。
    //
    // 以前は `d.opsize32` を条件にしていたが、それは32bitセグメントでは
    // **プレフィクス無しでも常に真**である。つまり32bitコードの全命令が
    // ここで BTreeSet::insert を踏んでいて、プロファイルの27%を占めていた —
    // 診断のつもりの1行が、実行そのものより高くついていた。
    // 記録済みかどうかは配列1発で見る (木を歩かない)
    if d.p66 && !m.prefixed_seen[op as usize] {
        m.prefixed_seen[op as usize] = true;
        m.prefixed_ops.insert(op);
    }

    if cfg!(feature = "opstats") {
        m.op_counts[op as usize] += 1;
    }
    onebyte::exec(m, &d, op, start_ip);
}
