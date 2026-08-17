//! F1d-a: AArch64テンプレートJIT (ADR-0022) — JitOp列を手書きエンコードで焼く。
//!
//! F1c (Cranelift) と同じ意味論契約を、リンク税のないバックエンドで写す:
//! - フラグは lazy flags の材料 (cc_*) をメモリへ書くだけ (評価器を二重実装
//!   しない — CF入力と条件判定はヘルパ呼び)
//! - ipの更新はブロック出口で1回。tsc/tick/extra の清算は core 側
//! - F1d-aの語彙は**レジスタ形のみ** (メモリ・スタック形はブロックをそこで
//!   打ち切る)。#PFが起き得ない = 脱出点が要らない = 全量実行のみ
//!
//! ## 生成コードとMachineの別名参照について (F1cから引き継ぐ規律)
//!
//! 生成コードはMachineのフィールド実番地 (jit::layout) を直接読み書きし、
//! ヘルパは生ポインタから参照を作る。呼び出し元 (step_cached) の &mut Machine
//! と重なるが、入口は**必ず不透明な関数ポインタ (JitHook.try_enter)** なので
//! コンパイラは呼び出しをまたいだ別名最適化をできない。この前提が壊れる
//! 書き方 (enterのインライン化・LTOでの貫通) をしないこと。
//!
//! ## レジスタの割り当て (生成コード内)
//!
//! ホストレジスタは呼び出し側保存 (x0-x15) だけを使い、**opをまたいで値を
//! 生かさない** (毎opロード/ストア — 骨格の単純さ優先。M1のストアバッファと
//! フォワーディングが充分隠すことはインタプリタの実測で既知)。
//! x9 = 定数番地の器、x10-x12 = 演算の器、x0-x1 = ヘルパ引数。

#![allow(clippy::fn_to_numeric_cast, clippy::fn_to_numeric_cast_any)]
#![allow(unknown_lints, function_casts_as_integer)]
// dynasm!の動的レジスタ指定子 X(r)/W(r) はマクロ内部で .into() を挟む —
// u8を渡すと「同型への変換」としてclippyが鳴るが、マクロの都合なので黙らせる
#![allow(clippy::useless_conversion)]

use dynasmrt::{dynasm, DynasmApi, DynasmLabelApi};
use rustx86_core::jit::{self, JitHook, JitLayout, JitOp};
use rustx86_core::{cpu, Machine};

// ---- ヘルパ (生成コードから呼ばれる) ----

/// CFの現在値 (遅延中なら1bitだけ計算)。ADC/SBB/INC/DECのCF入力・退避用
unsafe extern "C" fn h_cf(m: *const Machine) -> u32 {
    (*m).cpu.flag(cpu::CF) as u32
}

/// jcc/setccの条件判定 (意味論の原本 = alu::condition)
unsafe extern "C" fn h_cond(m: *const Machine, cc: u32) -> u32 {
    cpu::alu::condition(&(*m).cpu, cc as u8) as u32
}

/// shift/rot r32 — 意味論の原本 shift_rot をそのまま呼ぶ (eagerフラグ込み)
unsafe extern "C" fn h_shift(m: *mut Machine, kind: u32, reg: u32, count: u32) -> u32 {
    let c = &mut (*m).cpu;
    let a = c.regs[reg as usize];
    let v = cpu::shift::shift_rot(c, kind as u8, a, count as u8, 32);
    c.regs[reg as usize] = v;
    0
}

/// メモリロード (成功 = 値そのまま / 脱出 = bit32を立てる)。
/// セグメント適用は lin() — f1c-finalの h_ld* と同じ契約 (意味論の原本は
/// jit_try_read* = read系の速い道と同じ部品)
unsafe extern "C" fn h_ld32(m: *const Machine, seg: u32, off: u32) -> u64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off);
    match m.jit_try_read32(la) {
        Some(v) => v as u64,
        None => 1u64 << 32,
    }
}

unsafe extern "C" fn h_ld16(m: *const Machine, seg: u32, off: u32) -> u64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off);
    match m.jit_try_read16(la) {
        Some(v) => v as u64,
        None => 1u64 << 32,
    }
}

unsafe extern "C" fn h_ld8(m: *const Machine, seg: u32, off: u32) -> u64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off);
    match m.jit_try_read8(la) {
        Some(v) => v as u64,
        None => 1u64 << 32,
    }
}

/// 32bitストア (意味論はcoreのfast_write32へ委譲 — note_write/VRAM脱出込み)。
/// 1=完了 / 0=脱出 (何も書いてない)
unsafe extern "C" fn h_st32(m: *mut Machine, seg: u32, off: u32, val: u32) -> u32 {
    (*m).jit_try_write32(seg as usize, off, val) as u32
}

/// RMW `alu [mem], b` (read→alu→write+note_writeをヘルパ1呼びで)
unsafe extern "C" fn h_rmw32(m: *mut Machine, seg: u32, off: u32, kind: u32, b: u32) -> u32 {
    (*m).jit_try_rmw32(seg as usize, off, kind as u8, b) as u32
}

/// 8bitストア (意味論はfast_write8へ委譲)。1=完了 / 0=脱出
unsafe extern "C" fn h_st8(m: *mut Machine, seg: u32, off: u32, val: u32) -> u32 {
    (*m).jit_try_write8(seg as usize, off, val as u8) as u32
}

/// 8bit RMW `alu [m8], b` (read→alu8→write+note_writeをヘルパ1呼びで)
unsafe extern "C" fn h_rmw8(m: *mut Machine, seg: u32, off: u32, kind: u32, b: u32) -> u32 {
    (*m).jit_try_rmw8(seg as usize, off, kind as u8, b as u8) as u32
}

/// F6 kind0-3 レジスタ形 (test/not/neg) — 実行体はcore側 (grp3b8_reg)。
/// NEGのCF上書きが遅延材料に畳めないのでヘルパ1呼び (#PF不能・脱出不要)
unsafe extern "C" fn h_grp3b8(m: *mut Machine, kind: u32, reg8: u32, imm: u32) -> u32 {
    jit::grp3b8_reg(&mut *m, kind as u8, reg8 as u8, imm as u8);
    0
}

/// push (SP更新は書き込み確定後 — 意味論はexec.rsのfast_push32と同一実体)
unsafe extern "C" fn h_push32(m: *mut Machine, val: u32) -> u32 {
    (*m).jit_try_push32(val) as u32
}

/// pop (成功 = 値 / 脱出 = bit32)
unsafe extern "C" fn h_pop32(m: *mut Machine) -> u64 {
    match (*m).jit_try_pop32() {
        Some(v) => v as u64,
        None => 1u64 << 32,
    }
}

/// 焼けたブロック
struct Block {
    entry: unsafe extern "C" fn() -> u64,
    /// バッファの寿命の所有 (dropで実行可能メモリが消えるので手放さない)
    _buf: dynasmrt::ExecutableBuffer,
    n: u16,
    gen: u32,
    /// 損益の観測 (F1d-f): 入場数と実行命令数。taken分岐の早期退出で
    /// 「大きく焼けても2命令で出る」ブロックは入場税が勝つ — 標本が
    /// たまったら平均で裁いて負けブロックは負の印に降格する
    enters: u32,
    execd: u32,
}

/// 損益判定の標本数と、生き残りに要る平均実行命令数 (入場の固定費
/// ≒ インタプリタ2-3命令ぶん、の実測から)
const PROFIT_SAMPLE: u32 = 256;
const PROFIT_MIN_AVG: u32 = 3;

/// 直接マップのスロット。**ブロックはスロットが所有する** — 衝突退去は
/// dropで、メモリはJSLOTS×ブロック分で有界 (初版はblocks Vecに溜め込み、
/// 衝突ペアのリベイク暴走でExecutableBufferのmmapが無限に増えてOOM即死した)。
/// block: None かつ tag一致 = 「焼けない頭」の負の印 (collect再挑戦を防ぐ)
struct Slot {
    tag: u32,
    block: Option<Block>,
}

/// 直接マップのスロット数。taken分岐の着地 (~1/7命令) が毎回引くので、
/// HashMapでは税が勝つ (F1a-5 #58の教訓の再演を2026-08-16に実測 —
/// 67M probesで+1.9s)。64Kでは頭50k超に対して衝突リベイクが327k発生
/// したので256Kへ (スロット構造は薄く、ブロック本体はBoxの先)
const JSLOTS: usize = 2 * 1024 * 1024;
const TAG_FREE: u32 = 0xFFFF_FFFF;

struct Rt {
    layout: JitLayout,
    machine: *mut Machine,
    /// ブロック頭 pa → ブロックの直接マップ
    slots: Vec<Slot>,
    /// 観測: 焼いた数 / 語彙で落とした数 / 据付中の数
    pub baked: u64,
    pub rejected: u64,
    pub installed: u64,
    /// 損益判定で負の印に降格した数
    pub demoted: u64,
}

thread_local! {
    /// 取り付け済みランタイム (Box::leak)。try_enterはtaken分岐ごと (~1/7命令)
    /// に呼ばれるので、RefCellの借用検査すら払わない — 単一スレッド前提で
    /// 生ポインタ (アクセスは全部このモジュール内)
    static RT: std::cell::Cell<*mut Rt> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// ブロックの最大命令数。tick窓 (64命令) に収まる大きさ —
/// 「ブロック内でtickが起きない」入場保証 (core側のbudget) と噛み合う
const BLOCK_CAP: usize = 32;

/// JITを機械に取り付ける。**Machineは以後動かせない** (番地を焼き込むため。
/// 呼び手はBoxで持つこと)。取り外しは m.jit = None
///
/// # Safety
/// mが指すMachineがJIT実行中ずっと同じ番地に居ること (Box/Pin前提)
pub unsafe fn attach(m: &mut Machine) {
    let layout = jit::layout(m);
    let mut slots = Vec::with_capacity(JSLOTS);
    slots.resize_with(JSLOTS, || Slot {
        tag: TAG_FREE,
        block: None,
    });
    let rt = Box::leak(Box::new(Rt {
        layout,
        machine: m as *mut Machine,
        slots,
        baked: 0,
        demoted: 0,
        rejected: 0,
        installed: 0,
    }));
    RT.with(|cell| cell.set(rt as *mut Rt));
    m.jit = Some(JitHook { try_enter });
}

/// 環境変数 RUSTX86_JIT による標準の取り付け口 (タスク: ON/OFFの外部フラグ統一)。
/// "0" で無効、それ以外 (未設定含む) で有効。返り値 = 取り付けたか。
/// JIT対応ハーネスはこれを使う — on/off比較の作法を1箇所に固定する
///
/// # Safety
/// [`attach`] と同じ (MachineはBox固定)
pub unsafe fn attach_if_enabled(m: &mut Machine) -> bool {
    let on = std::env::var("RUSTX86_JIT")
        .map(|v| v != "0")
        .unwrap_or(true);
    if on {
        attach(m);
    }
    on
}

/// 観測値 (焼いた数, 語彙で落とした数, 据付中ブロック数, 損益降格数)
pub fn stats() -> (u64, u64, usize, u64) {
    RT.with(|cell| {
        let p = cell.get();
        if p.is_null() {
            (0, 0, 0, 0)
        } else {
            let r = unsafe { &*p };
            (r.baked, r.rejected, r.installed as usize, r.demoted)
        }
    })
}

fn try_enter(pa: u32, gen: u32, budget: u32) -> u32 {
    RT.with(|cell| {
        let p = cell.get();
        if p.is_null() {
            return 0;
        }
        // 単一スレッド・生成コードはRtに触らない — 借用は実行前に手放す構図を
        // 生ポインタで書いている (RefCell時代と同じ規律をコメントで固定)
        let rt = unsafe { &mut *p };
        // 命令頭は少数ページに密集する — ページ番号を混ぜて散らす
        let si = ((pa ^ (pa >> 12)) as usize) & (JSLOTS - 1);
        let slot = &mut rt.slots[si];
        if slot.tag == pa {
            let Some(b) = &mut slot.block else {
                return 0; // 焼けない頭 (語彙外・短すぎ・負け判定) の負の印
            };
            if b.gen != gen {
                // 世代落ち (自己書き換え)。捨てて、次の来訪で焼き直す
                slot.tag = TAG_FREE;
                slot.block = None;
                rt.installed -= 1;
                return 0;
            }
            if b.n as u32 > budget {
                return 0; // tick窓かチェーン残りに収まらない
            }
            let entry = b.entry;
            // 生成コードがMachineを書く間、Rtへの参照は使い切っている
            let k = unsafe { entry() } as u32;
            // 損益の観測: 256入場ごとに平均実行命令数で裁く**ローリング再審**。
            // 一度きりの審査だと、ブート期に良平均で通ったブロックが定常WLの
            // 分岐早退 (平均1.9命令) でも走り続ける — 局面は変わるものとして扱う
            let b = slot.block.as_mut().unwrap();
            b.enters += 1;
            b.execd += k;
            if b.enters == PROFIT_SAMPLE {
                if b.execd < PROFIT_SAMPLE * PROFIT_MIN_AVG {
                    // 入場税が勝つブロック — 負の印に降格 (tagは残す)
                    slot.block = None;
                    rt.installed -= 1;
                    rt.demoted += 1;
                } else {
                    b.enters = 0;
                    b.execd = 0;
                }
            }
            return k;
        }
        // ミス: 衝突退去 (直接マップ — dropで旧ブロックのメモリごと返す) か初訪
        // ---- 焼く (同期、初訪のみ)。collectは&Machineを覗くだけ ----
        let m = unsafe { &*rt.machine };
        let neg = |rt: &mut Rt| {
            rt.rejected += 1;
            if rt.slots[si].block.is_some() {
                rt.installed -= 1;
            }
            rt.slots[si] = Slot {
                tag: pa,
                block: None,
            };
        };
        let Some(blk) = jit::collect_block_caps(m, pa, BLOCK_CAP, jit::CAP_VOCAB2 | jit::CAP_CHAIN)
        else {
            neg(rt);
            return 0;
        };
        let ops = reg_only_prefix(&blk.ops);
        if ops.len() < 4 {
            // 小物はインタプリタに残す — 入場 (呼び出し+プロローグ+毎opロード/
            // ストア) の固定費が2-3命令では回収できない (F1d-f実測: 平均1.7命令/
            // 入場の嵐でgcc窓が-40%)。負の印で以後のプローブは即返し
            neg(rt);
            return 0;
        }
        let machine_addr = rt.machine as usize;
        let gen_addr = jit::page_gen_addr(m, pa);
        let Some(mut block) = emit_block(&rt.layout, machine_addr, &ops, gen_addr, gen) else {
            neg(rt);
            return 0;
        };
        block.gen = gen;
        // fillと同じ義務: このページに「コードあり」を立てる (立て忘れると
        // note_writeが素通りして世代が動かず、SMC/DMA上書きを見逃す)
        unsafe { jit::mark_code_page(&mut *rt.machine, pa) };
        rt.baked += 1;
        let n = block.n as u32;
        let entry = block.entry;
        if rt.slots[si].block.is_none() {
            rt.installed += 1;
        }
        rt.slots[si] = Slot {
            tag: pa,
            block: Some(block),
        };
        if n > budget {
            return 0;
        }
        unsafe { entry() as u32 }
    })
}

/// レジスタ形だけの前置列を切り出す (メモリ・スタック・8bit形の手前で打ち切り)。
/// 終端 (Jcc/Jmp) は含める
fn reg_only_prefix(ops: &[(u8, JitOp)]) -> Vec<(u8, JitOp)> {
    let mut out = Vec::new();
    for &(len, op) in ops {
        let ok = matches!(
            op,
            JitOp::MovRI { .. }
                | JitOp::MovRR { .. }
                | JitOp::AluRR { .. }
                | JitOp::AluRI { .. }
                | JitOp::TestRR { .. }
                | JitOp::IncDec { .. }
                | JitOp::Lea { .. }
                | JitOp::XchgA { .. }
                | JitOp::ShiftRI { .. }
                | JitOp::ShiftRC { .. }
                // ---- ロード形 (F1d-b)。脱出モデル: フォールトしそうなら
                //      状態を1つも変えずに実行済みiで戻る ----
                | JitOp::MovRM { .. }
                | JitOp::AluRM { .. }
                | JitOp::CmpMR { .. }
                | JitOp::CmpMI { .. }
                | JitOp::TestMR { .. }
                | JitOp::MovzxBM { .. }
                | JitOp::MovzxWM { .. }
                // ---- 8bit形 (F1d-f)。cc1の主食 — 材料はalu8の写し (cc_w=0) ----
                | JitOp::Mov8RI { .. }
                | JitOp::Mov8RR { .. }
                | JitOp::Mov8RM { .. }
                | JitOp::MovzxBR { .. }
                | JitOp::MovzxWR { .. }
                | JitOp::Alu8RR { .. }
                | JitOp::Alu8RI { .. }
                | JitOp::Alu8RM { .. }
                | JitOp::Cmp8MR { .. }
                | JitOp::Cmp8MI { .. }
                | JitOp::Test8RR { .. }
                | JitOp::Test8MR { .. }
                | JitOp::Grp3b8R { .. }
                | JitOp::Store8MR { .. }
                | JitOp::Store8MI { .. }
                | JitOp::Rmw8MR { .. }
                | JitOp::Rmw8MI { .. }
                // ---- ストア/スタック形 (F1d-c)。ストアの後は自ページ世代を
                //      照合し、動いていたらn+1で脱出 (jit.rsの契約) ----
                | JitOp::StoreMR { .. }
                | JitOp::StoreMI { .. }
                | JitOp::AluMR { .. }
                | JitOp::AluMI { .. }
                | JitOp::PushR { .. }
                | JitOp::PushI { .. }
                | JitOp::PopR { .. }
                | JitOp::Leave
                | JitOp::Jcc { .. }
                | JitOp::Jmp { .. }
                | JitOp::CallRel { .. }
                | JitOp::Ret
        );
        if !ok {
            break;
        }
        out.push((len, op));
        // Jccは終端にしない (CAP_CHAIN両側焼き — 不成立側を同じブロックで続ける)
        if matches!(op, JitOp::Jmp { .. } | JitOp::CallRel { .. } | JitOp::Ret) {
            break;
        }
    }
    out
}

/// 64bit即値をレジスタへ (movz + movk×3)。番地の焼き込み用
fn mov_abs(a: &mut dynasmrt::aarch64::Assembler, reg: u8, v: u64) {
    let lo = (v & 0xffff) as u32;
    dynasm!(a; .arch aarch64; movz X(reg), lo);
    let p1 = ((v >> 16) & 0xffff) as u32;
    let p2 = ((v >> 32) & 0xffff) as u32;
    let p3 = ((v >> 48) & 0xffff) as u32;
    if p1 != 0 {
        dynasm!(a; .arch aarch64; movk X(reg), p1, lsl 16);
    }
    if p2 != 0 {
        dynasm!(a; .arch aarch64; movk X(reg), p2, lsl 32);
    }
    if p3 != 0 {
        dynasm!(a; .arch aarch64; movk X(reg), p3, lsl 48);
    }
}

/// 32bit即値を w レジスタへ
fn mov_imm32(a: &mut dynasmrt::aarch64::Assembler, reg: u8, v: u32) {
    let lo = v & 0xffff;
    let hi = v >> 16;
    dynasm!(a; .arch aarch64; movz W(reg), lo);
    if hi != 0 {
        dynasm!(a; .arch aarch64; movk W(reg), hi, lsl 16);
    }
}

/// x19 (=Machine) からの差分。ldr/str の imm12 (4バイトスケール) に
/// 収まるときだけ Some — 収まらなければ呼び手が絶対番地へフォールバック
fn field_off4(machine: usize, addr: usize) -> Option<u32> {
    let d = addr.wrapping_sub(machine);
    if d < 16384 && d.is_multiple_of(4) {
        Some(d as u32)
    } else {
        None
    }
}

/// 同・バイトアクセス (imm12、スケールなし)
fn field_off1(machine: usize, addr: usize) -> Option<u32> {
    let d = addr.wrapping_sub(machine);
    if d < 4096 {
        Some(d as u32)
    } else {
        None
    }
}

/// guest regs[i] を w<dst> へ (x19相対1命令、収まらなければ絶対番地)
fn load_reg(a: &mut dynasmrt::aarch64::Assembler, l: &JitLayout, machine: usize, dst: u8, idx: u8) {
    let addr = l.regs + idx as usize * 4;
    if let Some(off) = field_off4(machine, addr) {
        dynasm!(a; .arch aarch64; ldr W(dst), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; ldr W(dst), [x9]);
    }
}

/// w<src> を guest regs[i] へ
fn store_reg(
    a: &mut dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    machine: usize,
    src: u8,
    idx: u8,
) {
    let addr = l.regs + idx as usize * 4;
    if let Some(off) = field_off4(machine, addr) {
        dynasm!(a; .arch aarch64; str W(src), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; str W(src), [x9]);
    }
}

/// guest 8bitレジスタのバイト番地 (ホストはLE: AL = regs[0]の第0バイト、
/// AH = regs[0]の第1バイト — reg8 0-3 = AL CL DL BL / 4-7 = AH CH DH BH)
fn reg8_addr(l: &JitLayout, r8: u8) -> usize {
    l.regs + (r8 as usize & 3) * 4 + usize::from(r8 >= 4)
}

/// guest 8bitレジスタを w<dst> へ (ldrbはゼロ拡張 — alu8の `a as u32` と同じ)
fn load_reg8(
    a: &mut dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    machine: usize,
    dst: u8,
    r8: u8,
) {
    let addr = reg8_addr(l, r8);
    if let Some(off) = field_off1(machine, addr) {
        dynasm!(a; .arch aarch64; ldrb W(dst), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; ldrb W(dst), [x9]);
    }
}

/// w<src> の下位1バイトを guest 8bitレジスタへ (他のバイトは不変 = set_reg8)
fn store_reg8(
    a: &mut dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    machine: usize,
    src: u8,
    r8: u8,
) {
    let addr = reg8_addr(l, r8);
    if let Some(off) = field_off1(machine, addr) {
        dynasm!(a; .arch aarch64; strb W(src), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; strb W(src), [x9]);
    }
}

/// 番地 addr へ w<src> を書く
fn store_at(a: &mut dynasmrt::aarch64::Assembler, machine: usize, addr: usize, src: u8) {
    if let Some(off) = field_off4(machine, addr) {
        dynasm!(a; .arch aarch64; str W(src), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; str W(src), [x9]);
    }
}

/// 番地 addr へ w<src> の下位1バイトを書く
fn store_b_at(a: &mut dynasmrt::aarch64::Assembler, machine: usize, addr: usize, src: u8) {
    if let Some(off) = field_off1(machine, addr) {
        dynasm!(a; .arch aarch64; strb W(src), [x19, off]);
    } else {
        mov_abs(a, 9, addr as u64);
        dynasm!(a; .arch aarch64; strb W(src), [x9]);
    }
}

/// ALUの遅延材料 (op/w/a/b/cin/r) をまとめて書く。
/// a=w10, b=w11, cin=w12, r=w13 に置いてから呼ぶ約束
fn store_cc(a: &mut dynasmrt::aarch64::Assembler, l: &JitLayout, machine: usize, op: u8, w: u8) {
    mov_imm32(a, 14, op as u32);
    store_b_at(a, machine, l.cc_op, 14);
    mov_imm32(a, 14, w as u32);
    store_b_at(a, machine, l.cc_w, 14);
    store_at(a, machine, l.cc_a, 10);
    store_at(a, machine, l.cc_b, 11);
    store_at(a, machine, l.cc_cin, 12);
    store_at(a, machine, l.cc_r, 13);
}

/// h_cf(machine) を呼び、結果 (0/1) を w<dst> に。x0-x17は死ぬ
fn call_cf(a: &mut dynasmrt::aarch64::Assembler, _machine: usize, dst: u8) {
    dynasm!(a; .arch aarch64; mov x0, x19);
    mov_abs(a, 16, h_cf as usize as u64);
    dynasm!(a; .arch aarch64; blr x16; mov W(dst), w0);
}

/// ブロックを機械語へ。戻り値 None = このopは焼けない (呼び手が弾く)
/// ブロックを機械語へ。戻り値 None = このopは焼けない (呼び手が弾く)。
///
/// ## 出口の設計 (F1d-b)
///
/// 出口は1つの共有テール (->exit) に集約する。契約: **w10 = ip0に足す差分、
/// w15 = 実行済み命令数**。終端 (Jcc/Jmp)・直線の落ち・メモリ脱出の全部が
/// この2レジスタを立てて exit へ飛ぶ。脱出は「op i の手前で戻る」=
/// w10 = opのブロック内オフセット、w15 = i (op iは未実行、状態は無傷)
fn emit_block(
    l: &JitLayout,
    machine: usize,
    ops: &[(u8, JitOp)],
    gen_addr: usize,
    gen: u32,
) -> Option<Block> {
    let mut a = dynasmrt::aarch64::Assembler::new().ok()?;
    let n = ops.len() as u16;
    let total_len: u32 = ops.iter().map(|&(len, _)| len as u32).sum();

    // prologue: フレーム48B。x19=Machine / x20=TLB先頭 / x21=ゲストRAM先頭 を
    // 掴み置き (callee-saved — ヘルパ呼びでも生きる)。フィールドは [x19,#差分]、
    // TLBインライン路は x20/x21 相対で走る。スピル1枠は [sp,40]
    dynasm!(a; .arch aarch64
        ; sub sp, sp, 48
        ; stp x29, x30, [sp]
        ; stp x19, x20, [sp, 16]
        ; str x21, [sp, 32]
        ; mov x29, sp
    );
    mov_abs(&mut a, 19, machine as u64);
    mov_abs(&mut a, 20, l.tlb as u64);
    mov_abs(&mut a, 21, l.mem as u64);

    let mut off: u32 = 0; // ブロック頭からのバイトオフセット (ip差分用)
    let mut terminal = false;
    for (i, &(len, op)) in ops.iter().enumerate() {
        match op {
            JitOp::MovRI { dst, imm } => {
                mov_imm32(&mut a, 10, imm);
                store_reg(&mut a, l, machine, 10, dst);
            }
            JitOp::MovRR { dst, src } => {
                load_reg(&mut a, l, machine, 10, src);
                store_reg(&mut a, l, machine, 10, dst);
            }
            JitOp::XchgA { reg } => {
                if reg != 0 {
                    load_reg(&mut a, l, machine, 10, 0);
                    load_reg(&mut a, l, machine, 11, reg);
                    store_reg(&mut a, l, machine, 11, 0);
                    store_reg(&mut a, l, machine, 10, reg);
                }
            }
            JitOp::Lea {
                dst,
                base,
                index,
                scale,
                disp,
            } => {
                emit_ea(&mut a, l, machine, 10, base, index, scale, disp);
                store_reg(&mut a, l, machine, 10, dst);
            }
            JitOp::AluRR { kind, dst, src } => {
                // ADC/SBB: cinのヘルパ呼びが全caller-savedを壊すので、
                // b→スピル → call_cf → b復元 → a取得 の順 (レジスタ形はaも取り直し)
                if kind == 2 || kind == 3 {
                    call_cf(&mut a, machine, 12);
                    load_reg(&mut a, l, machine, 10, dst);
                    load_reg(&mut a, l, machine, 11, src);
                } else {
                    load_reg(&mut a, l, machine, 10, dst);
                    load_reg(&mut a, l, machine, 11, src);
                }
                emit_alu(&mut a, kind)?;
                if kind != 7 {
                    store_reg(&mut a, l, machine, 13, dst);
                }
                store_cc(&mut a, l, machine, kind, 2);
            }
            JitOp::AluRI { kind, dst, imm } => {
                if kind == 2 || kind == 3 {
                    call_cf(&mut a, machine, 12);
                }
                load_reg(&mut a, l, machine, 10, dst);
                mov_imm32(&mut a, 11, imm);
                emit_alu(&mut a, kind)?;
                if kind != 7 {
                    store_reg(&mut a, l, machine, 13, dst);
                }
                store_cc(&mut a, l, machine, kind, 2);
            }
            JitOp::TestRR { a: ra, b: rb } => {
                load_reg(&mut a, l, machine, 10, ra);
                load_reg(&mut a, l, machine, 11, rb);
                dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11);
                store_cc(&mut a, l, machine, 4, 2);
            }
            JitOp::IncDec { reg, dec } => {
                call_cf(&mut a, machine, 15); // w15 = 旧CF (この後ヘルパを呼ばない区間)
                load_reg(&mut a, l, machine, 10, reg);
                mov_imm32(&mut a, 11, 1);
                dynasm!(a; .arch aarch64; movz w12, 0);
                if dec {
                    dynasm!(a; .arch aarch64; sub w13, w10, 1);
                } else {
                    dynasm!(a; .arch aarch64; add w13, w10, 1);
                }
                store_reg(&mut a, l, machine, 13, reg);
                store_cc(
                    &mut a,
                    l,
                    machine,
                    if dec { jit::CC_DEC } else { jit::CC_INC },
                    2,
                );
                mov_abs(&mut a, 9, l.flags as u64);
                dynasm!(a; .arch aarch64
                    ; ldr w10, [x9]
                    ; and w10, w10, 0xFFFF_FFFE  // CF (bit0) を消す
                    ; orr w10, w10, w15
                    ; str w10, [x9]
                );
            }
            JitOp::ShiftRI { kind, reg, count } => {
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, kind as u32);
                mov_imm32(&mut a, 2, reg as u32);
                mov_imm32(&mut a, 3, count as u32);
                mov_abs(&mut a, 16, h_shift as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
            }
            JitOp::ShiftRC { kind, reg } => {
                load_reg(&mut a, l, machine, 3, 1); // CL = regs[1]の下位
                dynasm!(a; .arch aarch64; and w3, w3, 0xff);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, kind as u32);
                mov_imm32(&mut a, 2, reg as u32);
                mov_abs(&mut a, 16, h_shift as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
            }
            // ---- ロード形 (F1d-b)。w0 = 値、脱出はexitへ ----
            JitOp::MovRM { dst, mem } => {
                emit_load(&mut a, l, machine, &mem, 4, i as u32, off);
                store_reg(&mut a, l, machine, 0, dst);
            }
            JitOp::MovzxBM { dst, mem } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                store_reg(&mut a, l, machine, 0, dst); // h_ld8はゼロ拡張済みの32bitを返す
            }
            JitOp::MovzxWM { dst, mem } => {
                emit_load(&mut a, l, machine, &mem, 2, i as u32, off);
                store_reg(&mut a, l, machine, 0, dst);
            }
            JitOp::AluRM { kind, dst, mem } => {
                emit_load(&mut a, l, machine, &mem, 4, i as u32, off);
                if kind == 2 || kind == 3 {
                    // b (=ロード値) をスピルしてcinを取り、復元
                    dynasm!(a; .arch aarch64; str w0, [sp, 40]);
                    call_cf(&mut a, machine, 12);
                    dynasm!(a; .arch aarch64; ldr w11, [sp, 40]);
                } else {
                    dynasm!(a; .arch aarch64; mov w11, w0);
                }
                load_reg(&mut a, l, machine, 10, dst);
                emit_alu(&mut a, kind)?;
                if kind != 7 {
                    store_reg(&mut a, l, machine, 13, dst);
                }
                store_cc(&mut a, l, machine, kind, 2);
            }
            JitOp::CmpMR { mem, reg } => {
                // cmp [mem], r — a=mem値, b=reg (向きが逆でないことに注意:
                // rm=dst形なので a がメモリ側)
                emit_load(&mut a, l, machine, &mem, 4, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                load_reg(&mut a, l, machine, 11, reg);
                dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11);
                store_cc(&mut a, l, machine, 7, 2);
            }
            JitOp::CmpMI { mem, imm } => {
                emit_load(&mut a, l, machine, &mem, 4, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                mov_imm32(&mut a, 11, imm);
                dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11);
                store_cc(&mut a, l, machine, 7, 2);
            }
            JitOp::TestMR { mem, reg } => {
                emit_load(&mut a, l, machine, &mem, 4, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                load_reg(&mut a, l, machine, 11, reg);
                dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11);
                store_cc(&mut a, l, machine, 4, 2);
            }
            // ---- ストア/RMW形 (F1d-c)。ヘルパ完了後に自ページ世代を照合、
            //      動いていたら i+1 で脱出 (最後のopなら照合不要 — どのみち終わる) ----
            JitOp::StoreMR { mem, src } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                load_reg(&mut a, l, machine, 3, src);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_abs(&mut a, 16, h_st32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::StoreMI { mem, imm } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                mov_imm32(&mut a, 3, imm);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_abs(&mut a, 16, h_st32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::AluMR { kind, mem, reg } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                load_reg(&mut a, l, machine, 4, reg);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_imm32(&mut a, 3, kind as u32);
                mov_abs(&mut a, 16, h_rmw32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::AluMI { kind, mem, imm } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                mov_imm32(&mut a, 4, imm);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_imm32(&mut a, 3, kind as u32);
                mov_abs(&mut a, 16, h_rmw32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            // ---- スタック形。pushは書き込み=世代照合あり、popは読みだけ ----
            JitOp::PushR { src } => {
                load_reg(&mut a, l, machine, 1, src);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_abs(&mut a, 16, h_push32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::PushI { imm } => {
                mov_imm32(&mut a, 1, imm);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_abs(&mut a, 16, h_push32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::PopR { dst } => {
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_abs(&mut a, 16, h_pop32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16; lsr x1, x0, 32; cbz x1, >ok);
                mov_imm32(&mut a, 10, off);
                mov_imm32(&mut a, 15, i as u32);
                dynasm!(a; .arch aarch64; b ->exit; ok:);
                store_reg(&mut a, l, machine, 0, dst);
            }
            JitOp::Leave => {
                // 読み (SS:[BP]) が確定してから SP=BP+4、BP=読んだ値 (execと同順)
                load_reg(&mut a, l, machine, 2, 5); // w2 = BP
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, 2); // SS
                mov_abs(&mut a, 16, h_ld32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16; lsr x1, x0, 32; cbz x1, >ok);
                mov_imm32(&mut a, 10, off);
                mov_imm32(&mut a, 15, i as u32);
                dynasm!(a; .arch aarch64; b ->exit; ok:);
                load_reg(&mut a, l, machine, 3, 5); // BP (ヘルパ後に取り直し)
                dynasm!(a; .arch aarch64; add w3, w3, 4);
                store_reg(&mut a, l, machine, 3, 4); // SP = BP+4
                store_reg(&mut a, l, machine, 0, 5); // BP = [旧BP]
            }
            // ---- 終端: call/ret ----
            JitOp::CallRel { rel } => {
                // 戻り番地 = ip0 + off + len を push (脱出したらpushしていない)
                mov_abs(&mut a, 9, l.ip as u64);
                mov_imm32(&mut a, 2, off.wrapping_add(len as u32));
                dynasm!(a; .arch aarch64; ldr w1, [x9]; add w1, w1, w2);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_abs(&mut a, 16, h_push32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                let taken = off.wrapping_add(len as u32).wrapping_add(rel);
                mov_imm32(&mut a, 10, taken);
                mov_imm32(&mut a, 15, n as u32);
                terminal = true;
            }
            JitOp::Ret => {
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_abs(&mut a, 16, h_pop32 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16; lsr x1, x0, 32; cbz x1, >ok);
                mov_imm32(&mut a, 10, off);
                mov_imm32(&mut a, 15, i as u32);
                dynasm!(a; .arch aarch64; b ->exit; ok:);
                // 着地は絶対番地 (popした値) — deltaではなく exit_abs へ
                dynasm!(a; .arch aarch64; mov w10, w0);
                mov_imm32(&mut a, 15, n as u32);
                dynasm!(a; .arch aarch64; b ->exit_abs);
                terminal = true;
            }
            JitOp::Jcc { cc, rel } => {
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, cc as u32);
                mov_abs(&mut a, 16, h_cond as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                let not_taken = off.wrapping_add(len as u32);
                let taken = not_taken.wrapping_add(rel);
                if i + 1 == ops.len() {
                    // 終端形: 両側とも出口 (従来どおり)
                    mov_imm32(&mut a, 10, taken);
                    mov_imm32(&mut a, 11, not_taken);
                    dynasm!(a; .arch aarch64; cmp w0, 0; csel w10, w10, w11, ne);
                    mov_imm32(&mut a, 15, n as u32);
                    terminal = true;
                } else {
                    // 両側焼き (CAP_CHAIN): 成立側は「完全実行済み i+1」で
                    // 途中退出、不成立側は同じブロックの続きを走る
                    dynasm!(a; .arch aarch64; cbz w0, >nt);
                    mov_imm32(&mut a, 10, taken);
                    mov_imm32(&mut a, 15, (i + 1) as u32);
                    dynasm!(a; .arch aarch64; b ->exit; nt:);
                }
            }
            JitOp::Jmp { rel } => {
                let taken = off.wrapping_add(len as u32).wrapping_add(rel);
                mov_imm32(&mut a, 10, taken);
                mov_imm32(&mut a, 15, n as u32);
                terminal = true;
            }
            // ---- 8bit形 (F1d-f)。材料はalu8と同じ: a/bはゼロ拡張、
            //      cc_rは**幅でマスク** (alu_lazyの & mask を写す)。cc_w=0 ----
            JitOp::Mov8RI { dst8, imm } => {
                mov_imm32(&mut a, 10, imm as u32);
                store_reg8(&mut a, l, machine, 10, dst8);
            }
            JitOp::Mov8RR { dst8, src8 } => {
                load_reg8(&mut a, l, machine, 10, src8);
                store_reg8(&mut a, l, machine, 10, dst8);
            }
            JitOp::Mov8RM { dst8, mem } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                store_reg8(&mut a, l, machine, 0, dst8);
            }
            JitOp::MovzxBR { dst, src8 } => {
                load_reg8(&mut a, l, machine, 10, src8); // ldrbがゼロ拡張済み
                store_reg(&mut a, l, machine, 10, dst);
            }
            JitOp::MovzxWR { dst, src } => {
                load_reg(&mut a, l, machine, 10, src);
                dynasm!(a; .arch aarch64; uxth w10, w10);
                store_reg(&mut a, l, machine, 10, dst);
            }
            JitOp::Alu8RR { kind, dst8, src8 } => {
                if kind == 2 || kind == 3 {
                    call_cf(&mut a, machine, 12);
                }
                load_reg8(&mut a, l, machine, 10, dst8);
                load_reg8(&mut a, l, machine, 11, src8);
                emit_alu(&mut a, kind)?;
                dynasm!(a; .arch aarch64; and w13, w13, 0xff);
                if kind != 7 {
                    store_reg8(&mut a, l, machine, 13, dst8);
                }
                store_cc(&mut a, l, machine, kind, 0);
            }
            JitOp::Alu8RI { kind, dst8, imm } => {
                if kind == 2 || kind == 3 {
                    call_cf(&mut a, machine, 12);
                }
                load_reg8(&mut a, l, machine, 10, dst8);
                mov_imm32(&mut a, 11, imm as u32);
                emit_alu(&mut a, kind)?;
                dynasm!(a; .arch aarch64; and w13, w13, 0xff);
                if kind != 7 {
                    store_reg8(&mut a, l, machine, 13, dst8);
                }
                store_cc(&mut a, l, machine, kind, 0);
            }
            JitOp::Alu8RM { kind, dst8, mem } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                if kind == 2 || kind == 3 {
                    dynasm!(a; .arch aarch64; str w0, [sp, 40]);
                    call_cf(&mut a, machine, 12);
                    dynasm!(a; .arch aarch64; ldr w11, [sp, 40]);
                } else {
                    dynasm!(a; .arch aarch64; mov w11, w0);
                }
                load_reg8(&mut a, l, machine, 10, dst8);
                emit_alu(&mut a, kind)?;
                dynasm!(a; .arch aarch64; and w13, w13, 0xff);
                if kind != 7 {
                    store_reg8(&mut a, l, machine, 13, dst8);
                }
                store_cc(&mut a, l, machine, kind, 0);
            }
            JitOp::Cmp8MR { mem, reg8 } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                load_reg8(&mut a, l, machine, 11, reg8);
                dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11; and w13, w13, 0xff);
                store_cc(&mut a, l, machine, 7, 0);
            }
            JitOp::Cmp8MI { mem, imm } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                mov_imm32(&mut a, 11, imm as u32);
                dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11; and w13, w13, 0xff);
                store_cc(&mut a, l, machine, 7, 0);
            }
            JitOp::Test8RR { a8, b8 } => {
                load_reg8(&mut a, l, machine, 10, a8);
                load_reg8(&mut a, l, machine, 11, b8);
                dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11);
                store_cc(&mut a, l, machine, 4, 0);
            }
            JitOp::Test8MR { mem, reg8 } => {
                emit_load(&mut a, l, machine, &mem, 1, i as u32, off);
                dynasm!(a; .arch aarch64; mov w10, w0);
                load_reg8(&mut a, l, machine, 11, reg8);
                dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11);
                store_cc(&mut a, l, machine, 4, 0);
            }
            JitOp::Grp3b8R { kind, reg8, imm } => {
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, kind as u32);
                mov_imm32(&mut a, 2, reg8 as u32);
                mov_imm32(&mut a, 3, imm as u32);
                mov_abs(&mut a, 16, h_grp3b8 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
            }
            // ---- 8bitの書く形。ストア後の自ページ世代照合は32bit形と同じ契約 ----
            JitOp::Store8MR { mem, src8 } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                load_reg8(&mut a, l, machine, 3, src8);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_abs(&mut a, 16, h_st8 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::Store8MI { mem, imm } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                mov_imm32(&mut a, 3, imm as u32);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_abs(&mut a, 16, h_st8 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::Rmw8MR { kind, mem, reg8 } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                load_reg8(&mut a, l, machine, 4, reg8);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_imm32(&mut a, 3, kind as u32);
                mov_abs(&mut a, 16, h_rmw8 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
            JitOp::Rmw8MI { kind, mem, imm } => {
                emit_ea(
                    &mut a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp,
                );
                mov_imm32(&mut a, 4, imm as u32);
                dynasm!(a; .arch aarch64; mov x0, x19);
                mov_imm32(&mut a, 1, mem.seg as u32);
                mov_imm32(&mut a, 3, kind as u32);
                mov_abs(&mut a, 16, h_rmw8 as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                emit_escape_if_zero(&mut a, i as u32, off);
                emit_genck(
                    &mut a,
                    gen_addr,
                    gen,
                    i,
                    off.wrapping_add(len as u32),
                    ops.len(),
                );
            }
        }
        off = off.wrapping_add(len as u32);
    }
    if !terminal {
        // 非終端 (語彙外の手前・cap) で終わるブロック: 直線の着地
        mov_imm32(&mut a, 10, total_len);
        mov_imm32(&mut a, 15, n as u32);
    }
    // ---- 共有テール: ->exit は ip += w10 (差分)、->exit_abs は ip = w10 (絶対)。
    //      どちらも w15 (実行済み命令数) を返す ----
    dynasm!(a; .arch aarch64; ->exit:);
    mov_abs(&mut a, 9, l.ip as u64);
    dynasm!(a; .arch aarch64
        ; ldr w11, [x9]
        ; add w11, w11, w10
        ; str w11, [x9]
        ; mov w0, w15
        ; ldp x19, x20, [sp, 16]
        ; ldr x21, [sp, 32]
        ; ldp x29, x30, [sp]
        ; add sp, sp, 48
        ; ret
        ; ->exit_abs:
    );
    mov_abs(&mut a, 9, l.ip as u64);
    dynasm!(a; .arch aarch64
        ; str w10, [x9]
        ; mov w0, w15
        ; ldp x19, x20, [sp, 16]
        ; ldr x21, [sp, 32]
        ; ldp x29, x30, [sp]
        ; add sp, sp, 48
        ; ret
    );
    let start = dynasmrt::AssemblyOffset(0);
    let buf = a.finalize().ok()?;
    let entry: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(buf.ptr(start)) };
    Some(Block {
        entry,
        _buf: buf,
        n,
        gen: 0, // 呼び手が上書き
        enters: 0,
        execd: 0,
    })
}

/// 実効オフセットを w<dst> に作る (disp + base + index<<scale)
#[allow(clippy::too_many_arguments)] // EAの材料そのもの — 束ねる構造体を作る方が嵩む
fn emit_ea(
    a: &mut dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    machine: usize,
    dst: u8,
    base: i8,
    index: i8,
    scale: u8,
    disp: u32,
) {
    mov_imm32(a, dst, disp);
    if base >= 0 {
        load_reg(a, l, machine, 11, base as u8);
        dynasm!(a; .arch aarch64; add W(dst), W(dst), w11);
    }
    if index >= 0 {
        load_reg(a, l, machine, 11, index as u8);
        match scale {
            0 => dynasm!(a; .arch aarch64; add W(dst), W(dst), w11),
            1 => dynasm!(a; .arch aarch64; add W(dst), W(dst), w11, lsl 1),
            2 => dynasm!(a; .arch aarch64; add W(dst), W(dst), w11, lsl 2),
            _ => dynasm!(a; .arch aarch64; add W(dst), W(dst), w11, lsl 3),
        }
    }
}

/// メモリロードを emit する: EA → h_ld{8,16,32} → 脱出検査。
/// 成功時 w0 = 値 (8/16bitはゼロ拡張済み)。脱出時は「op iの手前」で
/// exitへ (w10 = ip差分 = opのオフセット、w15 = i)
fn emit_load(
    a: &mut dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    machine: usize,
    mem: &rustx86_core::jit::JitMem,
    width: u8,
    i: u32,
    off: u32,
) {
    emit_ea(a, l, machine, 2, mem.base, mem.index, mem.scale, mem.disp); // w2 = off
                                                                         // ---- TLBヒットのインライン高速路 (F1d-d、32bitのみ) ----
                                                                         //
                                                                         // translate_forのヒット路 (カーネル・読み) の写し: リング0の読みは
                                                                         // 権限検査もA/D更新も無い — probe+合成だけで物理に届く。
                                                                         // 外れる条件 (CPL3 / ページ跨ぎ / TLBミス / RAM外) は全部従来ヘルパへ
                                                                         // (意味論はそちらが原本。インラインはヒットの近道でしかない)。
                                                                         // PG無効期 (解凍ステブ) はTLBが空なのでタグ不一致→ヘルパ = 従来どおり。
                                                                         // 注意: opstatsのtlb_probes計上はこの近道を通ると増えない (計測ビルドの
                                                                         // JITは近道ぶんだけ過小になる — 定規はJIT offで取る約束)
    if width == 4 {
        // la = hidden[seg].base + off
        let base_addr = l.hidden + mem.seg as usize * 12;
        if let Some(o) = field_off4(machine, base_addr) {
            dynasm!(a; .arch aarch64; ldr w3, [x19, o]);
        } else {
            mov_abs(a, 9, base_addr as u64);
            dynasm!(a; .arch aarch64; ldr w3, [x9]);
        }
        dynasm!(a; .arch aarch64; add w3, w3, w2);
        // CPL==3 なら遅い道 (U/S検査はヘルパにしか無い)
        let cs_addr = l.sregs + 2; // sregs[CS=1] (u16)
        let d = cs_addr.wrapping_sub(machine);
        if d < 4096 {
            let d = d as u32;
            dynasm!(a; .arch aarch64; ldrb w4, [x19, d]);
        } else {
            mov_abs(a, 9, cs_addr as u64);
            dynasm!(a; .arch aarch64; ldrb w4, [x9]);
        }
        dynasm!(a; .arch aarch64
            ; and w4, w4, 3
            ; cmp w4, 3
            ; b.eq >slow
            // ページ跨ぎは遅い道
            ; and w4, w3, 0xFFF
            ; cmp w4, 0xFFC
            ; b.hi >slow
            // TLB probe: slot = vpn & 4095、エントリは12B刻み
            ; lsr w5, w3, 12
            ; and w6, w5, 0xFFF
            ; add x7, x20, x6, lsl 3
            ; add x7, x7, x6, lsl 2
            ; ldp w8, w9, [x7]
            ; cmp w8, w5
            ; b.ne >slow
            // pa = (base_flags & !0xFFF) | (la & 0xFFF)
            ; and w10, w9, 0xFFFFF000
            ; orr w10, w10, w4
        );
        // RAM範囲 (pa+4 <= len)。128MB級なのでw即値2発で作る
        mov_imm32(a, 11, (l.mem_len.saturating_sub(4)) as u32);
        dynasm!(a; .arch aarch64
            ; cmp w10, w11
            ; b.hi >slow
            ; add x12, x21, w10, uxtw
            ; ldr w0, [x12]
            ; b >done
            ; slow:
        );
    }
    dynasm!(a; .arch aarch64; mov x0, x19);
    mov_imm32(a, 1, mem.seg as u32);
    let h = match width {
        1 => h_ld8 as usize,
        2 => h_ld16 as usize,
        _ => h_ld32 as usize,
    };
    mov_abs(a, 16, h as u64);
    dynasm!(a; .arch aarch64; blr x16
        ; lsr x1, x0, 32
        ; cbz x1, >done2
    );
    mov_imm32(a, 10, off); // ip差分 = このopのブロック内オフセット (未実行)
    mov_imm32(a, 15, i); // 実行済みはi個
    dynasm!(a; .arch aarch64; b ->exit; done2:; done:);
}

/// w0==0 (ヘルパの脱出合図) なら「op iの手前・状態無傷」でexitへ
fn emit_escape_if_zero(a: &mut dynasmrt::aarch64::Assembler, i: u32, off: u32) {
    dynasm!(a; .arch aarch64; cbnz w0, >ok);
    mov_imm32(a, 10, off);
    mov_imm32(a, 15, i);
    dynasm!(a; .arch aarch64; b ->exit; ok:);
}

/// 自ページ世代の照合 (ストア/push後、最後のop以外)。焼いた時の世代と
/// 違ったら**このopまで実行済み (i+1)** で脱出 — 自分の居るページを
/// 書き換えたブロックが古い続きを走らない (jit.rs の n+1契約)
fn emit_genck(
    a: &mut dynasmrt::aarch64::Assembler,
    gen_addr: usize,
    gen: u32,
    i: usize,
    off_after: u32,
    n_ops: usize,
) {
    if i + 1 == n_ops || gen_addr == 0 {
        return; // 最後のopはどのみちブロックが終わる — 次の頭照合が裁く
    }
    mov_abs(a, 9, gen_addr as u64);
    dynasm!(a; .arch aarch64; ldr w1, [x9]);
    mov_imm32(a, 2, gen);
    dynasm!(a; .arch aarch64; cmp w1, w2; b.eq >gok);
    mov_imm32(a, 10, off_after);
    mov_imm32(a, 15, (i + 1) as u32);
    dynasm!(a; .arch aarch64; b ->exit; gok:);
}

/// w10=a, w11=b, (ADC/SBBは w12=cin設定済み) から w13=r を作る。
/// フラグは作らない — 材料を store_cc が書く (意味論の原本は cc_cf/cc_of)
fn emit_alu(a: &mut dynasmrt::aarch64::Assembler, kind: u8) -> Option<()> {
    match kind {
        0 => dynasm!(a; .arch aarch64; movz w12, 0; add w13, w10, w11), // ADD
        1 => dynasm!(a; .arch aarch64; movz w12, 0; orr w13, w10, w11), // OR
        2 => dynasm!(a; .arch aarch64; add w13, w10, w11; add w13, w13, w12), // ADC (w12=cin)
        3 => dynasm!(a; .arch aarch64; sub w13, w10, w11; sub w13, w13, w12), // SBB
        4 => dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11), // AND
        5 | 7 => dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11), // SUB/CMP
        6 => dynasm!(a; .arch aarch64; movz w12, 0; eor w13, w10, w11), // XOR
        _ => return None,
    }
    Some(())
}
