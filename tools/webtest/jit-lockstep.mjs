// JITの決定性をjit-checkより強く審判する: interp と JIT を並走させ、
// **スナップショット全体 (CPU+装置+RAM) をバイト比較**する。
//   node tools/webtest/jit-lockstep.mjs
// exit 0 = ブート完了まで全一致 / exit 1 = 食い違い (区間と差分を絞り込んで表示)。
//
// jit-check (シリアル+量子化tsc) では見えない食い違いを捕まえる。実績:
// F1aから潜んでいた「JIT清算のページ跨ぎ着地でtsc+1過払い」は、シリアルには
// µs未満でしか現れずゲートを通り抜けたが、これは一発で捕まえた —
// signature は「同一tscでJIT側だけipが1命令手前」(PR #60)。
//
// 決定的だから成立する二分探索: 粗い刻みで食い違い区間を見つけたら、
// **新しいペアを作り直して**その区間の頭まで走らせ、刻みを1/100にして再走。
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { setupJit, pumpJit, resetJit } from '../../web/jit-runtime.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const modUrl = pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href;
const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinux-lts')));
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));

async function mk(useJit, tag) {
  // クエリ付きimportでmodule-level stateを分離 (メモリ・JIT台帳を独立させる)
  const mod = await import(`${modUrl}?run=${tag}`);
  const exports = await mod.default({ module_or_path: wasmBytes });
  resetJit();
  const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
  if (useJit) { setupJit(emu, exports); emu.jit_enable(); }
  return { emu, useJit };
}

function runTo(m, target) {
  // run_slice は予算ちょうどで止まる (step_budgeted) ので tsc に正確に合流できる
  for (;;) {
    const t = m.emu.tsc();
    if (t >= target) return;
    m.emu.run_slice(Math.min(target - t, 50_000_000));
    if (m.useJit) pumpJit(m.emu);
    m.emu.serial_out(); // シリアルは吐き捨て (状態比較はスナップショットで)
  }
}

function firstDiff(a, b) {
  if (a.length !== b.length) return -2; // RAMが違うとRLE圧縮後の長さから違う
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return i;
  return -1;
}

/** スナップショット先頭のレジスタ・ip・flagsを人間向けに */
function parseState(s) {
  const b = Buffer.from(s);
  const names = ['eax', 'ecx', 'edx', 'ebx', 'esp', 'ebp', 'esi', 'edi'];
  const o = {};
  names.forEach((n, i) => (o[n] = b.readUInt32LE(10 + 4 * i).toString(16)));
  o.ip = b.readUInt32LE(54).toString(16);
  o.flags = b.readUInt32LE(58).toString(16);
  return o;
}

// 1ラウンド: base まで走ってから step 刻みで比較。食い違った区間の頭を返す
let round = 0;
async function compare(base, step, cap) {
  const A = await mk(false, `i${round}`);
  const B = await mk(true, `j${round}`);
  round++;
  runTo(A, base); runTo(B, base);
  for (;;) {
    const t0 = A.emu.tsc();
    if (t0 >= cap) return { t0: null };
    runTo(A, t0 + step); runTo(B, t0 + step);
    const sa = A.emu.save_state(), sb = B.emu.save_state();
    const d = firstDiff(sa, sb);
    if (d !== -1) return { t0, t1: A.emu.tsc(), d, sa, sb };
  }
}

const CAP = 3_000_000_000;
let base = 0, step = 10_000_000;
for (;;) {
  const r = await compare(base, step, CAP);
  if (r.t0 === null) {
    console.log('OK  ブート完了までスナップショット全一致 (10M命令刻み)');
    process.exit(0);
  }
  console.log(`NG  食い違い: tsc (${r.t0}, ${r.t1}] snapshotオフセット=${r.d} (step=${step})`);
  if (step <= 200) {
    console.log('interp:', JSON.stringify(parseState(r.sa)));
    console.log('jit   :', JSON.stringify(parseState(r.sb)));
    console.log(`確定: tsc=${r.t0} から ${step} 命令以内`);
    process.exit(1);
  }
  base = r.t0;
  step = Math.max(100, Math.floor(step / 100));
}
