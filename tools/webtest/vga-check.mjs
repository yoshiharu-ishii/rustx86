// mode 13h がwasmの窓から見えるかの検証ハーネス。
//   node tools/webtest/vga-check.mjs
// asm/mode13.bin (パレット3色 + グラデーション) をブートし、
// video_mode / fb_ptr / palette がブラウザ側の描き手に渡す値そのものを
// 返すことを確かめる。exit 0 = 全部一致。
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
// initの戻り値からリニアメモリを取る (machine.js と同じ作法)
const wasm = await mod.default({ module_or_path: wasmBytes });
globalThis.__rustx86_panic ??= (msg) => console.error('[panic]', msg);
mod.install_panic_hook();

const sector = new Uint8Array(readFileSync(join(root, 'asm/mode13.bin')));
const emu = new mod.Emulator(sector);
emu.run(100_000);

const fail = (msg) => {
  console.error('NG:', msg);
  process.exit(1);
};

if (!emu.halted()) fail('HLT到達せず');
if (emu.video_mode() !== 0x13) fail(`video_mode=${emu.video_mode()}`);
if (emu.fb_len() !== 320 * 200) fail(`fb_len=${emu.fb_len()}`);

// FB: 先頭行は 0..255 のグラデーション、2行目の頭は色16,17,18。
// ビューはポインタを取った後に作る (growでメモリが動くと古いビューは死ぬ)
const fb = new Uint8Array(wasm.memory.buffer, emu.fb_ptr(), emu.fb_len());
for (let x = 0; x < 256; x++) {
  if (fb[x] !== x) fail(`画素(${x},0)=${fb[x]}`);
}
if (fb[320] !== 16 || fb[321] !== 17 || fb[322] !== 18) {
  fail(`2行目=${fb[320]},${fb[321]},${fb[322]}`);
}

// パレット: 色16=赤 色17=緑 色18=青 (6bit値)。色1はEGA既定の青
const pal = emu.palette();
const c = (i) => [pal[i * 3], pal[i * 3 + 1], pal[i * 3 + 2]].join(',');
if (c(16) !== '63,0,0') fail(`色16=${c(16)}`);
if (c(17) !== '0,63,0') fail(`色17=${c(17)}`);
if (c(18) !== '0,0,63') fail(`色18=${c(18)}`);
if (c(1) !== '0,0,42') fail(`色1(EGA青)=${c(1)}`);

console.log('mode13: video_mode / フレームバッファ / パレット 全部一致 (wasm経由)');
