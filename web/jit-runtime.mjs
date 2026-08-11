// F1a: JITトランポリン (ADR-0008、call_indirect版)。
//
// 焼き上がったバイト列を instantiate し、その関数 (export "b") を
// **core の関数テーブル** (__indirect_function_table) へ table.set する。
// core (Rust) はそのスロット番号を関数ポインタへ transmute して呼ぶので、
// 実行時は **call_indirect が1個出るだけ — JSへの往復はゼロ**。
// instantiate だけは JS にしかできないので、そこだけをここが担う。
//
//   import { setupJit, pumpJit } from './jit-runtime.mjs';
//   const exports = await mod.default(...);
//   setupJit(emu, exports);   // テーブルとimportを用意
//   emu.jit_enable();
//   // ...run_slice のたびに pumpJit(emu)

/** 生成モジュールが共有する import と、core の関数テーブル */
let ctx = null;

/**
 * @param emu      Emulator (jit_function_table を持つ)
 * @param exports  mod.default() が返す生exports (memory / rx86_jit_cf / rx86_jit_cond)
 */
export function setupJit(emu, exports) {
  ctx = {
    table: emu.jit_function_table(), // = __indirect_function_table (growable)
    imports: {
      e: {
        m: exports.memory,
        cf: exports.rx86_jit_cf,
        cond: exports.rx86_jit_cond,
      },
    },
  };
}

/**
 * その回のスライスで焼き上がったブロックを instantiate して関数テーブルへ据える。
 * run_slice の後に呼ぶ。焼く数は少数 (Linuxブートで数千が上限) なので同期でよい。
 * @returns 据え付けた数
 */
export function pumpJit(emu) {
  let installed = 0;
  for (;;) {
    const bytes = emu.drain_job();
    if (!bytes) break;
    try {
      const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes), ctx.imports);
      // 関数テーブルを1つ伸ばし、生成関数をそのスロットへ。core はこの
      // スロット番号で call_indirect する (JS境界なし)
      const slot = ctx.table.grow(1);
      ctx.table.set(slot, inst.exports.b);
      emu.install_block(slot);
      installed++;
    } catch (e) {
      // 焼き損じは捨てる — coreはインタプリタで走り続ける (退路は常にある)
      console.error('jit instantiate failed:', e);
      emu.discard_job();
    }
  }
  return installed;
}

/** テスト・再起動用 (テーブルはインスタンスごとに新しいので状態は持たない) */
export function resetJit() {
  ctx = null;
}
