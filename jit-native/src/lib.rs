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
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
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
}

fn sig(call_conv: CallConv, params: &[types::Type], ret: types::Type) -> Signature {
    let mut s = Signature::new(call_conv);
    for &p in params {
        s.params.push(AbiParam::new(p));
    }
    s.returns.push(AbiParam::new(ret));
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
        self.ld32_at(self.lay.regs + 4 * r as usize)
    }
    fn set_reg(&mut self, r: u8, v: Value) {
        self.st32_at(self.lay.regs + 4 * r as usize, v);
    }
    fn helper1(&mut self, sr: cranelift_codegen::ir::SigRef, f: usize, args: &[Value]) -> Value {
        let callee = self.fb.ins().iconst(types::I64, f as i64);
        let call = self.fb.ins().call_indirect(sr, callee, args);
        self.fb.inst_results(call)[0]
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
        let ip = self.ld32_at(self.lay.ip);
        let off = self.c32(self.cur_ip_off);
        let nip = self.fb.ins().iadd(ip, off);
        self.st32_at(self.lay.ip, nip);
        let k = self.c32(self.cur_k);
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

    fn emit_ld32(&mut self, mem: &JitMem) -> Value {
        let m = self.m_ptr();
        let seg = self.c32(mem.seg as u32);
        let off = self.eff_off(mem);
        let v64 = self.helper1(self.sigs.ld32, h_ld32 as usize, &[m, seg, off]);
        self.check_v64(v64)
    }

    /// ALU共通部: 演算 + cc材料の書き出し + (kind7以外) dstへ格納
    fn alu_core(&mut self, kind: u8, a: Value, b: Value, dst: Option<u8>) {
        let cin = if kind == 2 || kind == 3 {
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
        self.st8_at(self.lay.cc_op, kind);
        self.st8_at(self.lay.cc_w, 2);
        self.st32_at(self.lay.cc_a, a);
        self.st32_at(self.lay.cc_b, b);
        self.st32_at(self.lay.cc_cin, cin);
        self.st32_at(self.lay.cc_r, r);
        if kind != 7 {
            if let Some(d) = dst {
                self.set_reg(d, r);
            }
        }
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
                self.st8_at(self.lay.cc_op, if dec { 9 } else { 8 });
                self.st8_at(self.lay.cc_w, 2);
                self.st32_at(self.lay.cc_a, a);
                let one2 = self.c32(1);
                self.st32_at(self.lay.cc_b, one2);
                let z = self.c32(0);
                self.st32_at(self.lay.cc_cin, z);
                self.st32_at(self.lay.cc_r, r);
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
                let m = self.m_ptr();
                let seg = self.c32(mem.seg as u32);
                let off = self.eff_off(&mem);
                let v = self.reg(src);
                let ok = self.helper1(self.sigs.st32, h_st32 as usize, &[m, seg, off, v]);
                self.check_ok(ok);
            }
            JitOp::StoreMI { mem, imm } => {
                let m = self.m_ptr();
                let seg = self.c32(mem.seg as u32);
                let off = self.eff_off(&mem);
                let v = self.c32(imm);
                let ok = self.helper1(self.sigs.st32, h_st32 as usize, &[m, seg, off, v]);
                self.check_ok(ok);
            }
            JitOp::AluMR { kind, mem, reg } => {
                let m = self.m_ptr();
                let seg = self.c32(mem.seg as u32);
                let off = self.eff_off(&mem);
                let k = self.c32(kind as u32);
                let b = self.reg(reg);
                let ok = self.helper1(self.sigs.rmw32, h_rmw32 as usize, &[m, seg, off, k, b]);
                self.check_ok(ok);
            }
            JitOp::AluMI { kind, mem, imm } => {
                let m = self.m_ptr();
                let seg = self.c32(mem.seg as u32);
                let off = self.eff_off(&mem);
                let k = self.c32(kind as u32);
                let b = self.c32(imm);
                let ok = self.helper1(self.sigs.rmw32, h_rmw32 as usize, &[m, seg, off, k, b]);
                self.check_ok(ok);
            }
            JitOp::PushR { src } => {
                let m = self.m_ptr();
                let v = self.reg(src);
                let ok = self.helper1(self.sigs.push32, h_push32 as usize, &[m, v]);
                self.check_ok(ok);
            }
            JitOp::PushI { imm } => {
                let m = self.m_ptr();
                let v = self.c32(imm);
                let ok = self.helper1(self.sigs.push32, h_push32 as usize, &[m, v]);
                self.check_ok(ok);
            }
            JitOp::PopR { dst } => {
                let m = self.m_ptr();
                let v64 = self.helper1(self.sigs.pop32, h_pop32 as usize, &[m]);
                let v = self.check_v64(v64);
                // pop esp もこの順で正しい (SP更新→上書き)
                self.set_reg(dst, v);
            }
            JitOp::Leave => {
                let m = self.m_ptr();
                let ok = self.helper1(self.sigs.leave, h_leave as usize, &[m]);
                self.check_ok(ok);
            }
            JitOp::XchgA { reg } => {
                let a = self.reg(0);
                let r = self.reg(reg);
                self.set_reg(0, r);
                self.set_reg(reg, a);
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
    };

    let mut tr = Tr {
        fb: &mut fb,
        lay,
        sigs: &sigs,
        maddr,
        cur_k: 0,
        cur_ip_off: 0,
    };

    // 本体 (wasm側 compile_block と同じ流れ)。終端は出口で
    enum Term {
        Jcc { cc: u8, rel: u32 },
        Jmp { rel: u32 },
        Call { rel: u32 },
        Ret,
    }
    let mut term = None;
    let mut total_len: u32 = 0;
    for (i, &(len, ref op)) in blk.ops.iter().enumerate() {
        tr.cur_k = i as u32;
        tr.cur_ip_off = total_len;
        total_len += len as u32;
        match *op {
            JitOp::Jcc { cc, rel } => term = Some(Term::Jcc { cc, rel }),
            JitOp::Jmp { rel } => term = Some(Term::Jmp { rel }),
            JitOp::CallRel { rel } => term = Some(Term::Call { rel }),
            JitOp::Ret => term = Some(Term::Ret),
            _ => tr.op(op),
        }
    }

    // 出口: ipの帳尻と (call/retなら) スタック操作
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
                for blk in jit::collect_run(m, pa, 32, 8) {
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
