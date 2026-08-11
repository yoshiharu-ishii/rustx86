//! JITビュー (F1a、ADR-0008) — 焼き候補ブロックを外の生成器へ渡す口。
//!
//! coreの内部表現 (Uop) は公開しない。ここにあるのは**coreが「JITで焼いてよい」
//! と認めた命令だけ**の中立なIRで、認めない命令 (メモリに触る形・特権命令…) に
//! 当たったらブロックはそこで終わる。生成器 (wasmシェル/ネイティブランナー) は
//! このIRとフィールド実アドレス表 ([`layout`]) だけを見て動く — coreの無依存と
//! 「意味論の原本はインタプリタ」の規律はこの境界で守られる。
//!
//! F1aの対象はレジスタとフラグしか触らない命令に限る:
//! #PFが起き得ない = 巻き戻しが要らない = 生成コードに脱出点が要らないため、
//! 最初の骨格から失敗系を締め出せる。
//!
//! F1b-1でメモリ**ロード**が加わった。フォールト脱出モデル (ADR-0008):
//! ロードの前にヘルパでtranslateを試し、フォールトしそうなら状態を1つも
//! 変えずにブロックを脱出する — 巻き戻しは相変わらず要らない。

use super::{decode, MemRef, Rm, Uop};
use crate::Machine;

/// メモリオペランドの作り方 (JITの語彙)。coreの [`MemRef`] の公開鏡 —
/// 実効アドレスは生成コードがレジスタの**今の**値から組み、セグメント適用と
/// 変換はヘルパ (Rust) がやる
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitMem {
    /// 基底レジスタ (-1 = 無し)
    pub base: i8,
    /// インデックスレジスタ (-1 = 無し)
    pub index: i8,
    pub scale: u8,
    /// セグメント (デコード時に上書き規則まで解決済み)
    pub seg: u8,
    pub disp: u32,
}

impl From<MemRef> for JitMem {
    fn from(m: MemRef) -> Self {
        JitMem {
            base: m.base,
            index: m.index,
            scale: m.scale,
            seg: m.seg,
            disp: m.disp,
        }
    }
}

/// JITの語彙: レジスタ間 (F1a) + メモリロード (F1b-1) + ストア/RMW (F1b-2)。
///
/// ストアが入っても「ブロック内で世代 (page_gen) が動かない」前提は保たれる —
/// 現行coreで世代を進めるのは REP文字列 (note_write_range) と write_phys8 だけで、
/// 素の線形ストアは進めない (JITヘルパはこれを正確に写す)。REPは語彙に無い。
/// この前提が変わる (素のストアが世代を進めるようになる) ときは、
/// 「ヘルパが自ページの世代を進めたら実行済みn+1で脱出」の受けを足すこと。
///
/// フラグの扱いは lazy flags (cc_*) の材料更新として生成する —
/// C1で作ったcc_op方式がそのままJITのフラグモデルになる。
/// RMWだけは例外で、ccの更新ごとRustヘルパ (alu_w) の中で済む
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitOp {
    /// mov r32, imm32
    MovRI { dst: u8, imm: u32 },
    /// mov r32, r32
    MovRR { dst: u8, src: u8 },
    /// ALU r32, r32 — dst = alu(kind, dst, src)。kind7 (CMP) はdstを書かない
    AluRR { kind: u8, dst: u8, src: u8 },
    /// ALU r32, imm32 (0x83の符号拡張は済んでいる)
    AluRI { kind: u8, dst: u8, imm: u32 },
    /// test r32, r32 (フラグだけ)
    TestRR { a: u8, b: u8 },
    /// inc/dec r32 (CFは不変)
    IncDec { reg: u8, dec: bool },
    /// lea r32, [base + index<<scale + disp] — 計算だけ、メモリに触らない
    Lea {
        dst: u8,
        base: i8,
        index: i8,
        scale: u8,
        disp: u32,
    },
    /// mov r32, [mem] (F1b-1。フォールトしそうなら脱出)
    MovRM { dst: u8, mem: JitMem },
    /// ALU r32, [mem] — dst = alu(kind, dst, mem)。kind7 (CMP) はdstを書かない
    AluRM { kind: u8, dst: u8, mem: JitMem },
    /// cmp [mem], r32 (ALUグリッドのrm=dst形でロードだけで済むkind7)
    CmpMR { mem: JitMem, reg: u8 },
    /// cmp [mem], imm32 (Grp1のkind7 — 同上)
    CmpMI { mem: JitMem, imm: u32 },
    /// test [mem], r32 (フラグだけ)
    TestMR { mem: JitMem, reg: u8 },
    /// mov [mem], r32 (F1b-2。フォールトしそうなら書く前に脱出)
    StoreMR { mem: JitMem, src: u8 },
    /// mov [mem], imm32
    StoreMI { mem: JitMem, imm: u32 },
    /// ALU [mem], r32 — read→alu→write をRustヘルパ1呼びで (kind7は来ない)
    AluMR { kind: u8, mem: JitMem, reg: u8 },
    /// ALU [mem], imm32
    AluMI { kind: u8, mem: JitMem, imm: u32 },
    /// 終端: 条件分岐 (取られたら ip += rel は命令長込みの相対)
    Jcc { cc: u8, rel: u32 },
    /// 終端: 無条件相対ジャンプ
    Jmp { rel: u32 },
}

/// 焼き候補ブロック: 先頭物理アドレスと (命令長, op) の列。
/// 終端は Jcc/Jmp、またはJIT対象外の命令の手前
#[derive(Debug)]
pub struct JitBlock {
    pub head_pa: u32,
    pub ops: Vec<(u8, JitOp)>,
}

/// Uop 1個をJITの語彙へ。(op, これで終端か)。対象外は None
fn convert(u: &Uop) -> Option<(JitOp, bool)> {
    let op = match *u {
        Uop::MovRImm { reg, imm } => JitOp::MovRI { dst: reg, imm },
        Uop::MovRmR {
            rm: Rm::Reg(dst),
            reg,
        } => JitOp::MovRR { dst, src: reg },
        Uop::MovRRm {
            reg,
            rm: Rm::Reg(src),
        } => JitOp::MovRR { dst: reg, src },
        // ---- F1b-1: メモリロード (書かない形だけ。RMW/ストアはF1b-2) ----
        Uop::MovRRm {
            reg,
            rm: Rm::Mem(mr),
        } => JitOp::MovRM {
            dst: reg,
            mem: mr.into(),
        },
        Uop::AluRRm {
            kind,
            reg,
            rm: Rm::Mem(mr),
        } => JitOp::AluRM {
            kind,
            dst: reg,
            mem: mr.into(),
        },
        // rm=dst形: kind7 (CMP) はロードだけ、他はRMW (F1b-2)
        Uop::AluRmR {
            kind: 7,
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::CmpMR {
            mem: mr.into(),
            reg,
        },
        Uop::AluRmR {
            kind,
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::AluMR {
            kind,
            mem: mr.into(),
            reg,
        },
        Uop::Grp1RmImm {
            kind: 7,
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::CmpMI {
            mem: mr.into(),
            imm,
        },
        Uop::Grp1RmImm {
            kind,
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::AluMI {
            kind,
            mem: mr.into(),
            imm,
        },
        Uop::TestRmR {
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::TestMR {
            mem: mr.into(),
            reg,
        },
        // ---- F1b-2: ストア ----
        Uop::MovRmR {
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::StoreMR {
            mem: mr.into(),
            src: reg,
        },
        Uop::MovRmImm {
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::StoreMI {
            mem: mr.into(),
            imm,
        },
        Uop::MovRmImm {
            rm: Rm::Reg(dst),
            imm,
        } => JitOp::MovRI { dst, imm },
        // ALUの向き: RmR は rm=dst / RRm は reg=dst — sub/cmpで順序が効く
        Uop::AluRmR {
            kind,
            rm: Rm::Reg(dst),
            reg,
        } => JitOp::AluRR {
            kind,
            dst,
            src: reg,
        },
        Uop::AluRRm {
            kind,
            reg,
            rm: Rm::Reg(src),
        } => JitOp::AluRR {
            kind,
            dst: reg,
            src,
        },
        Uop::AluAImm { kind, imm } => JitOp::AluRI { kind, dst: 0, imm },
        Uop::Grp1RmImm {
            kind,
            rm: Rm::Reg(dst),
            imm,
        } => JitOp::AluRI { kind, dst, imm },
        Uop::TestRmR {
            rm: Rm::Reg(a),
            reg,
        } => JitOp::TestRR { a, b: reg },
        Uop::IncR { reg } => JitOp::IncDec { reg, dec: false },
        Uop::DecR { reg } => JitOp::IncDec { reg, dec: true },
        Uop::Lea { reg, mem } => JitOp::Lea {
            dst: reg,
            base: mem.base,
            index: mem.index,
            scale: mem.scale,
            disp: mem.disp,
        },
        Uop::Jcc { cc, rel } => return Some((JitOp::Jcc { cc, rel }, true)),
        Uop::JmpRel { rel } => return Some((JitOp::Jmp { rel }, true)),
        _ => return None, // メモリ形・スタック・その他はF1a対象外
    };
    Some((op, false))
}

/// `pa` から直線にデコードして、JITで焼ける範囲を切り出す。
/// 対象外の命令・ページ末・`cap` で打ち切り。1命令も取れなければ None。
/// **デコードするだけ** — 機械の状態は変えない (&Machine)
pub fn collect_block(m: &Machine, pa: u32, cap: usize) -> Option<JitBlock> {
    let mut ops = Vec::new();
    let mut p = pa;
    while ops.len() < cap {
        let Some((len, uop)) = decode::decode_at(m, p) else {
            break; // デコード不能の手前まで
        };
        let Some((op, term)) = convert(&uop) else {
            break; // JIT対象外の手前まで
        };
        ops.push((len, op));
        p = p.wrapping_add(len as u32);
        if term || p & 0xFFF == 0 {
            break; // 分岐で終端 / ページ末 (跨ぎ命令はdecode_atが拒否済み)
        }
    }
    if ops.is_empty() {
        None
    } else {
        Some(JitBlock { head_pa: pa, ops })
    }
}

/// 生成コードが直接読み書きするフィールドの**実アドレス** (wasmならリニアメモリ
/// のオフセット)。生成時に定数として焼き込む。
///
/// 有効なのは Machine が動かない間だけ — wasm側の Emulator は wasm-bindgen が
/// Box化して持つので、生成後にアドレスが変わることはない (再ロード時は
/// フィールドへの上書き代入なので番地は不変)。ネイティブで使うならBox/Pinが前提
#[derive(Debug, Clone, Copy)]
pub struct JitLayout {
    /// regs[0..8] の先頭 (u32×8、連続)
    pub regs: usize,
    pub ip: usize,
    pub flags: usize,
    pub cc_op: usize,
    pub cc_w: usize,
    pub cc_a: usize,
    pub cc_b: usize,
    pub cc_cin: usize,
    pub cc_r: usize,
    pub tsc: usize,
    pub tick_countdown: usize,
}

/// 焼けたブロックの実行フック (F1a)。
///
/// coreは生成器もランタイムも知らない — 知っているのは「ブロック頭で焼けた
/// ブロックがあれば `enter(slot)` を呼べばn命令ぶん進む」という契約だけ。
/// dynではなくfnポインタなのは coreの無依存を保つため。
///
/// ブロックの有無・世代照合 (現世代か)・命令数は **Entry側**が持つ — 実行時に
/// 毎ブロック頭のhashmap照合を払わないため。フックは「スロットを呼ぶ」だけ。
/// 呼ばれるのはブロック頭 (分岐の着地直後か step_cached 入口) で、`m.cpu.ip` は
/// 先頭命令を指す。tsc/tick_countdown/extra の清算はcore側が n命令まとめて
/// 毎命令の意味順序どおりに再現する (位置がずれると命令数の決定性が壊れる)。
#[derive(Clone, Copy)]
pub struct JitHook {
    /// スロット番号を受け取り、そのJITブロックを実行して実行命令数を返す。
    /// **ブロックの全命令数より少ない返り値 = フォールト脱出** (F1b) —
    /// 返した数までは完全に実行済み、次の命令は状態を1つも変えていない。
    /// core側はその1命令をインタプリタでやり直す (skip_jit)
    pub enter: fn(slot: u32) -> u32,
}

/// ブロックのページ世代 (焼いた時点の値を控えて、実行前に照合する)。
/// ブロック**内**で世代が動くことはない — 現行coreで世代を進めるのは
/// REP文字列と write_phys8 だけで、どちらも語彙に無い (素のストアは進めない。
/// JitOpのdocを参照)。よって頭での照合が、インタプリタの毎命令照合と同じ強さになる
pub fn page_gen(m: &Machine, pa: u32) -> u32 {
    m.dcache.page_gen_of(pa)
}

pub fn layout(m: &Machine) -> JitLayout {
    // jit.rs は cpu モジュールの子孫なので、Cpuのprivateフィールドに触れる
    // (alu.rs が cc_* に触れるのと同じ理屈)。アドレスはここで写し取り、
    // 生成器には数値としてだけ渡す
    JitLayout {
        regs: m.cpu.regs.as_ptr() as usize,
        ip: &m.cpu.ip as *const u32 as usize,
        flags: &m.cpu.flags as *const u32 as usize,
        cc_op: &m.cpu.cc_op as *const u8 as usize,
        cc_w: &m.cpu.cc_w as *const u8 as usize,
        cc_a: &m.cpu.cc_a as *const u32 as usize,
        cc_b: &m.cpu.cc_b as *const u32 as usize,
        cc_cin: &m.cpu.cc_cin as *const u32 as usize,
        cc_r: &m.cpu.cc_r as *const u32 as usize,
        tsc: &m.cpu.tsc as *const u64 as usize,
        tick_countdown: &m.tick_countdown as *const u32 as usize,
    }
}
