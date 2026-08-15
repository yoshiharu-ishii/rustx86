// ネットワークの**実効速度の定規**。1本のHTTP転送を通し、
// 実時間・ゲストの時間の使われ方・フレームの出入りを1枚の表にする。
//
//   RUSTX86_NET_E2E_URL='ws://127.0.0.1:8087/net?token=dev' \
//   BENCH_URL='http://cachefly.cachefly.net/1mb.test' \
//   node tools/webtest/netbench.mjs
//
// ## 読み方 (この4行で犯人が分かる)
//
//   実効速度   … 実時間あたりのバイト。ホストの `curl` と比べるのが出発点
//   空回り     … ゲストの時間のうちHLT (待ち) の割合。高ければ**遅延**の問題、
//                 低ければ**単価** (CPU) の問題
//   実行命令   … 1バイト運ぶのに何命令使ったか。再送や割り込みの空騒ぎで膨らむ
//   落とした   … 受信フレームの取りこぼし。TCPには一番効く毒
//
// ## 宛先の選び方 (**外の遅さを測らないこと**)
//
// 遠い配信元を選ぶと、測っているのはインターネットであってエミュレータでは
// なくなる (実話: tele2.net は1MBにホストのcurl自身が6.3秒かかり、ゲストの
// 6.9秒はほぼ素通しだった)。**必ず先に `curl -w '%{time_total}'` で
// ホストの素の速さを測り、その何倍かで語る**。
//
// 閉じた世界で測るなら tools/webtest/net-e2e.sh と同じ組み方 —
// wsslirpd を `-allow-private` で立て、ホストのHTTPサーバを指す。
//
// ## 診断つまみ
//
//   NICDBG=1     … 100スライスごとにNICの中 (線の待ち枚数・ISR・CURR/BNRY) を出す
//   SAMPLE_EIP=1 … 待ちきれなかったとき、止まっていた eip の頻度を出す
//   SLICE=…      … 1スライスの命令数 (既定30万 ≒ ブラウザのワーカーの刻み)
import { readFileSync } from 'node:fs';
import { setImmediate as yieldLoop, setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const url = process.env.RUSTX86_NET_E2E_URL;
const benchUrl = process.env.BENCH_URL || 'http://cachefly.cachefly.net/1mb.test';
if (!url) {
  console.log('RUSTX86_NET_E2E_URL を設定すると実行する (wsslirpdが要る)');
  process.exit(0);
}
// 既定はブラウザのワーカーが忙しいときに落ち着く刻み (~8ms)。
// ここを大きくすると、フレームの出入りがスライス境界まで待たされる
const SLICE = Number(process.env.SLICE || 300_000);

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const pkg = process.env.PKG || join(root, 'web/pkg');
const mod = await import(pathToFileURL(join(pkg, 'rustx86_wasm.js')).href);
await mod.default({ module_or_path: readFileSync(join(pkg, 'rustx86_wasm_bg.wasm')) });
mod.install_panic_hook?.(); // 止まった理由を見えるようにする
const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinuz-lts')));
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));
const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
emu.net_attach(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
emu.set_rtc_unix(Date.now() / 1000);

const ws = new WebSocket(url);
ws.binaryType = 'arraybuffer';
let inbox = [];
let rxBytes = 0;
ws.onmessage = (e) => {
  rxBytes += e.data.byteLength;
  inbox.push(new Uint8Array(e.data));
};
await new Promise((ok, ng) => {
  ws.onopen = ok;
  ws.onerror = () => ng(new Error(`wsslirpに繋がらない: ${url}`));
});

const stat = { injected: 0, dropped: 0, tx: 0, pumps: 0, idle: 0 };
function pump() {
  stat.pumps++;
  for (;;) {
    const f = emu.net_take_frame();
    if (!f.length) break;
    stat.tx++;
    ws.send(f);
  }
  // 束で渡してよい — 入り切らない分はカードの前の「線」で待つ (core側)
  for (const f of inbox) {
    if (emu.net_inject_frame(f)) stat.injected++;
    else stat.dropped++;
  }
  inbox = [];
}

let serial = '';
// ゲストの時計を実時間に繋ぎ止める (web/linux-worker.js と同じ轡)
const INSTR_PER_GUEST_MS = (1_193_182 * 64) / 1000;
const clockT0 = performance.now();
let virtualMs = 0;

// **同期ループはWebSocketの受信を殺す** — スライスごとに一度制御を返す
async function step(slices, size = SLICE) {
  for (let i = 0; i < slices; i++) {
    emu.run_slice(size);
    pump();
    const out = emu.serial_out();
    if (out.length) serial += Buffer.from(out).toString('latin1');
    const tr = emu.trap_reason();
    if (tr) {
      console.log(`[TRAP] ${tr}`);
      process.exit(1);
    }
    stat.idle += emu.take_idle_skipped();
    virtualMs += size / INSTR_PER_GUEST_MS;
    const realMs = performance.now() - clockT0;
    if (virtualMs < realMs - 100) virtualMs = realMs - 100;
    for (;;) {
      const ahead = virtualMs - (performance.now() - clockT0);
      if (ahead <= 8) break;
      await sleep(Math.min(50, ahead));
      pump();
    }
    await yieldLoop();
  }
}

const eipHist = new Map();
async function waitFor(text, maxSlices, what, size = SLICE) {
  for (let i = 0; i < maxSlices; i++) {
    await step(1, size);
    if (process.env.NICDBG && i % 100 === 0) {
      console.log(`  [${i}] ${emu.net_debug()} inbox=${inbox.length} inj=${stat.injected} tx=${stat.tx}`);
    }
    if (process.env.SAMPLE_EIP) {
      const ip = JSON.parse(emu.cpu_json()).eip;
      eipHist.set(ip, (eipHist.get(ip) || 0) + 1);
    }
    if (serial.includes(text)) {
      console.log(`✓ ${what}`);
      return;
    }
  }
  console.log(`✗ ${what} — 待ちきれず`);
  console.log(serial.split('\n').slice(-15).join('\n'));
  console.log(`NIC: ${emu.net_debug()} / 注入 ${stat.injected} 落とした ${stat.dropped} 送信 ${stat.tx}`);
  if (process.env.SAMPLE_EIP) {
    console.log('--- 止まっていた場所 (eipの頻度) ---');
    for (const [ip, n] of [...eipHist].sort((a, b) => b[1] - a[1]).slice(0, 10)) {
      console.log(`${String(n).padStart(5)}  ${(ip >>> 0).toString(16)}`);
    }
  }
  process.exit(1);
}

const type = (s) => emu.serial_in(Buffer.from(s, 'latin1'));

// 起動は大きなスライスで一気に (測るのは転送だけ)
await waitFor('busybox shell', 400, 'シェル到達', 10_000_000);
await step(50, 10_000_000);
type('ifconfig eth0 | grep "inet addr"\n');
await waitFor('inet addr:10.0.2.15', 200, 'DHCPでアドレス取得');

// --- ここから計測 ---
const before = { tsc: emu.tsc(), rx: rxBytes, t: performance.now(), ...stat };
serial = '';
// センチネルは分割して打つ — コマンドのエコー自体に誤反応しない (netlinux.mjsの1敗)
type(`wget -q -O /dev/null ${benchUrl}; echo BENCH''_DONE=$?\n`);
await waitFor('BENCH_DONE=', 4000, `転送完了 (${benchUrl})`);

const secs = (performance.now() - before.t) / 1000;
const tsc = emu.tsc() - before.tsc;
const idle = stat.idle - before.idle;
const bytes = rxBytes - before.rx;
const dropped = stat.dropped - before.dropped;
const injected = stat.injected - before.injected;
console.log(`
版          : ${readFileSync(join(pkg, 'BUILDINFO'), 'utf8').trim()}
スライス    : ${SLICE.toLocaleString()} 命令
実時間      : ${secs.toFixed(2)} s
受信        : ${(bytes / 1e6).toFixed(2)} MB (WS上)
実効速度    : ${(bytes / 1e6 / secs).toFixed(2)} MB/s = ${((bytes * 8) / 1e6 / secs).toFixed(1)} Mbps
空回り      : ${((idle * 100) / tsc).toFixed(0)}% (ゲストの時間 ${(tsc / 1e6).toFixed(0)}M のうち ${(idle / 1e6).toFixed(0)}M がHLT)
実行命令    : ${((tsc - idle) / 1e6).toFixed(0)} M = ${((tsc - idle) / bytes).toFixed(1)} 命令/バイト
フレーム    : 注入 ${injected} / 落とした ${dropped} (${((dropped * 100) / (injected + dropped || 1)).toFixed(1)}%) / 送信 ${stat.tx - before.tx}
`);
ws.close();
process.exit(0);
