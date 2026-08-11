// ヘッドレスでWASMを駆動し、Linuxがシェルまで起動するか確かめる検証ハーネス。
// ネイティブの run.rs と同じ手順を Node + wasm で踏む。
//   node tools/webtest/headless.mjs
// exit 0 = シェル到達 (+ snakeの盤面描画も確認)、exit 1 = 失敗
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasm = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
// pathToFileURL: Windowsでは C:\ 絶対パスを裸で import() に渡せない
// (ERR_UNSUPPORTED_ESM_URL_SCHEME — ドライブ文字がURLスキームに見える)
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
await mod.default({ module_or_path: wasm });

// 既定は bzImage — ブラウザの既定と同じ、自己解凍ステブごと実行する本物の起動。
// KERNEL=vmlinux で直接ロードの近道を測れる (経路比較用、docs/reference/perf.md)
const kernel = new Uint8Array(
  (() => {
    if (process.env.KERNEL === 'vmlinux') {
      return readFileSync(join(root, 'images/vmlinux-lts'));
    }
    return readFileSync(join(root, 'images/vmlinuz-lts'));
  })(),
);
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));
console.log(`kernel ${(kernel.length/1e6).toFixed(1)}MB, initrd ${(initrd.length/1e6).toFixed(1)}MB`);

const t0 = Date.now();
const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
let serial = '', n = 0;
const CHUNK = 50_000_000, CAP = 3_000_000_000;
let banner = false;
while (n < CAP) {
  emu.run_slice(CHUNK); n += CHUNK;
  const o = emu.serial_out();
  if (o.length) serial += Buffer.from(o).toString('latin1');
  const tr = emu.trap_reason();
  if (tr) { console.log(`[TRAP] ${tr}`); break; }
  if (serial.includes('busybox shell')) { banner = true; break; }
}
const bootSecs = ((Date.now() - t0) / 1000).toFixed(1);
const mips = (n / ((Date.now() - t0) / 1000) / 1e6).toFixed(1);
console.log(`banner=${banner}  instrs=${(n/1e6)|0}M  time=${bootSecs}s  ${mips}MIPS`);

let snakeOk = false;
if (banner) {
  emu.serial_in(Buffer.from('snake\n', 'latin1'));
  for (let i = 0; i < 40; i++) {
    emu.run_slice(CHUNK);
    const o = emu.serial_out();
    if (o.length) serial += Buffer.from(o).toString('latin1');
    if (serial.includes('score')) { snakeOk = true; break; }
  }
  console.log(`snake_board=${snakeOk}`);
}
process.exit(banner && snakeOk ? 0 : 1);
