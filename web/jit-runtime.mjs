// F1a: JITトランポリン (ADR-0008)。
//
// coreは「ブロック頭で globalThis.rx86_call_block(idx) を呼べば n 命令進む」
// という契約だけを知っている。instantiate はJSにしかできないので、
// 焼き上がったバイト列をここで WebAssembly.Instance にして関数配列へ足し、
// core に添字を返す。生成モジュールはメインの**同じメモリ**を共有し、
// フラグ評価ヘルパ (cf/cond) もメインからimportする。
//
//   import { setupJit } from './jit-runtime.mjs';
//   const exports = await mod.default(...);   // wasm-bindgen の生exports
//   setupJit(mod, exports);                    // globalThis に配線
//   emu.jit_enable();                          // coreにフックを挿す
//
// スライスのたびに pumpJit(emu) を呼ぶと、その回に焼けたブロックを据え付ける。

/** 生成ブロックの実体 (export "b") を添える配列。coreは添字で呼ぶ */
const blocks = [];

/**
 * トランポリンを globalThis に据える。
 * @param mod      wasm-bindgen のモジュール名前空間 (Emulatorを含む)
 * @param exports  mod.default() が返す生exports (memory / rx86_jit_cf / rx86_jit_cond)
 */
export function setupJit(_mod, exports) {
  // 生成モジュールが e.* としてimportする資源。全ブロックで共有
  globalThis.__rx86_jit_imports = {
    e: {
      m: exports.memory,
      cf: exports.rx86_jit_cf,
      cond: exports.rx86_jit_cond,
    },
  };
  // coreのcall_block(idx) の実体
  globalThis.rx86_call_block = (idx) => blocks[idx]();
}

/**
 * その回のスライスで焼き上がったブロックを instantiate して据え付ける。
 * run_slice の後に呼ぶ。焼く数は少数 (Linuxブートで数千が上限) なので同期でよい。
 * @returns 据え付けた数
 */
export function pumpJit(emu) {
  let installed = 0;
  for (;;) {
    const bytes = emu.drain_job();
    if (!bytes) break;
    try {
      const inst = new WebAssembly.Instance(
        new WebAssembly.Module(bytes),
        globalThis.__rx86_jit_imports,
      );
      const idx = blocks.length;
      blocks.push(inst.exports.b);
      emu.install_block(idx);
      installed++;
    } catch (e) {
      // 焼き損じは捨てる — coreはインタプリタで走り続ける (退路は常にある)。
      // 開発中は握りつぶさず出す
      console.error('jit instantiate failed:', e);
      emu.discard_job();
    }
  }
  return installed;
}

/** テスト・再起動用: 据え付け済みブロックを捨てる */
export function resetJit() {
  blocks.length = 0;
}
