//! F1c 関門プローブ — Cranelift で「uopブロック相当」を焼いて、
//! 固定費が回収可能かを実測する (wasm版 tools/webtest/jit-probe.mjs と同じ問い)。
//!
//! 測るもの:
//!   1. 焼きコスト: IR構築→コンパイル→finalize を1ブロックぶん (サイズ別)
//!   2. 呼び出しコスト: 焼いた関数を関数ポインタ経由で呼ぶ1回あたりのns
//!   3. ブロック内のuop単価: レジスタ配列への load/ALU/store 連鎖のns/uop
//!
//! 比較の物差し: インタプリタは ~15〜40ns/命令 (M1、perf.md)。
//! wasm版の答えは 焼き6.3µs/ブロック・呼び出し~7ns だった。

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::time::Instant;

/// uop N個ぶんの「ブロック」を1本焼く。
/// 形は F1a の語彙に寄せる: regs配列 (i32×8) のロード → ALU → ストアの連鎖。
/// 戻り値は実行したuop数 (清算の写し)
static NEXT_TAG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn build_block(module: &mut JITModule, fbc: &mut FunctionBuilderContext, n: usize) -> *const u8 {
    let tag = NEXT_TAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut ctx = module.make_context();
    let ptr = module.target_config().pointer_type();
    ctx.func.signature.params.push(AbiParam::new(ptr)); // ®s
    ctx.func.signature.returns.push(AbiParam::new(types::I32)); // 実行数

    let mut b = FunctionBuilder::new(&mut ctx.func, fbc);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let regs = b.block_params(entry)[0];

    // reg[i%8] = reg[i%8] + reg[(i+1)%8] ^ 定数 … を N 連鎖
    let flags = MemFlags::trusted();
    for i in 0..n {
        let off_a = ((i % 8) * 4) as i32;
        let off_b = (((i + 1) % 8) * 4) as i32;
        let a = b.ins().load(types::I32, flags, regs, off_a);
        let c = b.ins().load(types::I32, flags, regs, off_b);
        let s = b.ins().iadd(a, c);
        let k = b.ins().iconst(types::I32, (i as i64) & 0xFF);
        let x = b.ins().bxor(s, k);
        b.ins().store(flags, x, regs, off_a);
    }
    let ret = b.ins().iconst(types::I32, n as i64);
    b.ins().return_(&[ret]);
    b.finalize();

    let id = module
        .declare_function(&format!("blk{tag}_{n}"), Linkage::Local, &ctx.func.signature)
        .unwrap();
    module.define_function(id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();
    module.get_finalized_function(id)
}

fn probe(opt: &str) {
    let mut sb = settings::builder();
    sb.set("opt_level", opt).unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(sb))
        .unwrap();
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(builder);
    let mut fbc = FunctionBuilderContext::new();

    println!("---- opt_level={opt} ----");

    // ---- 1. 焼きコスト (サイズ別、各100本の平均) ----
    for n in [4usize, 8, 16, 40] {
        let t = Instant::now();
        let rounds = 100;
        let mut last = std::ptr::null();
        for r in 0..rounds {
            let _ = r;
            last = build_block(&mut module, &mut fbc, n);
        }
        let per = t.elapsed().as_nanos() as f64 / rounds as f64;
        println!("焼き {n:>2}uop: {:>7.1}µs/ブロック (最後={last:p})", per / 1000.0);
    }

    // ---- 2. 呼び出しコスト + uop単価 ----
    for n in [4usize, 8, 16, 40] {
        let f = build_block(&mut module, &mut fbc, n);
        let f: extern "C" fn(*mut u32) -> u32 = unsafe { std::mem::transmute(f) };
        let mut regs = [1u32, 2, 3, 4, 5, 6, 7, 8];
        // 温める
        for _ in 0..100_000 {
            f(regs.as_mut_ptr());
        }
        let calls = 10_000_000u64;
        let t = Instant::now();
        let mut acc = 0u32;
        for _ in 0..calls {
            acc = acc.wrapping_add(f(regs.as_mut_ptr()));
        }
        let ns = t.elapsed().as_nanos() as f64 / calls as f64;
        println!(
            "呼び {n:>2}uop: {ns:>6.2}ns/呼び出し = {:>5.2}ns/uop (検算 acc={acc} regs0={})",
            ns / n as f64,
            regs[0]
        );
    }
}

fn main() {
    println!("== F1c関門プローブ (Cranelift 0.116) ==");
    probe("none");
    probe("speed");
}
