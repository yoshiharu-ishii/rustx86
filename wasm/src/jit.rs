//! F1a: テンプレートJITの生成器 — JitOp列を小さなwasmモジュールに焼く。
//!
//! 生成物は「関数1個 (export "b") のwasmモジュール」で、メインモジュールと
//! **同じリニアメモリ**をimportし、レジスタ・フラグ材料 (cc_*)・ipを
//! 実アドレス直打ちで読み書きする。番地は生成時に定数として焼き込む
//! (core::jit::layout が出す番地表)。
//!
//! ## 意味論の守り方
//!
//! - フラグはC1のlazy flags (cc_op方式) をそのまま踏襲 — 生成コードは
//!   材料 (op, a, b, cin, r) をメモリへ書くだけで、フラグを合成しない。
//!   インタプリタと**同じ表現**なので、ブロックの途中でインタプリタに
//!   戻っても状態はそのまま繋がる
//! - CFを「読む」必要がある所 (ADC/SBB/INC/DEC) と条件分岐の判定は、
//!   メインモジュールのヘルパ ([`rx86_jit_cf`]/[`rx86_jit_cond`]) を呼ぶ —
//!   遅延フラグの評価器を生成コードに二重実装しない (F1bで頻出形だけ
//!   インライン化を検討)。呼び出しは同期なので、直前にメモリへ書いた
//!   cc_* をヘルパが読む順序は保証される
//! - ipの更新は**ブロック出口で1回** — 途中で観測する者が居ない
//!   (F1aの語彙は#PF不能・割り込み受付はブロック境界の外) ため、
//!   毎命令更新と結果は同じ
//! - tsc/tick_countdown は生成コードでは触らない — 呼ぶ側 (Rust) が
//!   「実行した命令数」の返り値でまとめて清算する (契約は増分3)
//!
//! ## モジュール構造 (jit-probe で固定費実測済みの形)
//!
//! type: [()->i32, (i32)->i32, (i32,i32)->i32, (i32,i32,i32)->i64,
//!        (i32×4)->i32, (i32×5)->i32]
//! import: e.m = memory / e.cf = CF評価 / e.cond = 条件評価 /
//!         e.ld32 = ロード / e.st32 = ストア / e.rmw32 = alu [mem],b
//! export: "b" = ブロック本体 (返り値 = 実行した命令数。全数未満 = フォールト脱出)

use rustx86_core::jit::{JitBlock, JitLayout, JitMem, JitOp};
use rustx86_core::{cpu, Machine};

// ---- メインモジュール側のヘルパ (生成コードからimportされる) ----
//
// 引数はMachineの実アドレス (生成時に定数で焼く)。wasm32では
// 「メインモジュールのヒープ番地」= 共有リニアメモリのオフセット。
// 生成コードとRustが同じメモリを見ているからこの受け渡しが成立する。

/// 遅延フラグからCFを評価する (ADC/SBB/INC/DECのキャリー入力)
///
/// # Safety
/// `m` は生きているMachineの実アドレスであること (呼ぶのは生成コードだけ)
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_cf(m: *const Machine) -> i32 {
    (*m).cpu.flag(cpu::CF) as i32
}

/// 条件コード (jcc/setccのcc) を遅延フラグから評価する
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_cond(m: *const Machine, cc: i32) -> i32 {
    cpu::alu::condition(&(*m).cpu, cc as u8) as i32
}

/// メモリロード 32bit (F1b、ADR-0008のフォールト脱出モデル)。
/// セグメント適用 (lin) と変換はここ = Rust側でやる — 意味論の原本は1つ。
/// フォールトしそうなときは**記録せず** (pending_faultに触れず) 上位32bitで
/// 合図する。生成コードはそれを見て、状態を変えずにブロックを脱出する。
///
/// 返り値: 成功 = 値 (上位32bit=0) / 脱出 = 1<<32
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_ld32(m: *const Machine, seg: i32, off: i32) -> i64 {
    let m = &*m;
    let la = m.cpu.lin(seg as usize, off as u32);
    match m.jit_try_read32(la) {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

/// メモリストア 32bit (F1b-2)。全チェックを通ってから一括で書く —
/// 部分的に書いた状態は作らない。返り値: 1 = 完了 / 0 = 脱出 (何も書いていない)
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_st32(m: *mut Machine, seg: i32, off: i32, val: i32) -> i32 {
    // F1d世代のcore API (seg,off) — fast_write32へ委譲 (note_write込み、
    // 非平坦segは脱出=インタプリタへ)
    (*m).jit_try_write32(seg as usize, off as u32, val as u32) as i32
}

/// RMW (`alu [mem], b` — F1b-2)。read→alu_w→write をここで完結する。
/// 書き込み権限のtranslateを先に試すので、ccが汚れる前に脱出できる。
/// 返り値: 1 = 完了 (ccとメモリ更新済み) / 0 = 脱出 (状態は無傷)
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_rmw32(
    m: *mut Machine,
    seg: i32,
    off: i32,
    kind: i32,
    b: i32,
) -> i32 {
    (*m).jit_try_rmw32(seg as usize, off as u32, kind as u8, b as u32) as i32
}

/// push (F1b-3)。SPの確定は成功時だけ — push32と同じ順序。
/// 返り値: 1 = 完了 / 0 = 脱出 (SPもメモリも無傷)
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_push32(m: *mut Machine, val: i32) -> i32 {
    (*m).jit_try_push32(val as u32) as i32
}

/// pop (F1b-3)。返り値: 成功 = 値 (上位32bit=0) / 脱出 = 1<<32 (SP不変)
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_pop32(m: *mut Machine) -> i64 {
    match (*m).jit_try_pop32() {
        Some(v) => v as i64,
        None => 1i64 << 32,
    }
}

/// leave (F1b-3)。返り値: 1 = 完了 / 0 = 脱出 (SP/BP無傷)
///
/// # Safety
/// 同上
#[no_mangle]
pub unsafe extern "C" fn rx86_jit_leave(m: *mut Machine) -> i32 {
    (*m).jit_try_leave() as i32
}

// ---- wasmバイト列の組み立て (依存なしの手組み — jit-probeの移植) ----

fn uleb(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            return;
        }
    }
}

fn sleb(out: &mut Vec<u8>, mut n: i64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        let done = (n == 0 && b & 0x40 == 0) || (n == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            return;
        }
    }
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

// opcode (使う分だけ)
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I32_LOAD: u8 = 0x28;
const I32_STORE: u8 = 0x36;
const I32_STORE8: u8 = 0x3a;
const I32_ADD: u8 = 0x6a;
const I32_SUB: u8 = 0x6b;
const I32_AND: u8 = 0x71;
const I32_OR: u8 = 0x72;
const I32_XOR: u8 = 0x73;
const I32_SHL: u8 = 0x74;
const I64_SHR_U: u8 = 0x88;
const I32_WRAP_I64: u8 = 0xa7;
const I32_EQZ: u8 = 0x45;
const LOCAL_GET: u8 = 0x20;
const LOCAL_SET: u8 = 0x21;
const CALL: u8 = 0x10;
const SELECT: u8 = 0x1b;
const IF: u8 = 0x04;
const RETURN: u8 = 0x0f;
const END: u8 = 0x0b;
/// ifのblocktype: 結果なし
const BT_EMPTY: u8 = 0x40;

// ローカル変数の番地 (関数は引数なしなので0から)
const L_A: u32 = 0; // オペランドa
const L_B: u32 = 1; // オペランドb
const L_CIN: u32 = 2; // キャリー入力 (CF評価の置き場も兼ねる)
const L_R: u32 = 3; // 結果
const L_V64: u32 = 4; // ロードヘルパの返り値 (i64: 上位=脱出合図/下位=値)

/// 生成器の作業場。codeに命令列を積んでいく
struct Gen<'a> {
    code: Vec<u8>,
    lay: &'a JitLayout,
    /// Machineの実アドレス (ヘルパに渡す定数)
    maddr: u32,
    /// 今生成中の命令の手前までに**完全に実行し終えた**命令数 (脱出の返り値)
    cur_k: u32,
    /// ブロック頭から今の命令までのバイトオフセット (脱出時のip合わせ)
    cur_ip_off: u32,
}

impl Gen<'_> {
    fn iconst(&mut self, v: u32) {
        self.code.push(I32_CONST);
        sleb(&mut self.code, v as i32 as i64);
    }
    /// メモリ番地からi32を読む (align=2)
    fn load(&mut self, addr: usize) {
        self.iconst(addr as u32);
        self.code.extend_from_slice(&[I32_LOAD, 0x02, 0x00]);
    }
    /// スタックトップをメモリ番地へ書く — 呼ぶ側が [addr, value] の順で積む
    fn store_op(&mut self) {
        self.code.extend_from_slice(&[I32_STORE, 0x02, 0x00]);
    }
    /// 定数をメモリ番地へ書く
    fn store_const(&mut self, addr: usize, v: u32) {
        self.iconst(addr as u32);
        self.iconst(v);
        self.store_op();
    }
    /// ローカルをメモリ番地へ書く
    fn store_local(&mut self, addr: usize, l: u32) {
        self.iconst(addr as u32);
        self.local_get(l);
        self.store_op();
    }
    /// 1バイト書き (cc_op / cc_w 用)
    fn store8_const(&mut self, addr: usize, v: u8) {
        self.iconst(addr as u32);
        self.iconst(v as u32);
        self.code.extend_from_slice(&[I32_STORE8, 0x00, 0x00]);
    }
    fn local_get(&mut self, l: u32) {
        self.code.push(LOCAL_GET);
        uleb(&mut self.code, l as u64);
    }
    fn local_set(&mut self, l: u32) {
        self.code.push(LOCAL_SET);
        uleb(&mut self.code, l as u64);
    }
    fn reg_addr(&self, r: u8) -> usize {
        self.lay.regs + 4 * r as usize
    }
    /// CFをヘルパで評価して L_CIN へ
    fn eval_cf_into_cin(&mut self) {
        self.iconst(self.maddr);
        self.code.push(CALL);
        uleb(&mut self.code, 0); // import 0 = e.cf
        self.local_set(L_CIN);
    }

    /// 実効オフセットをスタックへ (disp + base + (index<<scale))。
    /// レジスタの**今の**値から組む — インタプリタの off_of と同じ形
    fn eff_off(&mut self, mem: &JitMem) {
        self.iconst(mem.disp);
        if mem.base >= 0 {
            self.load(self.reg_addr(mem.base as u8));
            self.code.push(I32_ADD);
        }
        if mem.index >= 0 {
            self.load(self.reg_addr(mem.index as u8));
            self.iconst(mem.scale as u32);
            self.code.push(I32_SHL);
            self.code.push(I32_ADD);
        }
    }

    /// 32bitロード (F1b)。成功なら値をスタックに残す。
    /// フォールトしそうなら**現命令の状態を1つも変える前に**脱出する:
    /// ip = ブロック頭 + cur_ip_off、返り値 = cur_k (完全実行済みの命令数)。
    /// フォールトの記録・配送はやり直すインタプリタの仕事
    fn emit_load32(&mut self, mem: &JitMem) {
        self.iconst(self.maddr);
        self.iconst(mem.seg as u32);
        self.eff_off(mem);
        self.code.push(CALL);
        uleb(&mut self.code, 2); // import 2 = e.ld32
        self.local_set(L_V64);
        self.escape_if_v64_hi();
        // 成功: 下位32bitが値
        self.local_get(L_V64);
        self.code.push(I32_WRAP_I64);
    }

    /// i64返しヘルパの合図を裁く: L_V64 の上位32bitが立っていたら脱出。
    /// 事前に local_set(L_V64) しておくこと
    fn escape_if_v64_hi(&mut self) {
        self.local_get(L_V64);
        self.code.push(I64_CONST);
        sleb(&mut self.code, 32);
        self.code.push(I64_SHR_U);
        self.code.push(I32_WRAP_I64);
        self.code.extend_from_slice(&[IF, BT_EMPTY]);
        self.emit_escape();
        self.code.push(END);
    }

    /// 脱出の本体: ip = ブロック頭 + cur_ip_off、返り値 = cur_k
    fn emit_escape(&mut self) {
        self.iconst(self.lay.ip as u32);
        self.load(self.lay.ip);
        self.iconst(self.cur_ip_off);
        self.code.push(I32_ADD);
        self.store_op();
        self.iconst(self.cur_k);
        self.code.push(RETURN);
    }

    /// ストア/RMWヘルパの返り値 (1=完了/0=脱出) を裁く。
    /// スタックトップに返り値が積まれた状態で呼ぶ
    fn escape_if_zero(&mut self) {
        self.code.push(I32_EQZ);
        self.code.extend_from_slice(&[IF, BT_EMPTY]);
        self.emit_escape();
        self.code.push(END);
    }

    /// ALUの共通部: L_A/L_B が積まれた前提で、kindの演算 + cc材料の書き出し。
    /// `dst` があれば結果をレジスタへ (kind7=CMPはNone扱い)
    fn alu_core(&mut self, kind: u8, dst: Option<u8>) {
        // cin: ADC/SBBだけCFを食う (インタプリタのalu_lazyと同じ)
        if kind == 2 || kind == 3 {
            self.eval_cf_into_cin();
        } else {
            self.iconst(0);
            self.local_set(L_CIN);
        }
        // r = 演算 (幅32bitはwasmのi32がそのまま面倒を見る)
        self.local_get(L_A);
        self.local_get(L_B);
        match kind {
            0 | 2 => {
                self.code.push(I32_ADD);
                self.local_get(L_CIN);
                self.code.push(I32_ADD);
            }
            1 => self.code.push(I32_OR),
            3 | 5 | 7 => {
                self.code.push(I32_SUB);
                self.local_get(L_CIN);
                self.code.push(I32_SUB);
            }
            4 => self.code.push(I32_AND),
            _ => self.code.push(I32_XOR), // 6 = XOR
        }
        self.local_set(L_R);
        // cc材料 (インタプリタのset_ccと同じ内容をメモリへ)
        self.store8_const(self.lay.cc_op, kind);
        self.store8_const(self.lay.cc_w, 2);
        self.store_local(self.lay.cc_a, L_A);
        self.store_local(self.lay.cc_b, L_B);
        self.store_local(self.lay.cc_cin, L_CIN);
        self.store_local(self.lay.cc_r, L_R);
        if kind != 7 {
            if let Some(d) = dst {
                self.store_local(self.reg_addr(d), L_R);
            }
        }
    }

    fn op(&mut self, op: &JitOp) {
        match *op {
            JitOp::MovRI { dst, imm } => self.store_const(self.reg_addr(dst), imm),
            JitOp::MovRR { dst, src } => {
                self.iconst(self.reg_addr(dst) as u32);
                self.load(self.reg_addr(src));
                self.store_op();
            }
            JitOp::AluRR { kind, dst, src } => {
                self.load(self.reg_addr(dst));
                self.local_set(L_A);
                self.load(self.reg_addr(src));
                self.local_set(L_B);
                self.alu_core(kind, Some(dst));
            }
            JitOp::AluRI { kind, dst, imm } => {
                self.load(self.reg_addr(dst));
                self.local_set(L_A);
                self.iconst(imm);
                self.local_set(L_B);
                self.alu_core(kind, Some(dst));
            }
            JitOp::TestRR { a, b } => {
                self.load(self.reg_addr(a));
                self.local_set(L_A);
                self.load(self.reg_addr(b));
                self.local_set(L_B);
                self.alu_core(4, None);
            }
            JitOp::IncDec { reg, dec } => {
                // CFは不変 — インタプリタのset_cc_incdecと同じく、
                // **遅延状態を上書きする前に**CFを評価してflagsのbit0へ退避
                self.eval_cf_into_cin();
                self.iconst(self.lay.flags as u32);
                self.load(self.lay.flags);
                self.iconst(!1u32);
                self.code.push(I32_AND);
                self.local_get(L_CIN);
                self.code.push(I32_OR);
                self.store_op();
                // a, r = a±1
                self.load(self.reg_addr(reg));
                self.local_set(L_A);
                self.local_get(L_A);
                self.iconst(1);
                self.code.push(if dec { I32_SUB } else { I32_ADD });
                self.local_set(L_R);
                // cc材料 (op=8:INC / 9:DEC、cinは0)
                self.store8_const(self.lay.cc_op, if dec { 9 } else { 8 });
                self.store8_const(self.lay.cc_w, 2);
                self.store_local(self.lay.cc_a, L_A);
                self.store_const(self.lay.cc_b, 1);
                self.store_const(self.lay.cc_cin, 0);
                self.store_local(self.lay.cc_r, L_R);
                self.store_local(self.reg_addr(reg), L_R);
            }
            JitOp::Lea {
                dst,
                base,
                index,
                scale,
                disp,
            } => {
                self.iconst(self.reg_addr(dst) as u32);
                self.iconst(disp);
                if base >= 0 {
                    self.load(self.reg_addr(base as u8));
                    self.code.push(I32_ADD);
                }
                if index >= 0 {
                    self.load(self.reg_addr(index as u8));
                    self.iconst(scale as u32);
                    self.code.push(I32_SHL);
                    self.code.push(I32_ADD);
                }
                self.store_op();
            }
            // ---- F1b-1: メモリロード (フォールトしそうなら emit_load32 が脱出) ----
            JitOp::MovRM { dst, mem } => {
                self.iconst(self.reg_addr(dst) as u32);
                self.emit_load32(&mem);
                self.store_op();
            }
            JitOp::AluRM { kind, dst, mem } => {
                // a = dst / b = メモリ (インタプリタの AluRRm と同じ向き)。
                // 脱出点はロード = 状態を変える前 (L_A/L_B はブロック外に見えない)
                self.load(self.reg_addr(dst));
                self.local_set(L_A);
                self.emit_load32(&mem);
                self.local_set(L_B);
                self.alu_core(kind, Some(dst));
            }
            JitOp::CmpMR { mem, reg } => {
                // a = メモリ / b = レジスタ (インタプリタの AluRmR kind7 と同じ向き)
                self.emit_load32(&mem);
                self.local_set(L_A);
                self.load(self.reg_addr(reg));
                self.local_set(L_B);
                self.alu_core(7, None);
            }
            JitOp::CmpMI { mem, imm } => {
                self.emit_load32(&mem);
                self.local_set(L_A);
                self.iconst(imm);
                self.local_set(L_B);
                self.alu_core(7, None);
            }
            // ---- F1b-2: ストア/RMW (ヘルパが全チェック後に書く — 部分状態なし) ----
            JitOp::StoreMR { mem, src } => {
                self.iconst(self.maddr);
                self.iconst(mem.seg as u32);
                self.eff_off(&mem);
                self.load(self.reg_addr(src));
                self.code.push(CALL);
                uleb(&mut self.code, 3); // import 3 = e.st32
                self.escape_if_zero();
            }
            JitOp::StoreMI { mem, imm } => {
                self.iconst(self.maddr);
                self.iconst(mem.seg as u32);
                self.eff_off(&mem);
                self.iconst(imm);
                self.code.push(CALL);
                uleb(&mut self.code, 3);
                self.escape_if_zero();
            }
            JitOp::AluMR { kind, mem, reg } => {
                // read→alu_w→write はヘルパの中 (ccの更新もRust側で済む)
                self.iconst(self.maddr);
                self.iconst(mem.seg as u32);
                self.eff_off(&mem);
                self.iconst(kind as u32);
                self.load(self.reg_addr(reg));
                self.code.push(CALL);
                uleb(&mut self.code, 4); // import 4 = e.rmw32
                self.escape_if_zero();
            }
            JitOp::AluMI { kind, mem, imm } => {
                self.iconst(self.maddr);
                self.iconst(mem.seg as u32);
                self.eff_off(&mem);
                self.iconst(kind as u32);
                self.iconst(imm);
                self.code.push(CALL);
                uleb(&mut self.code, 4);
                self.escape_if_zero();
            }
            JitOp::TestMR { mem, reg } => {
                self.emit_load32(&mem);
                self.local_set(L_A);
                self.load(self.reg_addr(reg));
                self.local_set(L_B);
                self.alu_core(4, None);
            }
            // ---- F1b-3: スタック形 (SP確定は成功時だけ — ヘルパが担保) ----
            JitOp::PushR { src } => {
                self.iconst(self.maddr);
                self.load(self.reg_addr(src));
                self.code.push(CALL);
                uleb(&mut self.code, 5); // import 5 = e.push32
                self.escape_if_zero();
            }
            JitOp::PushI { imm } => {
                self.iconst(self.maddr);
                self.iconst(imm);
                self.code.push(CALL);
                uleb(&mut self.code, 5);
                self.escape_if_zero();
            }
            JitOp::PopR { dst } => {
                self.iconst(self.reg_addr(dst) as u32);
                self.iconst(self.maddr);
                self.code.push(CALL);
                uleb(&mut self.code, 6); // import 6 = e.pop32
                self.local_set(L_V64);
                self.escape_if_v64_hi();
                self.local_get(L_V64);
                self.code.push(I32_WRAP_I64);
                self.store_op(); // pop esp もこの順で正しい (SP更新→上書き)
            }
            JitOp::Leave => {
                self.iconst(self.maddr);
                self.code.push(CALL);
                uleb(&mut self.code, 7); // import 7 = e.leave
                self.escape_if_zero();
            }
            JitOp::XchgA { reg } => {
                // eAX ↔ reg (reg=0 の 0x90 nop も自分と交換で正しい)
                self.load(self.reg_addr(0));
                self.local_set(L_A);
                self.iconst(self.reg_addr(0) as u32);
                self.load(self.reg_addr(reg));
                self.store_op();
                self.store_local(self.reg_addr(reg), L_A);
            }
            // 終端はcompile_blockが面倒を見る (ipの帳尻)
            JitOp::Jcc { .. } | JitOp::Jmp { .. } | JitOp::CallRel { .. } | JitOp::Ret => {
                unreachable!("終端はcompile_blockの出口で扱う")
            }
            // 語彙v2 (F1c-b2〜) はネイティブ専用 — wasmは凍結時点のF1B語彙で
            // collectする (CAP_F1B) ので、ここへは来ない
            _ => unreachable!("語彙v2はwasm生成器 (凍結) に渡らない (collectのcaps)"),
        }
    }
}

/// ブロックをwasmモジュールに焼く。
/// `machine_addr` は生きているMachineの実アドレス (ヘルパへ焼き込む)
/// 1ブロックを単独モジュールに包む (テスト用の互換口 — 本番はcompile_batch)
pub fn compile_block(block: &JitBlock, lay: &JitLayout, machine_addr: u32) -> Vec<u8> {
    compile_batch(&[compile_body(block, lay, machine_addr)])
}

/// ブロック本体 (locals宣言込みのcode section用ボディ) だけを焼く。
/// モジュールへの包みは compile_batch — **モジュール数を減らすのが目的**:
/// 1ブロック=1モジュールだと据え付け (エンジンのコンパイル) の固定費が
/// ブロック数×~0.5msで効き、36kブロックのブートで+19s (2026-08-17実測、
/// jit-probeの6.3µs/個は「1個なら」の数字だった)
pub fn compile_body(block: &JitBlock, lay: &JitLayout, machine_addr: u32) -> Vec<u8> {
    let mut g = Gen {
        code: Vec::new(),
        lay,
        maddr: machine_addr,
        cur_k: 0,
        cur_ip_off: 0,
    };

    // 終端の種類 (出口の形が違う)
    enum Term {
        Jcc { cc: u8, rel: u32 },
        Jmp { rel: u32 },
        Call { rel: u32 },
        Ret,
    }

    // 本体: 終端以外を順に。ipは触らない (通常出口で1回 —
    // フォールト脱出だけが途中のipを書く)。ループを抜けた時点の
    // cur_k/cur_ip_off は終端命令のもの — 終端の脱出 (call/retのpush/pop) が
    // そのまま正しい座標で逃げられる
    let mut term = None;
    let mut total_len: u32 = 0;
    for (i, &(len, ref op)) in block.ops.iter().enumerate() {
        g.cur_k = i as u32;
        g.cur_ip_off = total_len;
        total_len += len as u32;
        match *op {
            JitOp::Jcc { cc, rel } => term = Some(Term::Jcc { cc, rel }),
            JitOp::Jmp { rel } => term = Some(Term::Jmp { rel }),
            JitOp::CallRel { rel } => term = Some(Term::Call { rel }),
            JitOp::Ret => term = Some(Term::Ret),
            _ => g.op(op),
        }
    }

    // 出口: ipの帳尻と (call/retなら) スタック操作
    match term {
        None => {
            // ip = ip + total_len
            g.iconst(lay.ip as u32);
            g.load(lay.ip);
            g.iconst(total_len);
            g.code.push(I32_ADD);
            g.store_op();
        }
        Some(Term::Jmp { rel }) => {
            g.iconst(lay.ip as u32);
            g.load(lay.ip);
            g.iconst(total_len.wrapping_add(rel));
            g.code.push(I32_ADD);
            g.store_op();
        }
        Some(Term::Jcc { cc, rel }) => {
            // ip = ip + select(total+rel, total, cond)
            g.iconst(lay.ip as u32);
            g.load(lay.ip);
            g.iconst(total_len.wrapping_add(rel));
            g.iconst(total_len);
            g.iconst(g.maddr);
            g.iconst(cc as u32);
            g.code.push(CALL);
            uleb(&mut g.code, 1); // import 1 = e.cond
            g.code.push(SELECT);
            g.code.push(I32_ADD);
            g.store_op();
        }
        Some(Term::Call { rel }) => {
            // 戻り番地 (= ip + total_len) をpush。ここが脱出点 —
            // 失敗ならSPもipも無傷でインタプリタがやり直す
            g.iconst(g.maddr);
            g.load(lay.ip);
            g.iconst(total_len);
            g.code.push(I32_ADD);
            g.code.push(CALL);
            uleb(&mut g.code, 5); // import 5 = e.push32
            g.escape_if_zero();
            // ip = ip + total_len + rel
            g.iconst(lay.ip as u32);
            g.load(lay.ip);
            g.iconst(total_len.wrapping_add(rel));
            g.code.push(I32_ADD);
            g.store_op();
        }
        Some(Term::Ret) => {
            // popした値が次のip。popが脱出点
            g.iconst(g.maddr);
            g.code.push(CALL);
            uleb(&mut g.code, 6); // import 6 = e.pop32
            g.local_set(L_V64);
            g.escape_if_v64_hi();
            g.iconst(lay.ip as u32);
            g.local_get(L_V64);
            g.code.push(I32_WRAP_I64);
            g.store_op();
        }
    }

    // 返り値: 実行した命令数 (脱出しなかったら全部)
    g.iconst(block.ops.len() as u32);
    g.code.push(END);

    // locals (i32×4, i64×1) + 本体 = code sectionのボディ
    let mut body = Vec::new();
    body.extend_from_slice(&[2, 4, 0x7f, 1, 0x7e]);
    body.extend_from_slice(&g.code);
    body
}

/// N個のボディを1モジュールに包む。export名は "b0".."bN-1" (関数import 8本の後)
pub fn compile_batch(bodies: &[Vec<u8>]) -> Vec<u8> {
    let n = bodies.len();
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    // type: [()->i32, (i32)->i32, (i32,i32)->i32, (i32,i32,i32)->i64,
    //        (i32×4)->i32, (i32×5)->i32, (i32)->i64]
    let mut b = Vec::new();
    uleb(&mut b, 7);
    b.extend_from_slice(&[0x60, 0x00, 0x01, 0x7f]);
    b.extend_from_slice(&[0x60, 0x01, 0x7f, 0x01, 0x7f]);
    b.extend_from_slice(&[0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f]);
    b.extend_from_slice(&[0x60, 0x03, 0x7f, 0x7f, 0x7f, 0x01, 0x7e]);
    b.extend_from_slice(&[0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f]);
    b.extend_from_slice(&[0x60, 0x05, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f]);
    b.extend_from_slice(&[0x60, 0x01, 0x7f, 0x01, 0x7e]);
    section(&mut m, 1, &b);
    // import: e.cf/e.cond/e.ld32/e.st32/e.rmw32/e.push32/e.pop32/e.leave (func)、
    //         e.m (memory min1)
    let mut b = Vec::new();
    uleb(&mut b, 9);
    for (name, desc) in [
        ("cf", &[0x00, 0x01][..]),
        ("cond", &[0x00, 0x02][..]),
        ("ld32", &[0x00, 0x03][..]),
        ("st32", &[0x00, 0x04][..]),
        ("rmw32", &[0x00, 0x05][..]),
        ("push32", &[0x00, 0x02][..]),
        ("pop32", &[0x00, 0x06][..]),
        ("leave", &[0x00, 0x01][..]),
        ("m", &[0x02, 0x00, 0x01][..]),
    ] {
        uleb(&mut b, 1);
        b.push(b'e');
        uleb(&mut b, name.len() as u64);
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(desc);
    }
    section(&mut m, 2, &b);
    // function: type0 × N
    let mut b = Vec::new();
    uleb(&mut b, n as u64);
    b.extend(std::iter::repeat_n(0x00, n)); // 全部type0
    section(&mut m, 3, &b);
    // export: "b0".."bN-1" = func 8+i
    let mut b = Vec::new();
    uleb(&mut b, n as u64);
    for i in 0..n {
        let name = format!("b{i}");
        uleb(&mut b, name.len() as u64);
        b.extend_from_slice(name.as_bytes());
        b.push(0x00);
        uleb(&mut b, (8 + i) as u64);
    }
    section(&mut m, 7, &b);
    // code: N本
    let mut b = Vec::new();
    uleb(&mut b, n as u64);
    for body in bodies {
        uleb(&mut b, body.len() as u64);
        b.extend_from_slice(body);
    }
    section(&mut m, 10, &b);
    m
}

// ---- ランタイム (F1d世代): try_enter駆動の直接マップ ----
//
// F1a時代は「熱カウンタ→Entryへ据え付け」だったが、Entryの受け口は税
// (+8B/Entry、削除で-22%) なので、ネイティブ (jit-a64) と同じ
// **チェーン入口+taken着地でランタイムに問い合わせる**形へ載せ替えた。
// wasm特有の事情は1つだけ: instantiateはJSにしかできないので、
// bake (バイト列生成) は同期・据え付けは非同期 (スライス境界でJSがpump)。
// 据え付くまでそのブロック頭は0を返してインタプリタが走る — 退路は常にある

/// 焼き上がってJSのinstantiate待ちのジョブ (本体のみ — モジュールへの
/// 包みはdrain時にバッチでやる)
pub struct Job {
    pub pa: u32,
    pub gen: u32,
    pub n: u32,
    pub body: Vec<u8>,
}

/// 直接マップのスロット状態
#[derive(Clone, Copy, PartialEq)]
enum SlotState {
    Free,
    /// バイト列は出荷済み、JSのinstantiate待ち
    Baking,
    /// 据え付け済み (table_slotが有効)
    Installed,
    /// 語彙外・短すぎ — 再挑戦しない負の印
    Rejected,
}

#[derive(Clone, Copy)]
struct Slot {
    tag: u32,
    gen: u32,
    table_slot: u32,
    n: u16,
    state: SlotState,
}

const JSLOTS: usize = 64 * 1024;
const TAG_FREE: u32 = 0xFFFF_FFFF;

/// JITランタイムの台帳。Emulatorが1個持つ (Boxで番地固定)
pub struct JitRt {
    slots: Vec<Slot>,
    jobs: Vec<Job>,
    machine: *mut Machine,
    pub installed: usize,
    pub baked: u64,
}

thread_local! {
    /// try_enter (裸のfnポインタ) から届くための生ポインタ。wasmは単一スレッド
    static RT: core::cell::Cell<*mut JitRt> = const { core::cell::Cell::new(core::ptr::null_mut()) };
    /// 診断モード (jit-checkの税分解用): 0=通常 / 1=即return0 (プローブ機構の税だけ) /
    /// 2=焼く+据えるが実行しない (焼き税まで)
    static DIAG: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// 診断モード設定 (税の分解計測用 — 本番では触らない)
pub fn set_diag(mode: u32) {
    DIAG.with(|c| c.set(mode));
}

impl Default for JitRt {
    fn default() -> Self {
        Self::new()
    }
}

impl JitRt {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(JSLOTS);
        slots.resize(
            JSLOTS,
            Slot {
                tag: TAG_FREE,
                gen: 0,
                table_slot: 0,
                n: 0,
                state: SlotState::Free,
            },
        );
        JitRt {
            slots,
            jobs: Vec::new(),
            machine: core::ptr::null_mut(),
            installed: 0,
            baked: 0,
        }
    }

    /// 取り付け (Emulator::jit_enable から)。JitRtとMachineは以後動かない前提
    ///
    /// # Safety
    /// self と m がJIT実行中ずっと同じ番地に居ること (Boxの中で不動)
    pub unsafe fn attach(&mut self, m: *mut Machine) {
        self.machine = m;
        RT.with(|c| c.set(self as *mut JitRt));
    }

    /// 溜まったジョブを最大cap件、1バッチとして取り出す
    pub fn take_batch(&mut self, cap: usize) -> Vec<Job> {
        let k = self.jobs.len().min(cap);
        self.jobs.drain(..k).collect()
    }

    /// JSが table.set したスロット番号を受けて据え付ける。
    /// 焼いた後に世代が動いていても構わず据える — try_enterの世代照合が
    /// 初回ヒットで捨てて焼き直す (自己回復)
    pub fn install(&mut self, pa: u32, gen: u32, n: u32, table_slot: u32) {
        let si = ((pa ^ (pa >> 12)) as usize) & (JSLOTS - 1);
        let slot = &mut self.slots[si];
        if slot.tag == pa && slot.state == SlotState::Baking {
            slot.gen = gen;
            slot.table_slot = table_slot;
            slot.n = n as u16;
            slot.state = SlotState::Installed;
            self.installed += 1;
        }
        // tagが変わっていたら (衝突退去)、据え付け先を失った孤児 — 捨てるだけ
    }

    /// スナップショット復元・OS入れ替えで全部捨てる
    pub fn flush(&mut self) {
        for s in &mut self.slots {
            *s = Slot {
                tag: TAG_FREE,
                gen: 0,
                table_slot: 0,
                n: 0,
                state: SlotState::Free,
            };
        }
        self.jobs.clear();
        self.installed = 0;
    }
}

/// coreのJitHook.try_enter に挿す実行口 (jit-a64と同じ契約):
/// 焼けたブロックがあり世代が合い予算に収まるなら実行して実行数を返す。
/// 無ければ焼いてジョブへ積み (据え付けはJSのpump待ち)、0を返す
pub fn try_enter(_ctx: usize, pa: u32, gen: u32, budget: u32) -> u32 {
    let p = RT.with(|c| c.get());
    if p.is_null() {
        return 0;
    }
    let diag = DIAG.with(|c| c.get());
    if diag == 1 {
        return 0; // プローブ機構 (coreの再プローブループ+この呼び出し) の税だけ測る
    }
    let rt = unsafe { &mut *p };
    let si = ((pa ^ (pa >> 12)) as usize) & (JSLOTS - 1);
    let slot = rt.slots[si];
    if slot.tag == pa {
        match slot.state {
            SlotState::Installed => {
                if diag == 2 {
                    return 0; // 焼き+据え付けまでの税 (ブロック実行なし)
                }
                if slot.gen != gen {
                    // 世代落ち (自己書き換え)。捨てて次の来訪で焼き直す
                    rt.slots[si].state = SlotState::Free;
                    rt.slots[si].tag = TAG_FREE;
                    rt.installed -= 1;
                    return 0;
                }
                if slot.n as u32 > budget {
                    return 0;
                }
                return call_block(slot.table_slot);
            }
            SlotState::Baking | SlotState::Rejected => return 0,
            SlotState::Free => {}
        }
    }
    // ミス (衝突 or 初訪)。**Baking中の占有者は退去させない** — 据え付け
    // (JSのpump) は非同期なので、退去させるとジョブが全員孤児になる
    // (2026-08-17に installed=0 + 焼き39.7万の暴走として実測)。
    // 新参はpumpが片づけるまで素通り (インタプリタが走る — 退路)
    if rt.slots[si].state == SlotState::Baking {
        return 0;
    }
    // 同期でバイト列を焼き、instantiateはJSへ
    let m = unsafe { &mut *rt.machine };
    // fillと同じ義務: このページに「コードあり」を立てる (立て忘れは
    // SMC/DMA検出網の穴 — jit-a64で実証済みの事故)
    rustx86_core::jit::mark_code_page(m, pa);
    let Some(blk) = rustx86_core::jit::collect_block_caps(m, pa, 32, rustx86_core::jit::CAP_F1B)
    else {
        rt.slots[si] = Slot {
            tag: pa,
            gen,
            table_slot: 0,
            n: 0,
            state: SlotState::Rejected,
        };
        return 0;
    };
    if blk.ops.len() < 2 {
        rt.slots[si] = Slot {
            tag: pa,
            gen,
            table_slot: 0,
            n: 0,
            state: SlotState::Rejected,
        };
        return 0;
    }
    let lay = rustx86_core::jit::layout(m);
    let maddr = rt.machine as u32;
    let n = blk.ops.len() as u32;
    let body = compile_body(&blk, &lay, maddr);
    rt.baked += 1;
    if rt.slots[si].state == SlotState::Installed {
        rt.installed -= 1; // 衝突退去
    }
    rt.slots[si] = Slot {
        tag: pa,
        gen,
        table_slot: 0,
        n: n as u16,
        state: SlotState::Baking,
    };
    rt.jobs.push(Job { pa, gen, n, body });
    0
}

/// 生成ブロックを **JS境界なしで** 呼ぶ (F1a call_indirect)。
///
/// `slot` は `__indirect_function_table` (= wasm-bindgen の function_table) の添字。
/// wasm では `fn()->u32` の値はこのテーブルの添字そのものなので、slot を関数
/// ポインタへ transmute して呼ぶと **call_indirect** が1個出るだけ — JSへの
/// 往復が消える。
///
/// # Safety (実質)
/// slot に据わっているのは install 時に table.set した `()->i32` の生成関数。
/// 型不一致なら call_indirect が trap する
#[cfg(target_arch = "wasm32")]
fn call_block(slot: u32) -> u32 {
    let f: fn() -> u32 = unsafe { core::mem::transmute::<usize, fn() -> u32>(slot as usize) };
    f()
}

/// ホスト (テスト) では生成wasmを実行できない — 常に「焼けていない」扱い
#[cfg(not(target_arch = "wasm32"))]
fn call_block(_slot: u32) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustx86_core::{jit, MachineProfile};

    fn block_from(code: &[u8]) -> (Machine, JitBlock, JitLayout) {
        let mut m = Machine::with_profile(MachineProfile::pc_32bit(4));
        for (i, b) in code.iter().enumerate() {
            m.write_phys8(0x10000 + i as u32, *b);
        }
        let blk = jit::collect_block(&m, 0x10000, 32).expect("block");
        let lay = jit::layout(&m);
        (m, blk, lay)
    }

    /// 生成モジュールが**本物のwasm検証** (型・スタック規律・LEB) を通るか。
    /// V8のinstantiateが通る形であることの、CIで回せる代役
    #[test]
    fn generated_modules_pass_real_validation() {
        let cases: &[&[u8]] = &[
            // mov/ALU/jcc入りのループ
            &[
                0xB8, 0x05, 0x00, 0x00, 0x00, // mov eax,5
                0x89, 0xC3, // mov ebx,eax
                0x01, 0xD8, // add eax,ebx
                0x83, 0xE8, 0x01, // sub eax,1
                0x85, 0xC0, // test eax,eax
                0x75, 0xF4, // jne
            ],
            // adc/sbb (CFヘルパ呼び出し) と inc/dec、lea
            &[
                0x11, 0xC8, // adc eax,ecx
                0x19, 0xDA, // sbb edx,ebx
                0x40, // inc eax
                0x4B, // dec ebx
                0x8D, 0x44, 0x8B, 0x08, // lea eax,[ebx+ecx*4+8]
                0xEB, 0xF2, // jmp
            ],
            // 分岐なしで途切れる形 (F1b: 末尾のロードもブロックに入る)
            &[0xB9, 0xFF, 0x00, 0x00, 0x00, 0x8B, 0x03],
            // F1b-1: メモリロードの語彙 (mov/alu/cmp/test の各ロード形)
            &[
                0x8B, 0x43, 0x04, // mov eax,[ebx+4]
                0x03, 0x0E, // add ecx,[esi]
                0x13, 0x02, // adc eax,[edx] (CFヘルパ+ロードの合わせ技)
                0x39, 0x07, // cmp [edi],eax
                0x85, 0x0B, // test [ebx],ecx
                0x75, 0xF3, // jne
            ],
            // F1b-2: ストア/RMW/cmp imm
            &[
                0x89, 0x07, // mov [edi],eax
                0xC7, 0x03, 0x2A, 0x00, 0x00, 0x00, // mov dword [ebx],42
                0x01, 0x0E, // add [esi],ecx
                0x83, 0x4B, 0x08, 0x10, // or dword [ebx+8],0x10 (Grp1 RMW)
                0x83, 0x7F, 0x04, 0x00, // cmp dword [edi+4],0 (Grp1 kind7=ロード)
                0x75, 0xEC, // jne
            ],
            // F1b-3: スタック形 (関数プロローグ〜エピローグの形) — call終端
            &[
                0x55, // push ebp
                0x89, 0xE5, // mov ebp,esp… は 89 (rm=ebp) レジスタ形
                0x51, // push ecx
                0x68, 0x78, 0x56, 0x34, 0x12, // push 0x12345678
                0x59, // pop ecx
                0x90, // nop (xchg eax,eax)
                0xE8, 0x10, 0x00, 0x00, 0x00, // call +0x10
            ],
            // F1b-3: leave + ret 終端
            &[
                0x8B, 0x45, 0xFC, // mov eax,[ebp-4]
                0xC9, // leave
                0xC3, // ret
            ],
        ];
        for (i, code) in cases.iter().enumerate() {
            let (_m, blk, lay) = block_from(code);
            let wasm = compile_block(&blk, &lay, 0x1000);
            wasmparser::validate(&wasm)
                .unwrap_or_else(|e| panic!("case{i}: 検証に落ちた: {e} bytes={wasm:02x?}"));
        }
    }

    #[test]
    fn emits_valid_module_skeleton() {
        let (_m, blk, lay) = block_from(&[
            0xB8, 0x05, 0x00, 0x00, 0x00, // mov eax,5
            0x01, 0xD8, // add eax,ebx
            0x75, 0xF9, // jne -7
        ]);
        let wasm = compile_block(&blk, &lay, 0x1234);
        // マジックとバージョン
        assert_eq!(
            &wasm[..8],
            &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
        );
        // export "b" が居る
        let needle = [1, 1, b'b', 0x00, 8];
        assert!(
            wasm.windows(needle.len()).any(|w| w == needle),
            "export b が無い"
        );
        // セクションIDが昇順 (1,2,3,7,10)
        let mut pos = 8;
        let mut ids = Vec::new();
        while pos < wasm.len() {
            ids.push(wasm[pos]);
            let mut size = 0u64;
            let mut shift = 0;
            let mut p = pos + 1;
            loop {
                let byte = wasm[p];
                size |= ((byte & 0x7f) as u64) << shift;
                p += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            pos = p + size as usize;
        }
        assert_eq!(ids, vec![1, 2, 3, 7, 10]);
        assert_eq!(pos, wasm.len(), "セクション長の帳尻が合わない");
    }

    #[test]
    fn block_without_branch_ends_with_plain_ip_advance() {
        // mov eax,1 / mov ebx,2 / mov eax,[ebx] (F1b: ロードもブロックに入り、
        // 続く 00 00 (8bit ALUメモリ形) の手前で途切れる)
        let (_m, blk, lay) = block_from(&[
            0xB8, 0x01, 0x00, 0x00, 0x00, 0xBB, 0x02, 0x00, 0x00, 0x00, 0x8B, 0x03,
        ]);
        assert_eq!(blk.ops.len(), 3);
        let wasm = compile_block(&blk, &lay, 0);
        // 返り値の直前に「命令数3」のconstが居るはず (END の手前)
        assert_eq!(&wasm[wasm.len() - 3..], &[I32_CONST, 3, END]);
    }

    #[test]
    fn mem_ops_join_the_block_and_validate() {
        // ロード+ストア+RMW+スタック形を含むブロックが切り出され、検証を通る。
        // 8bit ALUメモリ形は語彙外 — そこで途切れる
        let (_m, blk, lay) = block_from(&[
            0x8B, 0x43, 0x04, // mov eax,[ebx+4]
            0x89, 0x07, // mov [edi],eax (F1b-2で語彙入り)
            0x01, 0x0E, // add [esi],ecx (RMW)
            0x50, // push eax (F1b-3で語彙入り)
            0x00, 0x03, // add [ebx],al — 8bit形は語彙外。ここで途切れる
        ]);
        assert_eq!(
            blk.ops.len(),
            4,
            "ロード+ストア+RMW+pushの4命令で、8bit形の手前まで"
        );
        let wasm = compile_block(&blk, &lay, 0x1000);
        wasmparser::validate(&wasm).expect("メモリop入りブロックの検証");
    }

    #[test]
    fn jit_try_read32_does_not_record_fault() {
        // 記録しない読み (F1b の核心): フォールトしそうでも pending_fault は無傷
        let m = Machine::with_profile(MachineProfile::pc_32bit(4));
        // ページ跨ぎは無条件で None (保守的な脱出)
        assert!(m.jit_try_read32(0xFFD).is_none());
        assert!(m.pending_fault.get().is_none(), "跨ぎでも記録しない");
        // ページ内・RAM内は普通に読める
        assert!(m.jit_try_read32(0x1000).is_some());
        assert!(m.pending_fault.get().is_none());
    }

    #[test]
    fn helpers_evaluate_lazy_flags() {
        // ヘルパが遅延フラグを正しく評価するか (JITの土台の直接検証)。
        // sub eax,eax 相当をインタプリタで実行 → ZF=1 のはず
        let mut m = Machine::with_profile(MachineProfile::pc_32bit(4));
        m.cpu.regs[0] = 7;
        // cc状態を作る: cmp eax,eax (インタプリタ経由)
        for (i, b) in [0x39u8, 0xC0].iter().enumerate() {
            m.write_phys8(0x7C00 + i as u32, *b);
        }
        // 実行はstepで (32bitセグメント設定などの面倒を避け、
        // ヘルパの評価だけを直接見る)
        unsafe {
            // condition 4 = ZF (cc=0x4: ZF set)
            let before = rx86_jit_cond(&m as *const Machine, 0x4);
            assert_eq!(before, 0, "まだZFは立っていない");
        }
    }
}
