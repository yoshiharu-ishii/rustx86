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

use dynasmrt::{dynasm, DynasmApi};
use rustx86_core::jit::{self, JitHook, JitLayout, JitOp};
use rustx86_core::{cpu, Machine};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

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

/// 焼けたブロック
struct Block {
    entry: unsafe extern "C" fn() -> u64,
    /// バッファの寿命の所有 (dropで実行可能メモリが消えるので手放さない)
    _buf: dynasmrt::ExecutableBuffer,
    n: u16,
    gen: u32,
}

struct Rt {
    layout: JitLayout,
    machine: *mut Machine,
    blocks: Vec<Block>,
    /// ブロック頭 pa → blocks index。チェーン入口 (~1/69命令) でしか
    /// 引かないので、F1a-5でEntryに畳んだhashmap税の教訓には抵触しない
    index: HashMap<u32, u32>,
    /// 語彙外などで焼けなかった頭 (再挑戦しない)。世代落ちで外す
    tried: HashSet<u32>,
    /// 観測: 焼いた数 / 語彙で落とした数
    pub baked: u64,
    pub rejected: u64,
}

thread_local! {
    static RT: RefCell<Option<Rt>> = const { RefCell::new(None) };
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
    RT.with(|rt| {
        *rt.borrow_mut() = Some(Rt {
            layout,
            machine: m as *mut Machine,
            blocks: Vec::new(),
            index: HashMap::new(),
            tried: HashSet::new(),
            baked: 0,
            rejected: 0,
        });
    });
    m.jit = Some(JitHook { try_enter });
}

/// 観測値 (焼いた数, 語彙で落とした数, 据付済みブロック数)
pub fn stats() -> (u64, u64, usize) {
    RT.with(|rt| {
        rt.borrow()
            .as_ref()
            .map(|r| (r.baked, r.rejected, r.blocks.len()))
            .unwrap_or((0, 0, 0))
    })
}

fn try_enter(pa: u32, gen: u32, budget: u32) -> u32 {
    RT.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(rt) = borrow.as_mut() else {
            return 0;
        };
        if let Some(&i) = rt.index.get(&pa) {
            let b = &rt.blocks[i as usize];
            if b.gen != gen {
                // 世代落ち (自己書き換え)。捨てて、次の来訪で焼き直す
                rt.index.remove(&pa);
                rt.tried.remove(&pa);
                return 0;
            }
            if b.n as u32 > budget {
                return 0; // tick窓かチェーン残りに収まらない
            }
            let entry = b.entry;
            // 生成コードがMachineを書く間、Rustの参照は一切持たない
            drop(borrow);
            return unsafe { entry() } as u32;
        }
        if rt.tried.contains(&pa) {
            return 0;
        }
        rt.tried.insert(pa);
        // ---- 焼く (同期、初訪のみ)。collectは&Machineを覗くだけ ----
        let m = unsafe { &*rt.machine };
        let Some(blk) = jit::collect_block_caps(m, pa, BLOCK_CAP, jit::CAP_VOCAB2) else {
            rt.rejected += 1;
            return 0;
        };
        let ops = reg_only_prefix(&blk.ops);
        if ops.len() < 2 {
            rt.rejected += 1; // 1命令はインタプリタと同着 — 呼び出し税だけ損
            return 0;
        }
        let machine_addr = rt.machine as usize;
        let Some(block) = emit_block(&rt.layout, machine_addr, &ops) else {
            rt.rejected += 1;
            return 0;
        };
        rt.baked += 1;
        let idx = rt.blocks.len() as u32;
        let n = block.n as u32;
        let entry = block.entry;
        rt.blocks.push(Block { gen, ..block });
        rt.index.insert(pa, idx);
        if n > budget {
            return 0;
        }
        drop(borrow);
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
                // ADC/SBB (kind 2/3) はCF入力のヘルパ呼びがa/bを壊すので
                // F1d-aでは外す (F1d-b: 再ロード形で入れる)
                | JitOp::AluRR { kind: 0 | 1 | 4..=7, .. }
                | JitOp::AluRI { kind: 0 | 1 | 4..=7, .. }
                | JitOp::TestRR { .. }
                | JitOp::IncDec { .. }
                | JitOp::Lea { .. }
                | JitOp::XchgA { .. }
                | JitOp::ShiftRI { .. }
                | JitOp::ShiftRC { .. }
                | JitOp::Jcc { .. }
                | JitOp::Jmp { .. }
        );
        if !ok {
            break;
        }
        out.push((len, op));
        if matches!(op, JitOp::Jcc { .. } | JitOp::Jmp { .. }) {
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

/// guest regs[i] を w<dst> へ
fn load_reg(a: &mut dynasmrt::aarch64::Assembler, l: &JitLayout, dst: u8, idx: u8) {
    mov_abs(a, 9, l.regs as u64);
    let off = idx as u32 * 4;
    dynasm!(a; .arch aarch64; ldr W(dst), [x9, off]);
}

/// w<src> を guest regs[i] へ
fn store_reg(a: &mut dynasmrt::aarch64::Assembler, l: &JitLayout, src: u8, idx: u8) {
    mov_abs(a, 9, l.regs as u64);
    let off = idx as u32 * 4;
    dynasm!(a; .arch aarch64; str W(src), [x9, off]);
}

/// 番地 addr へ w<src> を書く
fn store_at(a: &mut dynasmrt::aarch64::Assembler, addr: usize, src: u8) {
    mov_abs(a, 9, addr as u64);
    dynasm!(a; .arch aarch64; str W(src), [x9]);
}

/// 番地 addr へ w<src> の下位1バイトを書く
fn store_b_at(a: &mut dynasmrt::aarch64::Assembler, addr: usize, src: u8) {
    mov_abs(a, 9, addr as u64);
    dynasm!(a; .arch aarch64; strb W(src), [x9]);
}

/// ALUの遅延材料 (op/w/a/b/cin/r) をまとめて書く。
/// a=w10, b=w11, cin=w12, r=w13 に置いてから呼ぶ約束
fn store_cc(a: &mut dynasmrt::aarch64::Assembler, l: &JitLayout, op: u8, w: u8) {
    mov_imm32(a, 14, op as u32);
    store_b_at(a, l.cc_op, 14);
    mov_imm32(a, 14, w as u32);
    store_b_at(a, l.cc_w, 14);
    store_at(a, l.cc_a, 10);
    store_at(a, l.cc_b, 11);
    store_at(a, l.cc_cin, 12);
    store_at(a, l.cc_r, 13);
}

/// h_cf(machine) を呼び、結果 (0/1) を w<dst> に。x0-x17は死ぬ
fn call_cf(a: &mut dynasmrt::aarch64::Assembler, machine: usize, dst: u8) {
    mov_abs(a, 0, machine as u64);
    mov_abs(a, 16, h_cf as usize as u64);
    dynasm!(a; .arch aarch64; blr x16; mov W(dst), w0);
}

/// ブロックを機械語へ。戻り値 None = このopは焼けない (呼び手が弾く)
fn emit_block(l: &JitLayout, machine: usize, ops: &[(u8, JitOp)]) -> Option<Block> {
    let mut a = dynasmrt::aarch64::Assembler::new().ok()?;
    let n = ops.len() as u16;
    let total_len: u32 = ops.iter().map(|&(len, _)| len as u32).sum();

    // prologue: フレームを立てる (ヘルパ呼びで lr を使うため)
    dynasm!(a; .arch aarch64
        ; sub sp, sp, 16
        ; stp x29, x30, [sp]
        ; mov x29, sp
    );

    let mut off: u32 = 0; // ブロック頭からのバイトオフセット (ip差分用)
    for &(len, op) in ops {
        match op {
            JitOp::MovRI { dst, imm } => {
                mov_imm32(&mut a, 10, imm);
                store_reg(&mut a, l, 10, dst);
            }
            JitOp::MovRR { dst, src } => {
                load_reg(&mut a, l, 10, src);
                store_reg(&mut a, l, 10, dst);
            }
            JitOp::XchgA { reg } => {
                if reg != 0 {
                    load_reg(&mut a, l, 10, 0);
                    load_reg(&mut a, l, 11, reg);
                    store_reg(&mut a, l, 11, 0);
                    store_reg(&mut a, l, 10, reg);
                }
            }
            JitOp::Lea {
                dst,
                base,
                index,
                scale,
                disp,
            } => {
                mov_imm32(&mut a, 10, disp);
                if base >= 0 {
                    load_reg(&mut a, l, 11, base as u8);
                    dynasm!(a; .arch aarch64; add w10, w10, w11);
                }
                if index >= 0 {
                    load_reg(&mut a, l, 11, index as u8);
                    match scale {
                        0 => dynasm!(a; .arch aarch64; add w10, w10, w11),
                        1 => dynasm!(a; .arch aarch64; add w10, w10, w11, lsl 1),
                        2 => dynasm!(a; .arch aarch64; add w10, w10, w11, lsl 2),
                        _ => dynasm!(a; .arch aarch64; add w10, w10, w11, lsl 3),
                    }
                }
                store_reg(&mut a, l, 10, dst);
            }
            JitOp::AluRR { kind, dst, src } => {
                load_reg(&mut a, l, 10, dst); // a
                load_reg(&mut a, l, 11, src); // b
                emit_alu(&mut a, l, machine, kind)?;
                if kind != 7 {
                    store_reg(&mut a, l, 13, dst);
                }
                store_cc(&mut a, l, kind, 2);
            }
            JitOp::AluRI { kind, dst, imm } => {
                load_reg(&mut a, l, 10, dst);
                mov_imm32(&mut a, 11, imm);
                emit_alu(&mut a, l, machine, kind)?;
                if kind != 7 {
                    store_reg(&mut a, l, 13, dst);
                }
                store_cc(&mut a, l, kind, 2);
            }
            JitOp::TestRR { a: ra, b: rb } => {
                load_reg(&mut a, l, 10, ra);
                load_reg(&mut a, l, 11, rb);
                dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11);
                store_cc(&mut a, l, 4, 2); // AND の材料 (test = 書き戻さないAND)
            }
            JitOp::IncDec { reg, dec } => {
                // set_cc_incdec の写し: 先にCFを具現化して退避、材料はCC_INC/DEC
                call_cf(&mut a, machine, 15); // w15 = 旧CF (0/1)
                load_reg(&mut a, l, 10, reg); // a
                mov_imm32(&mut a, 11, 1); // b = 1
                dynasm!(a; .arch aarch64; movz w12, 0); // cin = 0
                if dec {
                    dynasm!(a; .arch aarch64; sub w13, w10, 1);
                } else {
                    dynasm!(a; .arch aarch64; add w13, w10, 1);
                }
                store_reg(&mut a, l, 13, reg);
                store_cc(&mut a, l, if dec { jit::CC_DEC } else { jit::CC_INC }, 2);
                // flags のCFビットへ旧値を書き戻す (CF不変の意味論)
                mov_abs(&mut a, 9, l.flags as u64);
                dynasm!(a; .arch aarch64
                    ; ldr w10, [x9]
                    ; and w10, w10, 0xFFFF_FFFE  // CF (bit0) を消す
                    ; orr w10, w10, w15
                    ; str w10, [x9]
                );
            }
            JitOp::ShiftRI { kind, reg, count } => {
                // 意味論はヘルパ (shift_rot) — eagerフラグ込みで完結
                mov_abs(&mut a, 0, machine as u64);
                mov_imm32(&mut a, 1, kind as u32);
                mov_imm32(&mut a, 2, reg as u32);
                mov_imm32(&mut a, 3, count as u32);
                mov_abs(&mut a, 16, h_shift as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
            }
            JitOp::ShiftRC { kind, reg } => {
                load_reg(&mut a, l, 3, 1); // CL = regs[1]の下位
                dynasm!(a; .arch aarch64; and w3, w3, 0xff);
                mov_abs(&mut a, 0, machine as u64);
                mov_imm32(&mut a, 1, kind as u32);
                mov_imm32(&mut a, 2, reg as u32);
                mov_abs(&mut a, 16, h_shift as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
            }
            JitOp::Jcc { cc, rel } => {
                // 終端: ip = ip0 + (成立 ? off+len+rel : off+len)、n命令実行
                mov_abs(&mut a, 0, machine as u64);
                mov_imm32(&mut a, 1, cc as u32);
                mov_abs(&mut a, 16, h_cond as usize as u64);
                dynasm!(a; .arch aarch64; blr x16);
                let not_taken = off.wrapping_add(len as u32);
                let taken = not_taken.wrapping_add(rel);
                mov_imm32(&mut a, 10, taken);
                mov_imm32(&mut a, 11, not_taken);
                dynasm!(a; .arch aarch64; cmp w0, 0; csel w10, w10, w11, ne);
                return finish(a, l, n, FinishIp::Delta10);
            }
            JitOp::Jmp { rel } => {
                let taken = off.wrapping_add(len as u32).wrapping_add(rel);
                mov_imm32(&mut a, 10, taken);
                return finish(a, l, n, FinishIp::Delta10);
            }
            _ => return None, // レジスタ語彙の外 (reg_only_prefixが弾くはずの網)
        }
        off = off.wrapping_add(len as u32);
    }
    // 非終端 (語彙外の手前・cap) で終わるブロック: 直線の着地
    mov_imm32(&mut a, 10, total_len);
    finish(a, l, n, FinishIp::Delta10)
}

enum FinishIp {
    /// w10 = ip0 に足す差分 (直線 or 分岐先)
    Delta10,
}

/// epilogue: ip = ip0 + w10 を書き、実行命令数 n を返す
fn finish(
    mut a: dynasmrt::aarch64::Assembler,
    l: &JitLayout,
    n: u16,
    _ip: FinishIp,
) -> Option<Block> {
    let n32 = n as u32;
    mov_abs(&mut a, 9, l.ip as u64);
    dynasm!(a; .arch aarch64
        ; ldr w11, [x9]
        ; add w11, w11, w10
        ; str w11, [x9]
        ; movz w0, n32
        ; ldp x29, x30, [sp]
        ; add sp, sp, 16
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
    })
}

/// w10=a, w11=b から w13=r と w12=cin を作る (kind = ALUグリッドの演算番号)。
/// フラグは作らない — 材料を store_cc が書く (意味論の原本は cc_cf/cc_of)
fn emit_alu(
    a: &mut dynasmrt::aarch64::Assembler,
    _l: &JitLayout,
    machine: usize,
    kind: u8,
) -> Option<()> {
    match kind {
        0 => dynasm!(a; .arch aarch64; movz w12, 0; add w13, w10, w11), // ADD
        1 => dynasm!(a; .arch aarch64; movz w12, 0; orr w13, w10, w11), // OR
        2 => {
            // ADC: cin = 今のCF (遅延中なら1bit計算)。**materialの前に**読む
            call_cf(a, machine, 12);
            // ヘルパがw10/w11を壊すので取り直しはしない — call_cfはw0/x9/x16系
            // しか触らない約束…は守れない (ABI上caller-savedは全部死ぬ)。
            // なのでADC/SBBだけは a/b をヘルパの後でロードし直す構成にする
            return None; // ← F1d-aでは語彙から外す (下のreject。台帳: F1d-bでa/b再ロード形に)
        }
        3 => return None,                                               // SBB 同上
        4 => dynasm!(a; .arch aarch64; movz w12, 0; and w13, w10, w11), // AND
        5 | 7 => dynasm!(a; .arch aarch64; movz w12, 0; sub w13, w10, w11), // SUB/CMP
        6 => dynasm!(a; .arch aarch64; movz w12, 0; eor w13, w10, w11), // XOR
        _ => return None,
    }
    Some(())
}
