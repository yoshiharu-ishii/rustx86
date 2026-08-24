// W1 の定規: wasm JIT の**焼き直しの嵐**を帳簿で測る soak。
//   SNAP=path/to/dsl-login.snap ISO=web/dsl-2024.rc7.iso node tools/webtest/jit-soak.mjs
//   (SNAP 無しなら images/vmlinux-lts + initramfs-mini を起動して回す)
//
// なぜ帳簿か: wasm の秒は配置ノイズで嘘をつく (perf.md 測定の規律、[native-ruler-only])。
// スロット表の効きは**同じ命令数で何回焼いたか**という決定的な数字で裁く。
//   baked   焼いた回数 (多い = 焼き直しの嵐)
//   install 据え付いた数
//   confl   据わっているブロックを衝突で追い出した回数 (W1 が狙う的)
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { setupJit, pumpJit, resetJit } from '../../web/jit-runtime.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const modUrl = pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href;
const BUDGET = Number(process.env.INSTRS ?? 8e9);
const SNAP = process.env.SNAP ?? '';
const ISO = process.env.ISO ?? '';
// 控えから戻した機械に打つ手順 (DSL: ログイン → bochs-drm → X)
const SCRIPT = process.env.SCRIPT ?? 'root\nroot\nmodprobe bochs-drm\nstartx\n';

const mod = await import(modUrl);
const exports = await mod.default({ module_or_path: wasmBytes });
resetJit();

let emu;
if (SNAP) {
  emu = mod.Emulator.from_snapshot(new Uint8Array(readFileSync(SNAP)));
  if (ISO && emu.cd_wanted()) emu.cd_attach(new Uint8Array(readFileSync(ISO)));
} else {
  emu = mod.Emulator.from_bzimage(
    new Uint8Array(readFileSync(join(root, 'images/vmlinux-lts'))),
    new Uint8Array(readFileSync(join(root, 'images/initramfs-mini'))),
    'console=ttyS0',
    128,
  );
}
setupJit(emu, exports);
emu.jit_enable();
// 控えから戻した機械の TSC は**続き**から始まる (DSL なら 45.6G)。
// この soak で走った分だけを数える
const tsc0 = emu.tsc();

// **手順は合図を見てから打つ。** 控えから戻した直後でもログインは 1G 命令かかるので、
// 一定間隔で打つと取りこぼしてアイドルの機械を測ることになる (実際に踏んだ)
const screen = () => {
  const cells = new Uint8Array(exports.memory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
  let s = '';
  for (let i = 0; i < cells.length; i += 2) s += String.fromCharCode(cells[i]);
  return s;
};
// [待つ合図 (文字列 or 述語), 打つ文字列]。
// **プロンプトの数を数える** — `~#` は打った後も画面に残るので、
// 文字列一致だけだと直前のコマンドの終了を待たずに次を打ってしまう
const prompts = (s) => (s.match(/~#/g) ?? []).length;
const STEPS = SNAP
  ? [
      ['login:', 'root\n'],
      ['assword:', 'root\n'],
      [(s) => prompts(s) >= 1, 'modprobe bochs-drm\n'],
      // **modprobe が通るとコンソールは fbcon (LFB) に移る** — 以後プロンプトは
      // テキスト VRAM に出ないので、合図は「画面が LFB になったか」で取る
      () => emu.lfb_on(),
    ]
  : [];
let step = 0;
const t0 = performance.now();
const CHUNK = 50_000_000;
let n = 0;
while (n < BUDGET) {
  emu.run_slice(CHUNK);
  n += CHUNK;
  pumpJit(emu);
  if (step < STEPS.length) {
    const [wait, send] = Array.isArray(STEPS[step]) ? STEPS[step] : [STEPS[step], 'startx\n'];
    const hit = typeof wait === 'function' ? wait(screen()) : !wait || screen().includes(wait);
    if (hit) {
      for (const ch of send) emu.type_text(ch);
      console.log(`[${(n / 1e9).toFixed(1)}G] 手順${step + 1} → ${JSON.stringify(send)}`);
      step++;
    }
  }
  const tr = emu.trap_reason();
  if (tr) {
    console.log(`[TRAP] ${tr}`);
    break;
  }
}
if (step < STEPS.length) {
  console.log(`⚠ 手順が途中で終わった (${step}/${STEPS.length}) — 画面末尾:`);
  console.log(screen().replace(/\s+$/, '').split('\n').slice(-3).join('\n'));
}
const wall = (performance.now() - t0) / 1000;
const tsc = emu.tsc() - tsc0;
const baked = emu.jit_baked();
const installed = emu.jit_installed();
const confl = emu.jit_conflicts?.() ?? -1;
console.log(
  `命令 ${(tsc / 1e9).toFixed(2)}G / ${wall.toFixed(1)}s = ${(tsc / wall / 1e6).toFixed(0)} MIPS`,
);
console.log(
  `帳簿: baked=${baked} install=${installed} confl=${confl} recycled=${emu.jit_recycled()} ` +
    `カバレッジ=${((emu.jit_instrs() / tsc) * 100).toFixed(1)}%`,
);
console.log(`焼き/据付 = ${(baked / Math.max(1, installed)).toFixed(1)} 倍 (小さいほど良い)`);
