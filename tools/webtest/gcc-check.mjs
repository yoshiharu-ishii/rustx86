// wasm版jcmd — gcc課程のon/off差分定規 (窓命令数+シリアルFNV)。
//
//   node tools/webtest/gcc-check.mjs off
//   node tools/webtest/gcc-check.mjs on
//
// 2本のFNVが一致 = JITの条件インライン (g2還流) 込みで526M命令ビット同一。
// jit-check (ブート) が踏まない8bit/分岐密度の高いcc1経路を差分で見張る。
// images/disk-gcc.img が要る (無ければ焼き方は docs/reference 参照)
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { dirname } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const { setupJit, pumpJit } = await import(pathToFileURL(join(root, 'web/jit-runtime.js')).href);
const wasmBytes = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
const exports = await mod.default({ module_or_path: wasmBytes });
const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinux-lts')));
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));
const disk = new Uint8Array(readFileSync(join(root, 'images/disk-gcc.img')));
const jit = process.argv[2] !== 'off';

const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
emu.blk_attach(disk);
if (jit) { setupJit(emu, exports); emu.jit_enable(); }

const CMD = 'time gcc /hello.c -o /tmp/h1; time gcc /hello.c -o /tmp/h2; printf "DONE%s\\n" MARK\n';
const SLICE = 2_000_000;
let serial = Buffer.alloc(0);
let n = 0, fed = false, winStart = 0, wallStart = 0;
const t0 = performance.now();
while (n < 60_000_000_000) {
  emu.run_slice(SLICE); n += SLICE;
  if (jit) pumpJit(emu);
  const o = emu.serial_out();
  if (o.length) serial = Buffer.concat([serial, Buffer.from(o)]);
  const s = serial.toString('latin1');
  if (!fed && s.includes('busybox shell')) {
    emu.serial_in(new TextEncoder().encode(CMD));
    fed = true; winStart = n; wallStart = performance.now();
  }
  if (fed && s.includes('DONEMARK')) { emu.run_slice(10_000_000); n += 10_000_000; break; }
}
// FNV-1a 64bit
let h = 0xcbf29ce484222325n;
for (const b of serial) { h ^= BigInt(b); h = (h * 0x100000001b3n) & 0xffffffffffffffffn; }
const win = n - winStart;
const wall = (performance.now() - wallStart) / 1000;
console.log(`${process.argv[2]} 窓命令数=${win} FNV=${h.toString(16).padStart(16, '0')} 窓=${wall.toFixed(2)}s (${(win / 1e6 / wall).toFixed(1)} MIPS) 全体=${((performance.now() - t0) / 1000).toFixed(1)}s`);
