// 16bitから本物のpingを打つE2E (ADR-0017の合格判定)。
//
// FreeDOSを起動し、NE2000パケットドライバ + mTCP で DHCP → PING を打つ。
// フレームは wsslirp (ユーザーモードNAT) へWebSocketで運ぶ。
// 網元が要るのでopt-in — 起動中の wsslirpd を指して:
//
//   (wsslirpリポジトリで) go run ./cmd/wsslirpd -listen 127.0.0.1:8098 -token test
//   RUSTX86_NET_E2E_URL='ws://127.0.0.1:8098/net?token=test' node tools/webtest/netping.mjs
//
// exit 0 = DHCPでアドレスを取り、1.1.1.1 からecho応答が返った。
import { readFileSync } from 'node:fs';
import { setImmediate as yieldLoop } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const url = process.env.RUSTX86_NET_E2E_URL;
if (!url) {
  console.log('RUSTX86_NET_E2E_URL を設定すると実行する (wsslirpdが要る)');
  process.exit(0);
}

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
const init = await mod.default({
  module_or_path: readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm')),
});
const emu = mod.Emulator.from_disk(
  new Uint8Array(readFileSync(join(root, 'images/fd14games.img'))),
);
emu.net_attach(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));

// wsslirpへの結線。届いたフレームは inbox に溜め、スライス境界で注入する
// (web/netlink.js と同じ設計。Node 21+ はWebSocketを標準で持っている)
const ws = new WebSocket(url);
ws.binaryType = 'arraybuffer';
const inbox = [];
ws.onmessage = e => inbox.push(new Uint8Array(e.data));
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

function screen() {
  const v = new Uint8Array(init.memory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
  const rows = [];
  for (let r = 0; r < emu.text_rows(); r++) {
    let line = '';
    for (let c = 0; c < emu.text_cols(); c++) {
      const ch = v[(r * emu.text_cols() + c) * 2];
      line += ch >= 32 && ch < 127 ? String.fromCharCode(ch) : ' ';
    }
    rows.push(line.trimEnd());
  }
  return rows.join('\n');
}

// **同期ループはWebSocketの受信を殺す** — Nodeのonmessageはイベントループが
// 回って初めて呼ばれる。スライスごとに一度ループへ制御を返すのが肝
async function step(slices) {
  for (let i = 0; i < slices; i++) {
    emu.run_slice(50_000);
    pump();
    await yieldLoop();
  }
}

async function waitFor(text, maxSlices, what) {
  for (let i = 0; i < maxSlices; i++) {
    await step(1);
    if (screen().includes(text)) return;
  }
  console.log(`タイムアウト: ${what ?? text}\n--- 画面 ---\n${screen()}`);
  process.exit(1);
}

async function typeSlow(s) {
  for (const ch of s) {
    emu.type_text(ch);
    await step(10);
  }
}

// FreeDOSをプロンプトまで (machines.js の起動スクリプトと同じ手順)
await waitFor('FreeDOS kernel', 6000);
emu.send_scancode(0x3f);
emu.send_scancode(0xbf); // F5
await waitFor('full shell command line', 6000);
await typeSlow('\\FREEDOS\\BIN\\COMMAND.COM\n');
await waitFor('A:\\>', 6000);

// パケットドライバ常駐 → mTCPの設定 → DHCP → ping
await typeSlow('NE2000 0x60 3 0x300\n');
await step(400);
await typeSlow('SET MTCPCFG=A:\\MTCP.CFG\n');
await step(100);
await typeSlow('DHCP\n');
await waitFor('10.0.2.15', 12000, 'DHCPのアドレス取得');
console.log('dhcp: 10.0.2.15 を取得');
await typeSlow('PING 1.1.1.1\n');
// mTCPのpingは応答ごとに "received from 1.1.1.1" を1行出す
await waitFor('received from 1.1.1.1', 24000, 'pingの応答');
await step(2000); // 残りの応答も流す
console.log('--- 画面 (下12行) ---');
console.log(
  screen()
    .split('\n')
    .filter(l => l.trim())
    .slice(-12)
    .join('\n'),
);
console.log('\nping: 1.1.1.1 から応答が返った — 16bitが本物のインターネットに届いた');
ws.close();
process.exit(0);
