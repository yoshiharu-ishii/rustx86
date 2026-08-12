//! F1c-a: ネイティブJITランナー (ADR-0012) — JitOp列をCraneliftで焼く。
//!
//! wasm生成器 (wasm/src/jit.rs) と**同じ意味論契約**をCLIFで写す:
//! - フラグは lazy flags の材料 (cc_*) をメモリへ書くだけ (評価器を二重実装しない —
//!   CF/条件はヘルパ呼び)
//! - メモリ・スタック形は core の `jit_try_*` ヘルパ — フォールトしそうなら
//!   **状態を1つも変えずに脱出**、返り値=完全実行数、core側がその1命令をやり直す
//! - ipの更新はブロック出口で1回。tsc/tick の清算は core 側 (契約は増分3)
//!
//! wasmと違うのは2点だけ:
//! - 生成先が wasmバイト列 → CLIF (関数ポインタ)
//! - 焼きが**背景スレッド** — 53〜276µs/ブロックの焼き代を実行の裏に隠す
//!   (関門プローブの結論)。据え付けはメインスレッドがスライス境界で行い、
//!   collect時のページ世代と照合してから — 世代が動いていたら捨てる
//!
//! ## 生成コードとMachineの別名参照について
//!
//! 生成コードはMachineのフィールド実番地 (jit::layout) を直接読み書きし、
//! ヘルパはMachineの生ポインタから参照を作る。呼び出し元 (step_cached) の
//! `&mut Machine` と重なるが、入口は**必ず不透明な関数ポインタ (JitHook.enter)**
//! なのでコンパイラは呼び出しをまたいだ別名最適化をできない — wasm側が
//! 共有リニアメモリ越しにやっていた事と同じ構図をネイティブに写した形。
//! この前提が壊れる書き方 (enterのインライン化・LTOでの貫通) をしないこと。

// ヘルパの関数アドレスを生成コードへ定数として焼き込む — このキャストは
// 「番地が欲しい」そのものなので、clippyの心配 (誤ってfnを数値にした?) は当たらない
#![allow(clippy::fn_to_numeric_cast, clippy::fn_to_numeric_cast_any)]
#![allow(unknown_lints, function_casts_as_integer)]

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Signature, Value};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use rustx86_core::jit::{JitBlock, JitLayout, JitMem, JitOp};
use rustx86_core::{cpu, jit, Machine};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver, Sender};

// ---- ヘルパ (生成コードから呼ばれる) ----
//
// wasmシェルの rx86_jit_* と同じ体。引数のMachineは生成時に焼き込んだ実番地。

unsafe extern "C" fn h_cf(m: *const Machine) -> i32 {
    (*m).cpu.flag(cpu::CF) as i32
}

unsafe extern "C" fn h_cond(m: *const Machine, cc: i32) -> i32 {
    cpu::alu::condition(&(*m).cpu, cc as u8) as i32
}

/// 成功 = 値 (上位32bit=0) / 脱出 = 1<<32
unsafe extern "C" fn h_ld32(m: *const Machine, seg: i32, off: i32) -> i64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off as u32);
    match m.jit_try_read32(la) {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

/// 1 = 完了 / 0 = 脱出 (何も書いていない)
unsafe extern "C" fn h_st32(m: *mut Machine, seg: i32, off: i32, val: i32) -> i32 {
    let m = &mut *m;
    let la = m.cpu.lin(seg as usize, off as u32);
    m.jit_try_write32(la, val as u32) as i32
}

/// 1 = 完了 (ccとメモリ更新済み) / 0 = 脱出 (状態は無傷)
unsafe extern "C" fn h_rmw32(m: *mut Machine, seg: i32, off: i32, kind: i32, b: i32) -> i32 {
    let m = &mut *m;
    let la = m.cpu.lin(seg as usize, off as u32);
    m.jit_try_rmw32(la, kind as u8, b as u32) as i32
}

unsafe extern "C" fn h_push32(m: *mut Machine, val: i32) -> i32 {
    (*m).jit_try_push32(val as u32) as i32
}

/// 成功 = 値 / 脱出 = 1<<32 (SP不変)
unsafe extern "C" fn h_pop32(m: *mut Machine) -> i64 {
    match (*m).jit_try_pop32() {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

unsafe extern "C" fn h_leave(m: *mut Machine) -> i32 {
    (*m).jit_try_leave() as i32
}

// ---- 語彙v2 (F1c-b2) のヘルパ ----

/// shift/rot r32。フラグはshift_rot (意味論の原本) の中で完結。#PF不能
unsafe extern "C" fn h_shift_r(m: *mut Machine, kind: i32, reg: i32, count: i32) {
    let m = &mut *m;
    let a = m.cpu.regs[reg as usize];
    let v = cpu::shift::shift_rot(&mut m.cpu, kind as u8, a, count as u8, 32);
    m.cpu.regs[reg as usize] = v;
}

/// 8bit読み。成功 = 値 / 脱出 = 1<<32
unsafe extern "C" fn h_ld8(m: *const Machine, seg: i32, off: i32) -> i64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off as u32);
    match m.jit_try_read8(la) {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

/// 16bit読み。成功 = 値 / 脱出 = 1<<32 (跨ぎ含む)
unsafe extern "C" fn h_ld16(m: *const Machine, seg: i32, off: i32) -> i64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off as u32);
    match m.jit_try_read16(la) {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

/// F6 kind0-3 (test imm/not/neg) のレジスタ形。NEGのCF上書きが遅延材料に
/// 畳めないので、インタプリタ (dcache exec の Grp3b) をそのまま写す。#PF不能
unsafe extern "C" fn h_grp3b8_r(m: *mut Machine, kind: i32, reg: i32, imm: i32) {
    let m = &mut *m;
    let r = reg as usize;
    let a = if r < 4 {
        m.cpu.regs[r] as u8
    } else {
        (m.cpu.regs[r - 4] >> 8) as u8
    };
    let set8 = |m: &mut Machine, v: u8| {
        if r < 4 {
            m.cpu.regs[r] = (m.cpu.regs[r] & !0xFF) | v as u32;
        } else {
            m.cpu.regs[r - 4] = (m.cpu.regs[r - 4] & !0xFF00) | (v as u32) << 8;
        }
    };
    match kind {
        0 | 1 => {
            cpu::alu::alu8(&mut m.cpu, 4, a, imm as u8);
        }
        2 => set8(m, !a),
        _ => {
            let v = cpu::alu::alu8(&mut m.cpu, 5, 0, a);
            m.cpu.set_flag(cpu::CF, a != 0);
            set8(m, v);
        }
    }
}

/// ブロック関数の形: 引数なし → 実行した命令数 (全数未満 = フォールト脱出)
pub type BlockFn = unsafe extern "C" fn() -> u32;

// ---- 据え付けテーブル (メインスレッド専用) ----
//
// enter は JitHook の契約で素の fn ポインタ — 状態はこのスレッドローカルが持つ。
// 据え付け (push) も実行 (enter) もメインスレッドだけが触る

thread_local! {
    static TABLE: RefCell<Vec<BlockFn>> = const { RefCell::new(Vec::new()) };
}

/// coreのJitHookに挿す実行口
pub fn enter(slot: u32) -> u32 {
    TABLE.with(|t| {
        let f = t.borrow()[slot as usize];
        unsafe { f() }
    })
}

fn table_push(f: BlockFn) -> u32 {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        t.push(f);
        (t.len() - 1) as u32
    })
}

// ---- 翻訳器 (JitOp → CLIF) ----

/// ヘルパのシグネチャ一式 (関数ごとに import する)
struct Sigs {
    cf: cranelift_codegen::ir::SigRef,
    cond: cranelift_codegen::ir::SigRef,
    ld32: cranelift_codegen::ir::SigRef,
    st32: cranelift_codegen::ir::SigRef,
    rmw32: cranelift_codegen::ir::SigRef,
    push32: cranelift_codegen::ir::SigRef,
    pop32: cranelift_codegen::ir::SigRef,
    leave: cranelift_codegen::ir::SigRef,
    /// (m, i32, i32, i32) -> なし相当 — Craneliftは戻り値必須ではないが
    /// 統一のためi32を返させず、戻り無しシグネチャで呼ぶ
    quad_void: cranelift_codegen::ir::SigRef,
}

fn sig(call_conv: CallConv, params: &[types::Type], ret: types::Type) -> Signature {
    let mut s = Signature::new(call_conv);
    for &p in params {
        s.params.push(AbiParam::new(p));
    }
    s.returns.push(AbiParam::new(ret));
    s
}

fn sig_void(call_conv: CallConv, params: &[types::Type]) -> Signature {
    let mut s = Signature::new(call_conv);
    for &p in params {
        s.params.push(AbiParam::new(p));
    }
    s
}

/// 1ブロックぶんの生成の作業場 (wasm側の Gen と同じ役割)
struct Tr<'a, 'b> {
    fb: &'a mut FunctionBuilder<'b>,
    lay: &'a JitLayout,
    sigs: &'a Sigs,
    /// Machineの実番地 (ヘルパ第1引数)
    maddr: i64,
    /// 今の命令の手前までに完全実行し終えた命令数 (脱出の返り値)
    cur_k: u32,
    /// ブロック頭から今の命令までのバイトオフセット (脱出時のip合わせ)
    cur_ip_off: u32,
    /// エントリで読んだ jit_budget (このブロック実行の最大命令数) — F1c-c4
    budget: Option<Value>,
    /// ブロック内レジスタ割付 (F1c-d3): GPRをCranelift変数 (= ホストレジスタ) に
    /// 載せる。liveビット = 変数が現値を持つ (初回読みで遅延ロード)。
    /// dirtyビット = 変数がメモリより新しい — **全脱出点・全終端・GPRを触る
    /// ヘルパの前**で書き戻すのが規律 (脱出モデルとの整合はこれで保つ)
    live: u8,
    dirty: u8,
    /// cc遅延化 (F1c-d4): ALUのcc材料6ストアを消費点まで遅延する。
    /// 消費点 = 脱出・h_cond・call_cf・ccを触るヘルパ・終端。
    /// 後続ALUが上書きすれば6ストアは丸ごと消える (デッドストア除去)
    cc: Option<PendCc>,
}

/// 遅延中のcc材料 (op/wはcompile-time定数、値はSSA Value)
#[derive(Clone, Copy)]
struct PendCc {
    op: u8,
    w: u8,
    a: Value,
    b: Value,
    cin: Value,
    r: Value,
}

const F: MemFlags = MemFlags::trusted();

impl Tr<'_, '_> {
    fn c32(&mut self, v: u32) -> Value {
        self.fb.ins().iconst(types::I32, v as i32 as i64)
    }
    fn addr(&mut self, a: usize) -> Value {
        self.fb.ins().iconst(types::I64, a as i64)
    }
    fn m_ptr(&mut self) -> Value {
        self.fb.ins().iconst(types::I64, self.maddr)
    }
    fn ld32_at(&mut self, a: usize) -> Value {
        let p = self.addr(a);
        self.fb.ins().load(types::I32, F, p, 0)
    }
    fn st32_at(&mut self, a: usize, v: Value) {
        let p = self.addr(a);
        self.fb.ins().store(F, v, p, 0);
    }
    fn st8_at(&mut self, a: usize, v: u8) {
        let p = self.addr(a);
        let c = self.c32(v as u32);
        self.fb.ins().istore8(F, c, p, 0);
    }
    fn reg(&mut self, r: u8) -> Value {
        let var = Variable::from_u32(r as u32);
        if self.live & (1 << r) == 0 {
            let mv = self.ld32_at(self.lay.regs + 4 * r as usize);
            self.fb.def_var(var, mv);
            self.live |= 1 << r;
        }
        self.fb.use_var(var)
    }
    fn set_reg(&mut self, r: u8, v: Value) {
        self.fb.def_var(Variable::from_u32(r as u32), v);
        self.live |= 1 << r;
        self.dirty |= 1 << r;
    }
    /// dirtyなGPRを**今いるブロックに**書き戻すストアを吐く。compile-time状態は
    /// 変えない — 脱出側ブロック専用 (fall-through側では引き続きdirty)
    fn flush_here(&mut self) {
        let d = self.dirty;
        for r in 0..8u8 {
            if d & (1 << r) != 0 {
                let v = self.fb.use_var(Variable::from_u32(r as u32));
                self.st32_at(self.lay.regs + 4 * r as usize, v);
            }
        }
    }
    /// maskのdirtyなGPRを書き戻してdirty解除 (GPRを読むヘルパの前・終端用)
    fn flush_regs(&mut self, mask: u8) {
        let d = self.dirty & mask;
        for r in 0..8u8 {
            if d & (1 << r) != 0 {
                let v = self.fb.use_var(Variable::from_u32(r as u32));
                self.st32_at(self.lay.regs + 4 * r as usize, v);
            }
        }
        self.dirty &= !mask;
    }
    /// GPRを書くヘルパの後で変数を無効化 (次の読みはメモリから)
    fn invalidate(&mut self, mask: u8) {
        self.live &= !mask;
        self.dirty &= !mask;
    }

    /// 遅延中のccを**今いるブロックに**書き戻す。compile-time状態は変えない —
    /// 脱出側ブロック専用 (fall-through側では引き続き遅延中)
    fn flush_cc_here(&mut self) {
        if let Some(c) = self.cc {
            self.st8_at(self.lay.cc_op, c.op);
            self.st8_at(self.lay.cc_w, c.w);
            self.st32_at(self.lay.cc_a, c.a);
            self.st32_at(self.lay.cc_b, c.b);
            self.st32_at(self.lay.cc_cin, c.cin);
            self.st32_at(self.lay.cc_r, c.r);
        }
    }
    /// 遅延中のccを書き戻して確定する (消費点用)
    fn flush_cc(&mut self) {
        self.flush_cc_here();
        self.cc = None;
    }
    fn set_cc(&mut self, op: u8, w: u8, a: Value, b: Value, cin: Value, r: Value) {
        self.cc = Some(PendCc { op, w, a, b, cin, r });
    }
    fn helper1(&mut self, sr: cranelift_codegen::ir::SigRef, f: usize, args: &[Value]) -> Value {
        let callee = self.fb.ins().iconst(types::I64, f as i64);
        let call = self.fb.ins().call_indirect(sr, callee, args);
        self.fb.inst_results(call)[0]
    }
    fn helper0(&mut self, sr: cranelift_codegen::ir::SigRef, f: usize, args: &[Value]) {
        let callee = self.fb.ins().iconst(types::I64, f as i64);
        self.fb.ins().call_indirect(sr, callee, args);
    }

    // ---- 8bitレジスタ (AH形: 4..7は regs[r-4] のバイト1) ----
    fn reg8v(&mut self, r: u8) -> Value {
        if r < 4 {
            let v = self.reg(r);
            self.fb.ins().band_imm(v, 0xFF)
        } else {
            let v = self.reg(r - 4);
            let s = self.fb.ins().ushr_imm(v, 8);
            self.fb.ins().band_imm(s, 0xFF)
        }
    }
    fn set_reg8v(&mut self, r: u8, v: Value) {
        if r < 4 {
            let cur = self.reg(r);
            let hi = self.fb.ins().band_imm(cur, !0xFFi64);
            let n = self.fb.ins().bor(hi, v);
            self.set_reg(r, n);
        } else {
            let cur = self.reg(r - 4);
            let keep = self.fb.ins().band_imm(cur, !0xFF00i64);
            let sh = self.fb.ins().ishl_imm(v, 8);
            let n = self.fb.ins().bor(keep, sh);
            self.set_reg(r - 4, n);
        }
    }

    /// 8bit ALU共通部 (alu_lazyのw=0を写す): 演算 (0xFFマスク) + cc材料 (cc_w=0)。
    /// 返り値 = r。dstへの書き戻しは呼ぶ側 (kind7は書かない約束も呼ぶ側)
    fn alu8_core(&mut self, kind: u8, a: Value, b: Value) -> Value {
        let cin = if kind == 2 || kind == 3 {
            self.flush_cc(); // call_cf (h_cf) はメモリのccから計算する
            self.call_cf()
        } else {
            self.c32(0)
        };
        let r0 = match kind {
            0 | 2 => {
                let s = self.fb.ins().iadd(a, b);
                self.fb.ins().iadd(s, cin)
            }
            1 => self.fb.ins().bor(a, b),
            3 | 5 | 7 => {
                let s = self.fb.ins().isub(a, b);
                self.fb.ins().isub(s, cin)
            }
            4 => self.fb.ins().band(a, b),
            _ => self.fb.ins().bxor(a, b),
        };
        let r = self.fb.ins().band_imm(r0, 0xFF);
        self.set_cc(kind, 0, a, b, cin, r);
        r
    }

    /// 8bitロード: TLBヒットならインライン (跨ぎ検査不要 — 1バイト)、外れたらヘルパ
    fn emit_ld8(&mut self, mem: &JitMem) -> Value {
        let (off, la) = self.lin_val(mem);
        let (pa, slow) = self.tlb_probe8(la);
        let slow_b = self.fb.create_block();
        let fast_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.append_block_param(cont, types::I32);
        self.fb.ins().brif(slow, slow_b, &[], fast_b, &[]);
        self.fb.switch_to_block(fast_b);
        self.fb.seal_block(fast_b);
        let pa64 = self.fb.ins().uextend(types::I64, pa);
        let membase = self.addr(self.lay.mem);
        let p = self.fb.ins().iadd(membase, pa64);
        let v = self.fb.ins().uload8(types::I32, F, p, 0);
        self.fb.ins().jump(cont, &[v]);
        self.fb.switch_to_block(slow_b);
        self.fb.seal_block(slow_b);
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let v64 = self.helper1(self.sigs.ld32, h_ld8 as usize, &[m, seg, off]);
        let sv = self.check_v64(v64);
        self.fb.ins().jump(cont, &[sv]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
        self.fb.block_params(cont)[0]
    }

    /// 8bit用のTLB判定 (跨ぎ検査なし、RAM境界は+0)
    fn tlb_probe8(&mut self, la: Value) -> (Value, Value) {
        use cranelift_codegen::ir::condcodes::IntCC;
        let lo = self.fb.ins().band_imm(la, 0xFFF);
        let vpn = self.fb.ins().ushr_imm(la, 12);
        let slot = self.fb.ins().band_imm(vpn, (self.lay.tlb_slots - 1) as i64);
        let slot64 = self.fb.ins().uextend(types::I64, slot);
        let stride = self.fb.ins().imul_imm(slot64, 12);
        let tlb = self.addr(self.lay.tlb);
        let e = self.fb.ins().iadd(tlb, stride);
        let tag = self.fb.ins().load(types::I32, F, e, 0);
        let miss = self.fb.ins().icmp(IntCC::NotEqual, tag, vpn);
        let bf = self.fb.ins().load(types::I32, F, e, 4);
        let got = self.fb.ins().band_imm(bf, rustx86_core::jit::TLB_U as i64);
        let noperm = self.fb.ins().icmp_imm(IntCC::Equal, got, 0);
        let mask = self.c32(0xFFFF_F000);
        let base = self.fb.ins().band(bf, mask);
        let pa = self.fb.ins().bor(base, lo);
        let oob = self.fb.ins().icmp_imm(
            IntCC::UnsignedGreaterThanOrEqual,
            pa,
            self.lay.mem_len as i64,
        );
        let s1 = self.fb.ins().bor(miss, noperm);
        let slow = self.fb.ins().bor(s1, oob);
        (pa, slow)
    }

    /// 16bitロード: ヘルパのみ (頻度2M — インライン化は分布が要求してから)
    fn emit_ld16(&mut self, mem: &JitMem) -> Value {
        let (off, _la) = self.lin_val(mem);
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let v64 = self.helper1(self.sigs.ld32, h_ld16 as usize, &[m, seg, off]);
        self.check_v64(v64)
    }
    fn call_cf(&mut self) -> Value {
        let m = self.m_ptr();
        self.helper1(self.sigs.cf, h_cf as usize, &[m])
    }

    /// 実効オフセット (disp + base + (index<<scale)) — off_of と同じ形
    fn eff_off(&mut self, mem: &JitMem) -> Value {
        let mut v = self.c32(mem.disp);
        if mem.base >= 0 {
            let b = self.reg(mem.base as u8);
            v = self.fb.ins().iadd(v, b);
        }
        if mem.index >= 0 {
            let i = self.reg(mem.index as u8);
            let s = self.fb.ins().ishl_imm(i, mem.scale as i64);
            v = self.fb.ins().iadd(v, s);
        }
        v
    }

    /// 脱出の本体: ip = ip + cur_ip_off、返り値 = cur_k。
    /// `cond_escape` が真なら脱出、偽なら続行 — 続行側のブロックへ切り替えて戻る
    fn escape_if(&mut self, cond_escape: Value) {
        let esc = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.ins().brif(cond_escape, esc, &[], cont, &[]);
        self.fb.switch_to_block(esc);
        self.fb.seal_block(esc);
        // 脱出 = interpが「実行済みk命令のメモリ状態」から再開する — dirtyを実体化
        self.flush_here();
        self.flush_cc_here();
        let ip = self.ld32_at(self.lay.ip);
        let off = self.c32(self.cur_ip_off);
        let nip = self.fb.ins().iadd(ip, off);
        self.st32_at(self.lay.ip, nip);
        let k = self.c32(self.cur_k);
        self.fb.ins().return_(&[k]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
    }

    /// 予算ガード (F1c-c4): op i の手前で i >= jit_budget なら途中退出。
    /// 退出はescapeと同じ機構 (ip=頭+off、返り値=i) だが、こちらは
    /// **完全実行済みの正規出口** — tickがちょうどここで刻まれる
    fn budget_guard(&mut self, i: u32) {
        use cranelift_codegen::ir::condcodes::IntCC;
        let Some(b) = self.budget else { return };
        let hit = self
            .fb
            .ins()
            .icmp_imm(IntCC::UnsignedLessThanOrEqual, b, i as i64);
        self.escape_if(hit);
    }

    /// 途中のjcc (F1c-c): 成立なら ip = ip + (jcc末尾までのオフセット + rel) を
    /// 書いて k+1 (このjcc込みの完全実行数) で退出。不成立なら素通り。
    /// 脱出 (escape_if) と同じ形だが、こちらは**やり直し不要の正規の出口**
    fn emit_jcc_mid(&mut self, cc: u8, rel: u32, len: u32) {
        self.flush_cc(); // h_condはメモリのccから判定する
        let m = self.m_ptr();
        let c = self.c32(cc as u32);
        let cond = self.helper1(self.sigs.cond, h_cond as usize, &[m, c]);
        let taken = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.ins().brif(cond, taken, &[], cont, &[]);
        self.fb.switch_to_block(taken);
        self.fb.seal_block(taken);
        // 正規の途中退出もメモリ状態を実体化してから戻る
        self.flush_here();
        let ip = self.ld32_at(self.lay.ip);
        let d = self.c32(self.cur_ip_off.wrapping_add(len).wrapping_add(rel));
        let nip = self.fb.ins().iadd(ip, d);
        self.st32_at(self.lay.ip, nip);
        let k = self.c32(self.cur_k + 1);
        self.fb.ins().return_(&[k]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
    }

    /// i64返しヘルパ (ld32/pop32) の裁き: 上位32bitが立っていたら脱出。
    /// 成功時は下位32bitの値を返す
    fn check_v64(&mut self, v64: Value) -> Value {
        let hi = self.fb.ins().ushr_imm(v64, 32);
        let hi = self.fb.ins().ireduce(types::I32, hi);
        self.escape_if(hi);
        self.fb.ins().ireduce(types::I32, v64)
    }

    /// i32返しヘルパ (1=完了/0=脱出) の裁き
    fn check_ok(&mut self, ok: Value) {
        let z = self
            .fb
            .ins()
            .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, ok, 0);
        self.escape_if(z);
    }

    /// 実効オフセットとリニアアドレス (seg baseは実行時に隠しレジスタから読む)
    fn lin_val(&mut self, mem: &JitMem) -> (Value, Value) {
        let off = self.eff_off(mem);
        let sbase = self.ld32_at(self.lay.hidden + 12 * mem.seg as usize);
        let la = self.fb.ins().iadd(off, sbase);
        (off, la)
    }

    /// TLB高速路の判定部 (F1c-b)。la から (pa, 遅い道へ落ちる条件) を作る。
    /// `need` = base_flags に要求する旗 (読み: U / 書き: W|U|D)。
    /// 条件はインタプリタの translate_for + jit_try_* を**保守的に**写す —
    /// 高速路が通る場合に限り意味が一致し、迷いは全部ヘルパ (原本) へ落とす:
    /// ページ跨ぎ・TLBミス・旗不足・RAM外はヘルパ行き
    fn tlb_probe(&mut self, la: Value, need: u32) -> (Value, Value) {
        use cranelift_codegen::ir::condcodes::IntCC;
        let lo = self.fb.ins().band_imm(la, 0xFFF);
        let cross = self
            .fb
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, lo, 0xFFC);
        let vpn = self.fb.ins().ushr_imm(la, 12);
        let slot = self.fb.ins().band_imm(vpn, (self.lay.tlb_slots - 1) as i64);
        let slot64 = self.fb.ins().uextend(types::I64, slot);
        let stride = self.fb.ins().imul_imm(slot64, 12);
        let tlb = self.addr(self.lay.tlb);
        let e = self.fb.ins().iadd(tlb, stride);
        let tag = self.fb.ins().load(types::I32, F, e, 0);
        let miss = self.fb.ins().icmp(IntCC::NotEqual, tag, vpn);
        let bf = self.fb.ins().load(types::I32, F, e, 4);
        let got = self.fb.ins().band_imm(bf, need as i64);
        let noperm = self.fb.ins().icmp_imm(IntCC::NotEqual, got, need as i64);
        let mask = self.c32(0xFFFF_F000);
        let base = self.fb.ins().band(bf, mask);
        let pa = self.fb.ins().bor(base, lo);
        let oob = self.fb.ins().icmp_imm(
            IntCC::UnsignedGreaterThan,
            pa,
            (self.lay.mem_len as i64) - 4,
        );
        let s1 = self.fb.ins().bor(cross, miss);
        let s2 = self.fb.ins().bor(s1, noperm);
        let slow = self.fb.ins().bor(s2, oob);
        (pa, slow)
    }

    /// 書き込み高速路の追加条件: テキストVRAM窓は遅い道 (vram_dirtyの約束)
    fn vram_hit(&mut self, pa: Value) -> Value {
        use cranelift_codegen::ir::condcodes::IntCC;
        let hi3 = self.fb.ins().iadd_imm(pa, 3);
        let a = self.fb.ins().icmp_imm(
            IntCC::UnsignedGreaterThanOrEqual,
            hi3,
            self.lay.vram_lo as i64,
        );
        let b = self
            .fb
            .ins()
            .icmp_imm(IntCC::UnsignedLessThanOrEqual, pa, self.lay.vram_hi as i64);
        self.fb.ins().band(a, b)
    }

    /// ゲストRAMの pa から直接 i32 を読む
    fn ram_ld32(&mut self, pa: Value) -> Value {
        let pa64 = self.fb.ins().uextend(types::I64, pa);
        let membase = self.addr(self.lay.mem);
        let p = self.fb.ins().iadd(membase, pa64);
        self.fb.ins().load(types::I32, F, p, 0)
    }

    fn ram_st32(&mut self, pa: Value, v: Value) {
        let pa64 = self.fb.ins().uextend(types::I64, pa);
        let membase = self.addr(self.lay.mem);
        let p = self.fb.ins().iadd(membase, pa64);
        self.fb.ins().store(F, v, p, 0);
    }

    /// 32bitロード: TLBヒットならインライン (F1c-b)、外れたらヘルパ (原本)
    fn emit_ld32(&mut self, mem: &JitMem) -> Value {
        let (off, la) = self.lin_val(mem);
        let (pa, slow) = self.tlb_probe(la, rustx86_core::jit::TLB_U);
        let slow_b = self.fb.create_block();
        let fast_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.append_block_param(cont, types::I32);
        self.fb.ins().brif(slow, slow_b, &[], fast_b, &[]);
        self.fb.switch_to_block(fast_b);
        self.fb.seal_block(fast_b);
        let v = self.ram_ld32(pa);
        self.fb.ins().jump(cont, &[v]);
        self.fb.switch_to_block(slow_b);
        self.fb.seal_block(slow_b);
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let v64 = self.helper1(self.sigs.ld32, h_ld32 as usize, &[m, seg, off]);
        let sv = self.check_v64(v64);
        self.fb.ins().jump(cont, &[sv]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
        self.fb.block_params(cont)[0]
    }

    /// 32bitストア: TLBヒット (W|U|D、VRAM外) ならインライン、外れたらヘルパ
    fn emit_st32(&mut self, mem: &JitMem, v: Value) {
        use rustx86_core::jit::{TLB_D, TLB_U, TLB_W};
        let (off, la) = self.lin_val(mem);
        let (pa, slow0) = self.tlb_probe(la, TLB_W | TLB_U | TLB_D);
        let vr = self.vram_hit(pa);
        let slow = self.fb.ins().bor(slow0, vr);
        let slow_b = self.fb.create_block();
        let fast_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.ins().brif(slow, slow_b, &[], fast_b, &[]);
        self.fb.switch_to_block(fast_b);
        self.fb.seal_block(fast_b);
        self.ram_st32(pa, v);
        self.fb.ins().jump(cont, &[]);
        self.fb.switch_to_block(slow_b);
        self.fb.seal_block(slow_b);
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let ok = self.helper1(self.sigs.st32, h_st32 as usize, &[m, seg, off, v]);
        self.check_ok(ok);
        self.fb.ins().jump(cont, &[]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
    }

    /// RMW (`alu [mem], b`): TLBヒットならロード→alu (ccインライン)→ストア、
    /// 外れたらヘルパ (read→alu_w→writeをRustで完結 — 意味は同一)
    fn emit_rmw32(&mut self, mem: &JitMem, kind: u8, b: Value) {
        use rustx86_core::jit::{TLB_D, TLB_U, TLB_W};
        // 分岐の中でALUする唯一のop。旧pendingは分岐前に実体化し (SSA支配)、
        // 高速路が作る新ccも高速路内で即実体化する — rmwだけ遅延の恩恵なし
        self.flush_cc();
        let (off, la) = self.lin_val(mem);
        let (pa, slow0) = self.tlb_probe(la, TLB_W | TLB_U | TLB_D);
        let vr = self.vram_hit(pa);
        let slow = self.fb.ins().bor(slow0, vr);
        let slow_b = self.fb.create_block();
        let fast_b = self.fb.create_block();
        let cont = self.fb.create_block();
        self.fb.ins().brif(slow, slow_b, &[], fast_b, &[]);
        self.fb.switch_to_block(fast_b);
        self.fb.seal_block(fast_b);
        let a = self.ram_ld32(pa);
        let r = self.alu_core(kind, a, b, None);
        self.flush_cc(); // 高速路内で確定 (contの先ではSSA支配が切れる)
        self.ram_st32(pa, r);
        self.fb.ins().jump(cont, &[]);
        self.fb.switch_to_block(slow_b);
        self.fb.seal_block(slow_b);
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let k = self.c32(kind as u32);
        let ok = self.helper1(self.sigs.rmw32, h_rmw32 as usize, &[m, seg, off, k, b]);
        self.check_ok(ok);
        self.fb.ins().jump(cont, &[]);
        self.fb.switch_to_block(cont);
        self.fb.seal_block(cont);
    }

    /// ALU共通部: 演算 + cc材料の書き出し + (kind7以外) dstへ格納。
    /// 返り値 = 結果r (RMWインラインがメモリへ書き戻すのに使う)
    fn alu_core(&mut self, kind: u8, a: Value, b: Value, dst: Option<u8>) -> Value {
        let cin = if kind == 2 || kind == 3 {
            self.flush_cc(); // call_cf (h_cf) はメモリのccから計算する
            self.call_cf()
        } else {
            self.c32(0)
        };
        let r = match kind {
            0 | 2 => {
                let s = self.fb.ins().iadd(a, b);
                self.fb.ins().iadd(s, cin)
            }
            1 => self.fb.ins().bor(a, b),
            3 | 5 | 7 => {
                let s = self.fb.ins().isub(a, b);
                self.fb.ins().isub(s, cin)
            }
            4 => self.fb.ins().band(a, b),
            _ => self.fb.ins().bxor(a, b), // 6 = XOR
        };
        self.set_cc(kind, 2, a, b, cin, r);
        if kind != 7 {
            if let Some(d) = dst {
                self.set_reg(d, r);
            }
        }
        r
    }

    fn op(&mut self, op: &JitOp) {
        match *op {
            JitOp::MovRI { dst, imm } => {
                let v = self.c32(imm);
                self.set_reg(dst, v);
            }
            JitOp::MovRR { dst, src } => {
                let v = self.reg(src);
                self.set_reg(dst, v);
            }
            JitOp::AluRR { kind, dst, src } => {
                let a = self.reg(dst);
                let b = self.reg(src);
                self.alu_core(kind, a, b, Some(dst));
            }
            JitOp::AluRI { kind, dst, imm } => {
                let a = self.reg(dst);
                let b = self.c32(imm);
                self.alu_core(kind, a, b, Some(dst));
            }
            JitOp::TestRR { a, b } => {
                let av = self.reg(a);
                let bv = self.reg(b);
                self.alu_core(4, av, bv, None);
            }
            JitOp::IncDec { reg, dec } => {
                // CFは不変 — 遅延状態を上書きする前に評価してflagsのbit0へ退避
                self.flush_cc(); // call_cf (h_cf) はメモリのccから計算する
                let cf = self.call_cf();
                let f = self.ld32_at(self.lay.flags);
                let f = self.fb.ins().band_imm(f, !1i64);
                let f = self.fb.ins().bor(f, cf);
                self.st32_at(self.lay.flags, f);
                let a = self.reg(reg);
                let one = self.c32(1);
                let r = if dec {
                    self.fb.ins().isub(a, one)
                } else {
                    self.fb.ins().iadd(a, one)
                };
                let z = self.c32(0);
                self.set_cc(if dec { 9 } else { 8 }, 2, a, one, z, r);
                self.set_reg(reg, r);
            }
            JitOp::Lea {
                dst,
                base,
                index,
                scale,
                disp,
            } => {
                let v = self.eff_off(&JitMem {
                    base,
                    index,
                    scale,
                    seg: 0,
                    disp,
                });
                self.set_reg(dst, v);
            }
            JitOp::MovRM { dst, mem } => {
                let v = self.emit_ld32(&mem);
                self.set_reg(dst, v);
            }
            JitOp::AluRM { kind, dst, mem } => {
                // a = dst / b = メモリ。脱出点はロード = 状態を変える前
                let a = self.reg(dst);
                let b = self.emit_ld32(&mem);
                self.alu_core(kind, a, b, Some(dst));
            }
            JitOp::CmpMR { mem, reg } => {
                let a = self.emit_ld32(&mem);
                let b = self.reg(reg);
                self.alu_core(7, a, b, None);
            }
            JitOp::CmpMI { mem, imm } => {
                let a = self.emit_ld32(&mem);
                let b = self.c32(imm);
                self.alu_core(7, a, b, None);
            }
            JitOp::TestMR { mem, reg } => {
                let a = self.emit_ld32(&mem);
                let b = self.reg(reg);
                self.alu_core(4, a, b, None);
            }
            JitOp::StoreMR { mem, src } => {
                let v = self.reg(src);
                self.emit_st32(&mem, v);
            }
            JitOp::StoreMI { mem, imm } => {
                let v = self.c32(imm);
                self.emit_st32(&mem, v);
            }
            JitOp::AluMR { kind, mem, reg } => {
                let b = self.reg(reg);
                self.emit_rmw32(&mem, kind, b);
            }
            JitOp::AluMI { kind, mem, imm } => {
                let b = self.c32(imm);
                self.emit_rmw32(&mem, kind, b);
            }
            JitOp::PushR { src } => {
                let m = self.m_ptr();
                let v = self.reg(src);
                self.flush_regs(1 << 4); // ヘルパがESPを読み書きする
                let ok = self.helper1(self.sigs.push32, h_push32 as usize, &[m, v]);
                self.invalidate(1 << 4);
                self.check_ok(ok);
            }
            JitOp::PushI { imm } => {
                let m = self.m_ptr();
                let v = self.c32(imm);
                self.flush_regs(1 << 4);
                let ok = self.helper1(self.sigs.push32, h_push32 as usize, &[m, v]);
                self.invalidate(1 << 4);
                self.check_ok(ok);
            }
            JitOp::PopR { dst } => {
                let m = self.m_ptr();
                self.flush_regs(1 << 4);
                let v64 = self.helper1(self.sigs.pop32, h_pop32 as usize, &[m]);
                self.invalidate(1 << 4);
                let v = self.check_v64(v64);
                // pop esp もこの順で正しい (SP更新→上書き)
                self.set_reg(dst, v);
            }
            JitOp::Leave => {
                let m = self.m_ptr();
                self.flush_regs(0x30); // ヘルパがESP・EBPを読み書きする
                let ok = self.helper1(self.sigs.leave, h_leave as usize, &[m]);
                self.invalidate(0x30);
                self.check_ok(ok);
            }
            JitOp::XchgA { reg } => {
                let a = self.reg(0);
                let r = self.reg(reg);
                self.set_reg(0, r);
                self.set_reg(reg, a);
            }
            // ---- 語彙v2 (F1c-b2) ----
            JitOp::ShiftRI { kind, reg, count } => {
                let m = self.m_ptr();
                let k = self.c32(kind as u32);
                let r = self.c32(reg as u32);
                let c = self.c32(count as u32);
                self.flush_regs(1 << reg); // ヘルパが対象regを読み書きする
                self.flush_cc(); // ヘルパはflags/ccも読み書きする (count=0はflags不変)
                self.helper0(self.sigs.quad_void, h_shift_r as usize, &[m, k, r, c]);
                self.invalidate(1 << reg);
            }
            JitOp::ShiftRC { kind, reg } => {
                // countはCL (ECXの下位8bit) を実行時に読む
                let m = self.m_ptr();
                let k = self.c32(kind as u32);
                let r = self.c32(reg as u32);
                let c = self.reg8v(1);
                self.flush_regs(1 << reg);
                self.flush_cc();
                self.helper0(self.sigs.quad_void, h_shift_r as usize, &[m, k, r, c]);
                self.invalidate(1 << reg);
            }
            JitOp::MovzxBR { dst, src8 } => {
                let v = self.reg8v(src8);
                self.set_reg(dst, v);
            }
            JitOp::MovzxBM { dst, mem } => {
                let v = self.emit_ld8(&mem);
                self.set_reg(dst, v);
            }
            JitOp::MovzxWR { dst, src } => {
                let v = self.reg(src);
                let v = self.fb.ins().band_imm(v, 0xFFFF);
                self.set_reg(dst, v);
            }
            JitOp::MovzxWM { dst, mem } => {
                let v = self.emit_ld16(&mem);
                self.set_reg(dst, v);
            }
            JitOp::Alu8RR { kind, dst8, src8 } => {
                let a = self.reg8v(dst8);
                let b = self.reg8v(src8);
                let r = self.alu8_core(kind, a, b);
                if kind != 7 {
                    self.set_reg8v(dst8, r);
                }
            }
            JitOp::Alu8RI { kind, dst8, imm } => {
                let a = self.reg8v(dst8);
                let b = self.c32(imm as u32);
                let r = self.alu8_core(kind, a, b);
                if kind != 7 {
                    self.set_reg8v(dst8, r);
                }
            }
            JitOp::Alu8RM { kind, dst8, mem } => {
                let a = self.reg8v(dst8);
                let b = self.emit_ld8(&mem);
                let r = self.alu8_core(kind, a, b);
                if kind != 7 {
                    self.set_reg8v(dst8, r);
                }
            }
            JitOp::Cmp8MR { mem, reg8 } => {
                let a = self.emit_ld8(&mem);
                let b = self.reg8v(reg8);
                self.alu8_core(7, a, b);
            }
            JitOp::Cmp8MI { mem, imm } => {
                let a = self.emit_ld8(&mem);
                let b = self.c32(imm as u32);
                self.alu8_core(7, a, b);
            }
            JitOp::Test8RR { a8, b8 } => {
                let a = self.reg8v(a8);
                let b = self.reg8v(b8);
                self.alu8_core(4, a, b);
            }
            JitOp::Test8MR { mem, reg8 } => {
                let a = self.emit_ld8(&mem);
                let b = self.reg8v(reg8);
                self.alu8_core(4, a, b);
            }
            JitOp::Grp3b8R { kind, reg8, imm } => {
                let m = self.m_ptr();
                let k = self.c32(kind as u32);
                let r = self.c32(reg8 as u32);
                let i = self.c32(imm as u32);
                // ヘルパはreg8の土台 (AH形は r-4) を読み書きする
                let backing = if reg8 < 4 { reg8 } else { reg8 - 4 };
                self.flush_regs(1 << backing);
                self.flush_cc();
                self.helper0(self.sigs.quad_void, h_grp3b8_r as usize, &[m, k, r, i]);
                self.invalidate(1 << backing);
            }
            JitOp::Mov8RR { dst8, src8 } => {
                let v = self.reg8v(src8);
                self.set_reg8v(dst8, v);
            }
            JitOp::Mov8RM { dst8, mem } => {
                let v = self.emit_ld8(&mem);
                self.set_reg8v(dst8, v);
            }
            JitOp::Jcc { .. } | JitOp::Jmp { .. } | JitOp::CallRel { .. } | JitOp::Ret => {
                unreachable!("終端はtranslateの出口で扱う")
            }
        }
    }
}

/// 1ブロックをCLIFに翻訳して関数として定義する (finalizeは呼ぶ側がまとめて)
fn translate(
    module: &mut JITModule,
    fbc: &mut FunctionBuilderContext,
    blk: &JitBlock,
    lay: &JitLayout,
    maddr: i64,
    name: &str,
) -> cranelift_module::FuncId {
    let mut ctx = module.make_context();
    let cc = module.target_config().default_call_conv;
    ctx.func.signature.returns.push(AbiParam::new(types::I32));

    let mut fb = FunctionBuilder::new(&mut ctx.func, fbc);
    // GPR 8本の変数 (F1c-d3 ブロック内レジスタ割付)
    for r in 0..8u32 {
        fb.declare_var(Variable::from_u32(r), types::I32);
    }
    let entry = fb.create_block();
    fb.switch_to_block(entry);
    fb.seal_block(entry);

    let sigs = Sigs {
        cf: fb.import_signature(sig(cc, &[types::I64], types::I32)),
        cond: fb.import_signature(sig(cc, &[types::I64, types::I32], types::I32)),
        ld32: fb.import_signature(sig(cc, &[types::I64, types::I32, types::I32], types::I64)),
        st32: fb.import_signature(sig(
            cc,
            &[types::I64, types::I32, types::I32, types::I32],
            types::I32,
        )),
        rmw32: fb.import_signature(sig(
            cc,
            &[types::I64, types::I32, types::I32, types::I32, types::I32],
            types::I32,
        )),
        push32: fb.import_signature(sig(cc, &[types::I64, types::I32], types::I32)),
        pop32: fb.import_signature(sig(cc, &[types::I64], types::I64)),
        leave: fb.import_signature(sig(cc, &[types::I64], types::I32)),
        quad_void: fb.import_signature(sig_void(
            cc,
            &[types::I64, types::I32, types::I32, types::I32],
        )),
    };

    let mut tr = Tr {
        fb: &mut fb,
        lay,
        sigs: &sigs,
        maddr,
        cur_k: 0,
        cur_ip_off: 0,
        budget: None,
        live: 0,
        dirty: 0,
        cc: None,
    };
    // 予算 (jit_budget) はエントリで1回読む — coreがenter直前に書いている
    let b = tr.ld32_at(lay.jit_budget);
    tr.budget = Some(b);

    // 本体 (wasm側 compile_block と同じ流れ)。終端は出口で
    enum Term {
        Jcc { cc: u8, rel: u32 },
        Jmp { rel: u32 },
        Call { rel: u32 },
        Ret,
    }
    let mut term = None;
    let mut total_len: u32 = 0;
    let n_ops = blk.ops.len();
    for (i, &(len, ref op)) in blk.ops.iter().enumerate() {
        tr.cur_k = i as u32;
        tr.cur_ip_off = total_len;
        total_len += len as u32;
        if i > 0 {
            // 予算ガード (F1c-c4): coreは予算1以上で入場させる — op 0は無条件、
            // 以後はopごとに「ここまでで予算切れなら途中退出」
            tr.budget_guard(i as u32);
        }
        match *op {
            // F1c-c: 末尾のjccは従来の出口 (select)、**途中のjccは条件つき退出** —
            // 成立なら ip=成立先 を書いて k+1 で戻る (完全実行済みの退出)、
            // 不成立なら次のopへそのまま流れる (両側焼き)
            JitOp::Jcc { cc, rel } if i + 1 == n_ops => term = Some(Term::Jcc { cc, rel }),
            JitOp::Jcc { cc, rel } => tr.emit_jcc_mid(cc, rel, len as u32),
            JitOp::Jmp { rel } => term = Some(Term::Jmp { rel }),
            JitOp::CallRel { rel } => term = Some(Term::Call { rel }),
            JitOp::Ret => term = Some(Term::Ret),
            _ => tr.op(op),
        }
    }

    // 出口: 全終端でdirtyなGPRを実体化してから帳尻を合わせる (F1c-d3)。
    // Call/Retのヘルパ (push/pop) がESPを読むのもこのflushが前提
    tr.flush_regs(0xFF);
    tr.flush_cc();
    // ipの帳尻と (call/retなら) スタック操作
    match term {
        None => {
            let ip = tr.ld32_at(tr.lay.ip);
            let d = tr.c32(total_len);
            let nip = tr.fb.ins().iadd(ip, d);
            tr.st32_at(tr.lay.ip, nip);
        }
        Some(Term::Jmp { rel }) => {
            let ip = tr.ld32_at(tr.lay.ip);
            let d = tr.c32(total_len.wrapping_add(rel));
            let nip = tr.fb.ins().iadd(ip, d);
            tr.st32_at(tr.lay.ip, nip);
        }
        Some(Term::Jcc { cc: ccode, rel }) => {
            let m = tr.m_ptr();
            let c = tr.c32(ccode as u32);
            let cond = tr.helper1(tr.sigs.cond, h_cond as usize, &[m, c]);
            let taken = tr.c32(total_len.wrapping_add(rel));
            let not = tr.c32(total_len);
            let d = tr.fb.ins().select(cond, taken, not);
            let ip = tr.ld32_at(tr.lay.ip);
            let nip = tr.fb.ins().iadd(ip, d);
            tr.st32_at(tr.lay.ip, nip);
        }
        Some(Term::Call { rel }) => {
            // 戻り番地 (= ip + total_len) をpush。ここが脱出点
            let m = tr.m_ptr();
            let ip = tr.ld32_at(tr.lay.ip);
            let d = tr.c32(total_len);
            let ret_addr = tr.fb.ins().iadd(ip, d);
            let ok = tr.helper1(tr.sigs.push32, h_push32 as usize, &[m, ret_addr]);
            tr.check_ok(ok);
            let ip2 = tr.ld32_at(tr.lay.ip);
            let d2 = tr.c32(total_len.wrapping_add(rel));
            let nip = tr.fb.ins().iadd(ip2, d2);
            tr.st32_at(tr.lay.ip, nip);
        }
        Some(Term::Ret) => {
            let m = tr.m_ptr();
            let v64 = tr.helper1(tr.sigs.pop32, h_pop32 as usize, &[m]);
            let v = tr.check_v64(v64);
            tr.st32_at(tr.lay.ip, v);
        }
    }

    let n = tr.c32(blk.ops.len() as u32);
    tr.fb.ins().return_(&[n]);
    fb.finalize();

    let id = module
        .declare_function(name, Linkage::Local, &ctx.func.signature)
        .unwrap();
    module.define_function(id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    id
}

// ---- 背景焼き (ADR-0012 決定2) ----

/// 焼き依頼: collect済みブロック列と、collect時点のページ世代
struct BakeJob {
    blocks: Vec<(JitBlock, u32)>,
    lay: JitLayout,
    maddr: usize,
}

/// 焼き上がり: 据え付けに要るものだけ
pub struct Baked {
    pub pa: u32,
    pub n: u16,
    pub gen: u32,
    pub f: usize,
}

fn baker_thread(rx: Receiver<BakeJob>, tx: Sender<Baked>) {
    // モジュールはこのスレッドが持ち続ける (生成コードの生存 = プロセスの生存)
    let mut sb = settings::builder();
    sb.set("opt_level", "speed").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(sb))
        .unwrap();
    let mut module = JITModule::new(JITBuilder::with_isa(
        isa,
        cranelift_module::default_libcall_names(),
    ));
    let mut fbc = FunctionBuilderContext::new();
    let mut seq = 0usize;

    while let Ok(job) = rx.recv() {
        // バッチで定義して finalize は1回 (焼き代の固定部を割る)
        let mut ids = Vec::new();
        for (blk, gen) in &job.blocks {
            seq += 1;
            let id = translate(
                &mut module,
                &mut fbc,
                blk,
                &job.lay,
                job.maddr as i64,
                &format!("b{seq}"),
            );
            ids.push((id, blk.head_pa, blk.ops.len() as u16, *gen));
        }
        if module.finalize_definitions().is_err() {
            continue; // 生成失敗はこのバッチごと捨てる (インタプリタが正)
        }
        for (id, pa, n, gen) in ids {
            let f = module.get_finalized_function(id) as usize;
            if tx.send(Baked { pa, n, gen, f }).is_err() {
                return; // 受け手が居ない = 終了
            }
        }
    }
}

// ---- ランタイム (メインスレッド側の台帳) ----

pub struct JitRt {
    tx: Sender<BakeJob>,
    rx: Receiver<Baked>,
    /// 一度焼きに出したブロック頭 (二度焼かない)。据え付け本体はEntry側
    compiled: HashSet<u32>,
    pub installed: usize,
    pub dropped_stale: usize,
    _thread: std::thread::JoinHandle<()>,
}

impl JitRt {
    pub fn start() -> Self {
        let (tx, jrx) = channel::<BakeJob>();
        let (jtx, rx) = channel::<Baked>();
        let th = std::thread::Builder::new()
            .name("jit-baker".into())
            .spawn(move || baker_thread(jrx, jtx))
            .unwrap();
        JitRt {
            tx,
            rx,
            compiled: HashSet::new(),
            installed: 0,
            dropped_stale: 0,
            _thread: th,
        }
    }

    /// スライス境界で呼ぶ: 熱い頭を収集して焼きへ送り (待たない)、
    /// 焼き上がりを世代照合して据え付ける
    pub fn pump(&mut self, m: &mut Machine) {
        // 収集 → 焼き依頼
        let hot = m.dcache.drain_hot();
        if !hot.is_empty() {
            let lay = jit::layout(m);
            let maddr = m as *const Machine as usize;
            let mut blocks = Vec::new();
            for pa in hot {
                for blk in jit::collect_run_caps(m, pa, 32, 8, jit::CAP_VOCAB2 | jit::CAP_CHAIN) {
                    if blk.ops.len() < 2 {
                        continue; // 1命令ブロックはディスパッチ税で負ける
                    }
                    if !self.compiled.insert(blk.head_pa) {
                        continue;
                    }
                    let gen = jit::page_gen(m, blk.head_pa);
                    blocks.push((blk, gen));
                }
            }
            if !blocks.is_empty() {
                let _ = self.tx.send(BakeJob { blocks, lay, maddr });
            }
        }
        // 焼き上がりの据え付け (collect時の世代と今が一致するものだけ)
        while let Ok(b) = self.rx.try_recv() {
            if jit::page_gen(m, b.pa) != b.gen {
                self.dropped_stale += 1;
                continue;
            }
            let slot = table_push(unsafe { std::mem::transmute::<usize, BlockFn>(b.f) });
            m.dcache.set_jit(b.pa, slot, b.n);
            self.installed += 1;
        }
    }
}
