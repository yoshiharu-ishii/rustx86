// 32bitのLinuxから本物のインターネットへ届くE2E (ADR-0017 5cの合格判定)。
//
// Linux (bzImage) をwasmで起動し、RTL8029 (PCI) + udhcpc で DHCP →
// busyboxのwgetで http://example.com/ からHTMLを引く。フレームは
// wsslirp (ユーザーモードNAT) へWebSocketで運ぶ。16bit版 (netping.mjs) の
// Linux版で、同じ道を通るのはNICの皮とゲストのOSだけが違う。
//
// SLiRP backendが要るのでopt-in — 起動中の wsslirpd を指して:
//
//   RUSTX86_NET_E2E_URL='ws://127.0.0.1:8087/net?token=…' node tools/webtest/netlinux.mjs
//
// exit 0 = DHCPでアドレスを取り、example.com のHTMLが画面に出た。
import { readFileSync } from 'node:fs';
import { setImmediate as yieldLoop, setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const url = process.env.RUSTX86_NET_E2E_URL;
if (!url) {
  console.log('RUSTX86_NET_E2E_URL を設定すると実行する (wsslirpdが要る)');
  process.exit(0);
}

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
await mod.default({
  module_or_path: readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm')),
});
const kernel = new Uint8Array(readFileSync(join(root, 'images/vmlinuz-lts')));
const initrd = new Uint8Array(readFileSync(join(root, 'images/initramfs-mini')));
const emu = mod.Emulator.from_bzimage(kernel, initrd, 'console=ttyS0', 128);
// NICを挿すのは電源の瞬間 (ブラウザのワーカーと同じ手順)
emu.net_attach(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
// RTCを実時刻に (ブラウザのワーカーと同じ)。TLSの証明書検証は時計が前提
emu.set_rtc_unix(Date.now() / 1000);

const ws = new WebSocket(url);
ws.binaryType = 'arraybuffer';
const inbox = [];
ws.onmessage = (e) => inbox.push(new Uint8Array(e.data));
await new Promise((ok, ng) => {
  ws.onopen = ok;
  ws.onerror = () => ng(new Error(`wsslirpに繋がらない: ${url}`));
});

function pump() {
  for (;;) {
    const f = emu.net_take_frame();
    if (!f.length) break;
    ws.send(f);
  }
  for (const f of inbox) emu.net_inject_frame(f);
  inbox.length = 0;
}

let serial = '';
// --- ゲストの時計を実時間に繋ぎ止める (web/linux-worker.js と同じ轡) ---
//
// **仮想時間が実時間を追い越すと、ゲストの「1秒に1回」が実時間の洪水になる。**
// pingの再送が毎秒何百発も本物のインターネットへ飛んだ (実際に飛ばした)。
// 起動中は忙しくて先行しないので速さは落ちない。先行はHLTの早送りで起き、
// その分だけ実時間で待って返す
const INSTR_PER_GUEST_MS = (1_193_182 * 64) / 1000;
const clockT0 = performance.now();
let virtualMs = 0;

// **同期ループはWebSocketの受信を殺す** — スライスごとに一度制御を返す
async function step(slices, size = 10_000_000) {
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
    virtualMs += size / INSTR_PER_GUEST_MS + emu.take_idle_skipped() / INSTR_PER_GUEST_MS;
    const realMs = performance.now() - clockT0;
    // **借りの上限。** 起動中は実時間が先行する (wasmは想定76 MIPSより遅い)。
    // その貸しを後のアイドルでまとめて返させると、返している間の
    // 「ゲストの1秒」が実時間の数十msになって結局洪水になる (実際になった)
    if (virtualMs < realMs - 100) virtualMs = realMs - 100;
    // **返しきるまで寝る。** 1スライスは仮想131ms (10M命令) 進むので、
    // 50msで打ち切ると1スライスごとに81ms先行が積もる (実際に積もって
    // 3発のpingが0.3秒で飛んだ)。50ms刻みなのはWSの受信を捌くため
    for (;;) {
      const ahead = virtualMs - (performance.now() - clockT0);
      if (ahead <= 8) break;
      await sleep(Math.min(50, ahead));
      pump();
    }
    await yieldLoop();
  }
}

async function waitFor(text, maxSlices, what) {
  for (let i = 0; i < maxSlices; i++) {
    await step(1);
    if (serial.includes(text)) {
      console.log(`✓ ${what}`);
      return;
    }
  }
  console.log(`✗ ${what} — 待ちきれず`);
  console.log(serial.split('\n').slice(-12).join('\n'));
  process.exit(1);
}

const type = (s) => emu.serial_in(Buffer.from(s, 'latin1'));

await waitFor('busybox shell', 400, 'シェル到達');
// initの udhcpc -b が裏で回っている。リースが付くのを ifconfig で確かめる
await step(50);
type('ifconfig eth0 | grep "inet addr"\n');
await waitFor('inet addr:10.0.2.15', 200, 'DHCPでアドレス取得 (10.0.2.15)');
// ping。**実時間の定規で数える** — 時計の轡が外れると1秒1発が洪水になる
// (実際に洪水にした)。3発が実時間2秒未満で終わったら轡が外れている
{
  const t0 = performance.now();
  type('ping -c 3 1.1.1.1\n');
  await waitFor('3 packets transmitted', 600, 'ping 1.1.1.1 (3発)');
  const secs = (performance.now() - t0) / 1000;
  if (secs < 1.8) {
    console.log(`✗ pingが速すぎる (3発 ${secs.toFixed(1)}s) — ゲストの時計が実時間を追い越している`);
    process.exit(1);
  }
  console.log(`✓ pingの間隔は実時間 (3発 ${secs.toFixed(1)}s)`);
}
// 本物のインターネットからHTMLを引く (DNS → TCP → HTTP、全部wsslirp経由)
type('wget -q -O - http://example.com/ | grep -o "<title>.*</title>"\n');
await waitFor('<title>Example Domain</title>', 600, 'example.com からHTMLが引けた');
// https — TLSの壁3枚 (MMX・RTC実時刻・ssl_client+CA同梱) が全部越えられて
// いる証明。実物のPNGを引き、シグネチャ (89504e47) をゲスト内で確かめる。
// センチネルは分割して書く — コマンドのエコー自体に誤反応しない (1敗)
type("wget -q -O /tmp/i.png https://pocraft.net/wp-content/uploads/2026/08/wsslirp-eyecatch-s1.png && head -c4 /tmp/i.png | od -An -tx1 | tr -d ' \\n' && echo :GOT''PNG\n");
await waitFor('89504e47:GOTPNG', 1200, 'httpsで本物のPNGが引けた (TLS成立)');
ws.close();
process.exit(0);
