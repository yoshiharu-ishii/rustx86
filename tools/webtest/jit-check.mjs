// F1a: JITの決定性ゲート。
//   node tools/webtest/jit-check.mjs
// exit 0 = JIT on と off で「シェル到達までの命令数」と「シリアル出力」が
// **ビット同一** かつ据え付けが起きた。exit 1 = 食い違い or JIT不発。
//
// これがF1aの最終審判 — 生成コードとインタプリタが同じ意味を持つことの証明。
// 命令数決定性 (docs/reference/perf.md の柱) をJITが崩していないかをCIで見張る。
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { setupJit, pumpJit, resetJit } from '../../web/jit-runtime.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const modUrl = pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href;
const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinux-lts')));
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));

// 各ランごとに真っさらなwasmインスタンスを用意する (メモリ・JIT台帳を独立させる)
async function freshModule() {
  // クエリ付きimportで module-level state を作り直す
  const mod = await import(modUrl + '?run=' + Math.random());
  const exports = await mod.default({ module_or_path: wasmBytes });
  return { mod, exports };
}

async function boot(useJit) {
  const { mod, exports } = await freshModule();
  resetJit();
  const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
  if (useJit) { setupJit(emu, exports); emu.jit_enable(); }

  let serial = '', n = 0;
  const CHUNK = 50_000_000, CAP = 3_000_000_000;
  let banner = false;
  while (n < CAP) {
    emu.run_slice(CHUNK); n += CHUNK;
    if (useJit) pumpJit(emu);
    const o = emu.serial_out();
    if (o.length) serial += Buffer.from(o).toString('latin1');
    const tr = emu.trap_reason();
    if (tr) { console.log(`[TRAP ${useJit ? 'jit' : 'interp'}] ${tr}`); break; }
    if (serial.includes('busybox shell')) { banner = true; break; }
  }
  // 命令数は「シェル到達までTSCが進んだ量」で数える (決定的・精密)
  const instrs = emu.tsc();
  const installed = useJit ? emu.jit_installed() : 0;
  return { banner, serial, instrs, installed };
}

const off = await boot(false);
const on = await boot(true);

let ok = true;
function check(name, a, b) {
  const eq = a === b;
  if (!eq) ok = false;
  console.log(`${eq ? 'OK ' : 'NG '} ${name}: interp=${a} jit=${b}`);
}

check('banner到達', off.banner, on.banner);
check('シェル到達までの命令数 (TSC・精密)', off.instrs, on.instrs);
check('シリアル出力(全文)', off.serial, on.serial);
console.log(`jit据え付け: ${on.installed} ブロック`);
if (on.installed === 0) { ok = false; console.log('NG  JITが一度も発火していない'); }

process.exit(ok && on.banner ? 0 : 1);
