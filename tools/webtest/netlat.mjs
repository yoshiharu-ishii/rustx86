// **遅延の定規** — ゲストが測る ping の往復時間を、16bit と 32bit の両方で出す。
//
//   RUSTX86_NET_E2E_URL='ws://127.0.0.1:8087/net?token=dev' node tools/webtest/netlat.mjs
//
// ## 宛先は 10.0.2.2 (SLiRPのゲートウェイ)
//
// **外に出ない。** wsslirpのnetstackが自分で答えるので、測っているのは
// 「ゲスト → 仮想NIC → WS → netstack → 折り返し」だけになる。実インターネット
// 宛だと経路の遅さが混ざって、自分の遅延が見えなくなる (throughputで踏んだ罠と同じ)。
//
// ## 何が効くのか — スライスの刻み
//
// フレームの出入りは**スライス境界でしか起きない**。1スライスが実時間で t ms なら、
// 往復には最低でも t×2 かかる。だから 32bit 側は刻みを振って、RTTが刻みに
// 比例するかを見る。比例していれば「遅延の主因は刻み」で、頭打ちなら別の要因がいる。
import { readFileSync } from 'node:fs';
import { setImmediate as yieldLoop, setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const url = process.env.RUSTX86_NET_E2E_URL;
if (!url) {
  console.log('RUSTX86_NET_E2E_URL を設定すると実行する (wsslirpdが要る)');
  process.exit(0);
}
const TARGET = process.env.RUSTX86_NET_E2E_PING || '10.0.2.2';
// 32bit側で振る刻み。既定はブラウザのワーカーが忙しいときの値 (~8ms) を挟む
const SLICES = (process.env.SLICES || '10000000,1000000,300000,100000')
  .split(',')
  .map(Number);

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
const init = await mod.default({
  module_or_path: readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm')),
});
globalThis.__rustx86_panic ??= (msg) => console.error('[panic]', msg);
mod.install_panic_hook();

const MAC = new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
const INSTR_PER_GUEST_MS = (1_193_182 * 64) / 1000;

/** 線を1本張る。返すのは pump と後片付け */
async function link(emu) {
  const ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  let inbox = [];
  ws.onmessage = (e) => inbox.push(new Uint8Array(e.data));
  await new Promise((ok, ng) => {
    ws.onopen = ok;
    ws.onerror = () => ng(new Error(`wsslirpに繋がらない: ${url}`));
  });
  const pump = () => {
    for (;;) {
      const f = emu.net_take_frame();
      if (!f.length) break;
      ws.send(f);
    }
    for (const f of inbox) emu.net_inject_frame(f);
    inbox = [];
  };
  return { pump, close: () => ws.close(), inboxLen: () => inbox.length };
}

// ---------------- 32bit (Linux) ----------------

async function linux(slice) {
  const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinuz-lts')));
  const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));
  const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
  emu.net_attach(MAC);
  emu.set_rtc_unix(Date.now() / 1000);
  const { pump, close, inboxLen } = await link(emu);

  let serial = '';
  const t0 = performance.now();
  let virtualMs = 0;
  let realSliceMs = 0;
  let slices = 0;

  const step = async () => {
    const s0 = performance.now();
    emu.run_slice(slice);
    pump();
    const out = emu.serial_out();
    if (out.length) serial += Buffer.from(out).toString('latin1');
    emu.take_idle_skipped();
    // **忙しいスライスの実時間**だけを平均する (寝ている間は勘定に入れない)
    const dt = performance.now() - s0;
    if (dt > 0.05) {
      realSliceMs += dt;
      slices++;
    }
    virtualMs += slice / INSTR_PER_GUEST_MS;
    const realMs = performance.now() - t0;
    if (virtualMs < realMs - 100) virtualMs = realMs - 100;
    for (;;) {
      const ahead = virtualMs - (performance.now() - t0);
      if (ahead <= 8) break;
      // **寝ている間に届いたフレームで早く起きる** (WAKE=1)。
      // 寝の粒度がそのまま遅延の定数項になっているか、この有無で測る
      const nap = process.env.WAKE ? 1 : Math.min(50, ahead);
      await sleep(nap);
      const had = inboxLen();
      pump();
      if (process.env.WAKE && had) break;
    }
    await yieldLoop();
  };
  const waitFor = async (text, max) => {
    for (let i = 0; i < max; i++) {
      await step();
      if (serial.includes(text)) return true;
    }
    return false;
  };

  // 上限は**命令数で決める** — スライス数で切ると、刻みが細かいときに
  // 起動しきる前に打ち切ってしまう (100K刻みで実際に踏んだ)
  const bootCap = Math.ceil(3_000_000_000 / slice);
  if (!(await waitFor('busybox shell', bootCap))) throw new Error('シェルに届かない');
  for (let i = 0; i < 50; i++) await step();
  emu.serial_in(Buffer.from(`ping -c 5 ${TARGET}\n`, 'latin1'));
  if (!(await waitFor('packets transmitted', bootCap))) throw new Error('pingが返らない');

  close();
  const m = serial.match(/round-trip min\/avg\/max = ([\d.]+)\/([\d.]+)\/([\d.]+) ms/)
    || serial.match(/rtt min\/avg\/max\/mdev = ([\d.]+)\/([\d.]+)\/([\d.]+)/);
  const loss = serial.match(/(\d+)% packet loss/);
  return {
    min: m ? +m[1] : NaN,
    avg: m ? +m[2] : NaN,
    max: m ? +m[3] : NaN,
    loss: loss ? +loss[1] : NaN,
    sliceMs: realSliceMs / Math.max(1, slices),
  };
}

// ---------------- 16bit (FreeDOS + mTCP) ----------------

async function freedos() {
  const emu = mod.Emulator.from_disk(
    new Uint8Array(readFileSync(join(root, 'images/fd14games.img'))),
  );
  emu.net_attach(MAC);
  const { pump, close } = await link(emu);

  const screen = () => {
    const v = new Uint8Array(init.memory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
    let out = '';
    for (let r = 0; r < emu.text_rows(); r++) {
      for (let c = 0; c < emu.text_cols(); c++) {
        const ch = v[(r * emu.text_cols() + c) * 2];
        out += ch >= 32 && ch < 127 ? String.fromCharCode(ch) : ' ';
      }
      out += '\n';
    }
    return out;
  };
  const step = async (n) => {
    for (let i = 0; i < n; i++) {
      emu.run_slice(50_000);
      pump();
      await yieldLoop();
    }
  };
  const waitFor = async (text, max) => {
    for (let i = 0; i < max; i++) {
      await step(1);
      if (screen().includes(text)) return true;
    }
    return false;
  };
  const type = async (s) => {
    for (const ch of s) {
      emu.type_text(ch);
      await step(10);
    }
  };

  if (!(await waitFor('FreeDOS kernel', 6000))) throw new Error('FreeDOSが起動しない');
  emu.send_scancode(0x3f);
  emu.send_scancode(0xbf); // F5
  await waitFor('full shell command line', 6000);
  await type('\\FREEDOS\\BIN\\COMMAND.COM\n');
  await waitFor('A:\\>', 6000);
  await type('NE2000 0x60 3 0x300\n');
  await step(400);
  await type('SET MTCPCFG=A:\\MTCP.CFG\n');
  await step(100);
  await type('DHCP\n');
  if (!(await waitFor('10.0.2.15', 12000))) throw new Error('DHCPが通らない');
  await type(`PING ${TARGET}\n`);
  if (!(await waitFor(`received from ${TARGET}`, 24000))) throw new Error('pingが返らない');
  await step(3000);
  close();
  // mTCPは応答ごとに "... in 1.70 ms, ttl=64" を出す
  const rtts = [...screen().matchAll(/in ([\d.]+) ms/g)].map((m) => +m[1]);
  return {
    n: rtts.length,
    min: Math.min(...rtts),
    avg: rtts.reduce((a, b) => a + b, 0) / Math.max(1, rtts.length),
    max: Math.max(...rtts),
  };
}

// ---------------- 本測定 ----------------

console.log(`宛先: ${TARGET} (SLiRPのゲートウェイ — 外には出ない)\n`);

const d = await freedos();
console.log('16bit (FreeDOS + mTCP、スライス5万命令固定)');
console.log(`  RTT ${d.min.toFixed(2)} / ${d.avg.toFixed(2)} / ${d.max.toFixed(2)} ms (min/avg/max、${d.n}発)\n`);

console.log('32bit (Linux) — スライスの刻みを振る');
console.log('  刻み(命令)   スライスの実時間   RTT min/avg/max (ms)   欠損');
for (const slice of SLICES) {
  const r = await linux(slice);
  console.log(
    `  ${slice.toLocaleString().padStart(10)}   ${r.sliceMs.toFixed(2).padStart(8)} ms   ` +
      `${r.min.toFixed(2)}/${r.avg.toFixed(2)}/${r.max.toFixed(2)}`.padStart(20) +
      `   ${r.loss}%`,
  );
}
process.exit(0);
