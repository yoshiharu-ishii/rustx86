// 北極星ベンチ: v86 (copy.sh) に rustx86 と**同じ** vmlinuz-lts + initramfs-mini を
// 食わせ、同じ「busybox shell 到達」までの壁時計と実行命令数 (実MIPS) を測る。
// v86はx86→wasm JITを積んだ同類の完成形 — この数字が「wasmで登れる山の高さ」の指針。
//
// 準備 (作業ディレクトリはどこでもよい):
//   mkdir -p /tmp/v86bench && cd /tmp/v86bench
//   npm i v86
//   curl -sLO https://raw.githubusercontent.com/copy/v86/master/bios/seabios.bin
//   curl -sLO https://raw.githubusercontent.com/copy/v86/master/bios/vgabios.bin
//   node <rustx86>/tools/webtest/v86-bench.mjs
//
// 実測 (2026-08-12、M1): 4.4〜5.0s / 805〜807M命令 / **160〜185 MIPS**
// (同時刻の rustx86 wasm headless: 14.5s / 600M / 41 MIPS — 約4倍差)
import { V86 } from "v86";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const RX = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const toAB = (p) => {
  const b = readFileSync(p);
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
};

const t0 = performance.now();
const emulator = new V86({
  wasm_path: join(process.cwd(), "node_modules/v86/build/v86.wasm"),
  bios: { buffer: toAB("./seabios.bin") },
  vga_bios: { buffer: toAB("./vgabios.bin") },
  bzimage: { buffer: toAB(`${RX}/images/vmlinuz-lts`) },
  initrd: { buffer: toAB(`${RX}/images/initramfs-mini`) },
  cmdline: "console=ttyS0",
  memory_size: 128 * 1024 * 1024,
  vga_memory_size: 2 * 1024 * 1024,
  autostart: true,
  disable_keyboard: true,
  disable_mouse: true,
});

// 命令カウンタはu32で折り返すので、定期サンプリングで積算する
let total = 0n;
let last = 0;
function sample() {
  const now = emulator.get_instruction_counter() >>> 0;
  let d = now - last;
  if (d < 0) d += 0x1_0000_0000;
  total += BigInt(d);
  last = now;
}
const timer = setInterval(sample, 200);

let serial = "";
let done = false;
emulator.add_listener("serial0-output-byte", (byte) => {
  serial += String.fromCharCode(byte);
  if (!done && serial.includes("busybox shell")) {
    done = true;
    sample();
    clearInterval(timer);
    const secs = (performance.now() - t0) / 1000;
    const mips = Number(total) / secs / 1e6;
    console.log(`banner=true  time=${secs.toFixed(1)}s  instrs=${(Number(total) / 1e6).toFixed(0)}M  ${mips.toFixed(1)}MIPS`);
    emulator.destroy();
    process.exit(0);
  }
});

setTimeout(() => {
  console.log("TIMEOUT 180s。シリアル末尾:");
  console.log(serial.slice(-600));
  process.exit(1);
}, 180_000);
