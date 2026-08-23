// スナップショット v16 の検証 — CD から起動した機械 (ATAPI + VGA) を控えて戻し、
// 像を挿し直すと CD が読めることを wasm の窓から確かめる。
//   node tools/webtest/snap-cd.mjs            (web/Core-current.iso を使う)
//   ISO=web/xxx.iso PROMPT='login:' node tools/webtest/snap-cd.mjs
// exit 0 = 復元後に /dev/sr0 のセクタ 16 (CD001) が読めた、exit 1 = 失敗
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
const wasm = await mod.default({ module_or_path: wasmBytes });
globalThis.__rustx86_panic ??= (msg) => console.error('[panic]', msg);
mod.install_panic_hook();

const isoPath = process.env.ISO ?? join(root, 'web/Core-current.iso');
const prompt = process.env.PROMPT ?? 'tc@box';
const iso = new Uint8Array(readFileSync(isoPath));
const fail = (msg) => {
  console.error('NG:', msg);
  process.exit(1);
};

const screen = (emu) => {
  const cells = new Uint8Array(wasm.memory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
  let s = '';
  for (let i = 0; i < cells.length; i += 2) s += String.fromCharCode(cells[i]);
  return s;
};
const waitFor = (emu, needle, budget) => {
  let spent = 0;
  while (spent < budget) {
    emu.run(20_000_000);
    spent += 20_000_000;
    if (screen(emu).includes(needle)) return spent;
  }
  return -1;
};

const t0 = performance.now();
let emu = mod.Emulator.from_iso(iso, 128);
let n = waitFor(emu, prompt, 6_000_000_000);
if (n < 0) fail(`${prompt} が出ない:\n${screen(emu)}`);
console.log(`起動: ${prompt} まで ${(n / 1e6).toFixed(0)}M 命令 (${((performance.now() - t0) / 1000).toFixed(1)}s)`);

// 控える (像は入らない)
const snap = emu.save_state();
console.log(`控え: ${(snap.length / 1024 / 1024).toFixed(1)} MB (ISO は ${(iso.length / 1024 / 1024).toFixed(1)} MB)`);
if (snap.length > iso.length) fail('控えに像が混ざっている');
emu.free?.();

// 戻す → 像を待っている → 挿し直す
const t1 = performance.now();
emu = mod.Emulator.from_snapshot(snap);
if (!emu.cd_wanted()) fail('復元した機械が CD を待っていない');
emu.cd_attach(iso);
if (emu.cd_wanted()) fail('挿し直したのに待っている');
console.log(`復元: ${((performance.now() - t1) / 1000).toFixed(2)}s`);

// 挿し直した CD の中身が読める (セクタ 16 = PVD、先頭 "\x01CD001")
const MARK = 'SNAPCD';
for (const ch of `dd if=/dev/sr0 bs=2048 skip=16 count=1 2>/dev/null | head -c 6 | od -An -c; echo ${MARK}\n`) {
  emu.type_text(ch);
  emu.run(300_000);
}
n = waitFor(emu, MARK, 2_000_000_000);
const out = screen(emu);
if (n < 0) fail(`コマンドが終わらない:\n${out}`);
const line = out.slice(0, out.lastIndexOf(MARK)).replace(/ +/g, ' ');
if (!/C D 0 0 1/.test(line)) fail(`CD001 が読めない:\n${out}`);
console.log('OK: 復元した機械で /dev/sr0 のセクタ 16 に CD001 を読めた');
