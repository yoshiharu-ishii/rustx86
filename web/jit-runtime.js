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
/** assign_slots が返す「テーブルを伸ばして新しく取れ」の印 (core の jit::FRESH と同じ値) */
const FRESH = 0xffffffff;

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
        ld32: exports.rx86_jit_ld32, // F1b: メモリロード (フォールト脱出つき)
        st32: exports.rx86_jit_st32, // F1b-2: ストア
        rmw32: exports.rx86_jit_rmw32, // F1b-2: alu [mem], b (read→alu→write)
        push32: exports.rx86_jit_push32, // F1b-3: スタック形
        pop32: exports.rx86_jit_pop32,
        leave: exports.rx86_jit_leave,
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
  releaseJit(emu);
  for (;;) {
    // バッチ (1モジュール=最大4096ブロック)。モジュール数を減らすのが本体 —
    // 1ブロック=1モジュールはエンジンのコンパイル固定費でブート+19sだった
    const bytes = emu.drain_batch();
    if (!bytes) break;
    // 据え付け先の添字。**退去したブロックの添字を使い回す** — テーブルが
    // 伸びる一方だと、退去済みブロックのモジュール (Code) が添字に掴まれた
    // まま GC されず、X のような焼き直しの続く負荷で 6〜8 分でレンダラが
    // 落ちた (V8 の ExternalEntityTable 枯渇 = Chrome エラーコード 5)。
    // FRESH (0xFFFFFFFF) の分だけ伸ばし、grow した番号を当てる
    const slots = emu.assign_slots();
    const n = slots.length;
    try {
      const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes), ctx.imports);
      let fresh = 0;
      for (let i = 0; i < n; i++) if (slots[i] === FRESH) fresh++;
      let next = fresh ? ctx.table.grow(fresh) : 0;
      for (let i = 0; i < n; i++) {
        if (slots[i] === FRESH) slots[i] = next++;
        ctx.table.set(slots[i], inst.exports['b' + i]);
      }
      emu.install_batch(slots);
      installed += n;
    } catch (e) {
      // 焼き損じはバッチごと捨てる — coreはインタプリタで走り続ける
      console.error('jit instantiate failed:', e);
      emu.discard_batch(slots);
    }
  }
  return installed;
}

/**
 * 退去したブロックの添字を null にして、モジュールを GC に渡す。
 * pump の頭で毎回呼ぶ。jit_disable (flush) の後にも呼ぶ — 添字に関数が
 * 据わったままだと、退去しても 4096 ブロック分の Code が残り続ける
 */
export function releaseJit(emu) {
  if (!ctx) return;
  const freed = emu.take_freed();
  for (let i = 0; i < freed.length; i++) ctx.table.set(freed[i], null);
}

/** テスト・再起動用 (テーブルはインスタンスごとに新しいので状態は持たない) */
export function resetJit() {
  ctx = null;
}
