// 貼り付けのIntegration — **貼った文字がそのままの並びでゲストに届くか**。
//
//   node tools/webtest/paste.mjs
//
// ## なぜ結合で見るのか
//
// 貼り付けは3つの層をまたぐ。単体ではどれも正しく見えるのに、繋ぐと壊れる:
//
//   1. 配分の判断 (web/decide.js の pasteChunk — 今いくつ渡すか)
//   2. 装置 (8042の行列 → IRQ1 → BIOSの環16枠)
//   3. ゲスト (環を読んで画面へ echo する)
//
// 実際に踏んだ壊れ方も継ぎ目にあった。Ctrl+Shift+V の修飾キーが押されたまま
// 流し込まれて制御コードに化けたり (root → ↕¶)、8042の行列が空くまで待って
// 出しては止まったり、毎刻み1文字に絞ってタイプライターになったり。
//
// ## ここで証明できること・できないこと
//
// **できる**: 62文字が順序どおり欠けずに画面へ出ること。まとめて渡せている
// こと (1回あたりの文字数で見る — 細切れに退化すると落ちる)。
//
// **できない**: 環 (16枠) を溢れさせない、という保証。測ってみると
// `type_ascii` はスキャンコードを8042の行列 (上限なし) に積み、配送はゲストが
// 読むたびに1バイトずつなので、**一度に全部渡しても環の空きは15のまま減らない**。
// login: でもシェルでも、1200文字を一度に渡しても同じだった。つまり流量制御は
// 「溢れ止め」としては今のところ働いておらず、効いているのは配送の均しだけである。
// 溢れを本当に起こす条件 (ゲストが長時間キーを読まない場面) はまだ作れていない。
//
// exit 0 = 並びどおり届き、細切れでもない。exit 1 = 欠けた・化けた・細切れ
import { readFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { pasteChunk } from '../../web/decide.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const wasm = readFileSync(join(root, 'web/pkg/rustx86_wasm_bg.wasm'));
const mod = await import(pathToFileURL(join(root, 'web/pkg/rustx86_wasm.js')).href);
const inst = await mod.default({ module_or_path: wasm });
const memory = inst.memory ?? mod.memory;

/** 画面 (テキストVRAM) を80桁の行に直す */
function screen(emu) {
  const v = new Uint8Array(memory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
  const rows = [];
  for (let r = 0; r < 25; r++) {
    let line = '';
    for (let c = 0; c < 80; c++) line += String.fromCharCode(v[(r * 80 + c) * 2] || 32);
    rows.push(line.replace(/\s+$/, ''));
  }
  return rows.join('\n');
}

/** BIOSの環の空き枠。main.js の biosKeyRoom と同じ読み方 (BDA 0x41A/0x41C) */
function biosKeyRoom(emu) {
  const b = emu.read_mem(0x41a, 4);
  const head = b[0] | (b[1] << 8);
  const tail = b[2] | (b[3] << 8);
  const span = 0x3e - 0x1e;
  const used = ((((tail - head) % span) + span) % span) / 2;
  return Math.max(0, 16 - 1 - used);
}

const CHUNK = 2_000_000;

/** 画面に needle が出るまで回す */
function runUntil(emu, needle, budget) {
  for (let n = 0; n < budget; n += CHUNK) {
    emu.run_slice(CHUNK);
    if (emu.trap_reason()) return false;
    if (screen(emu).includes(needle)) return true;
  }
  return false;
}

const disk = new Uint8Array(readFileSync(join(root, 'images/fd2880.img')));
const emu = mod.Emulator.from_disk(disk);

if (!runUntil(emu, 'login:', 3_000_000_000)) {
  console.log('ELKSがlogin:まで来なかった (貼り付け以前の問題)');
  process.exit(1);
}

// 環 (16枠) より十分長く、欠けたら一目で分かる並び。改行は入れない —
// ゲストに実行させたいわけではなく、**届いた文字そのもの**を見たいので
const TEXT = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz';

let queue = TEXT;
let ticks = 0;
let handed = 0; // 実際に渡した回数 (まとめて渡せているかの目安)
let minRoom = 16; // 貼っている間、環がどこまで詰まったか
const MAX_TICKS = 4000;
while (queue && ticks < MAX_TICKS) {
  ticks++;
  // ブラウザの16ms刻みに相当する間、機械を進める
  emu.run_slice(CHUNK);
  if (emu.trap_reason()) {
    console.log(`[TRAP] ${emu.trap_reason()}`);
    process.exit(1);
  }
  const inflight = Math.ceil(emu.key_backlog() / 2);
  const room = biosKeyRoom(emu);
  minRoom = Math.min(minRoom, room);
  const n = pasteChunk(room, inflight, queue.length);
  if (!n) continue;
  emu.type_text(queue.slice(0, n));
  queue = queue.slice(n);
  handed++;
}

// 渡し終えてから、ゲストが読み切って画面に出るまで待つ
runUntil(emu, TEXT, 200_000_000);

// **折り返しを跨いで読む。** プロンプト (`[0.57 secs] login: `) と合わせると
// 80桁を超えるので、最後の数文字は次の行に回る。1行だけ見ると
// 「届いているのに欠けた」と誤って言うことになる (実際に一度そう出た)
const rows = screen(emu).split('\n');
const at = rows.findIndex((l) => l.includes(TEXT.slice(0, 8)));
const got = (at < 0 ? '' : rows[at] + (rows[at + 1] ?? '')).replace(/^.*login: /, '');
const ok = got === TEXT;

console.log(`handed=${handed}回 (${TEXT.length}文字)  ticks=${ticks}  環の空き最小=${minRoom}`);
console.log(`want: ${TEXT}`);
console.log(`got : ${got}`);
console.log(ok ? 'paste_intact=true' : 'paste_intact=false — 欠けている');

// **まとめて渡せていることも見る。** 1文字ずつに退化するとタイプライターに
// なるので、回数が文字数に近ければ配分が壊れている
const burst = TEXT.length / Math.max(1, handed);
console.log(`1回あたり ${burst.toFixed(1)} 文字`);
if (ok && burst < 4) {
  console.log('paste_burst=false — 細切れすぎる (環の空きを使い切れていない)');
  process.exit(1);
}
process.exit(ok ? 0 : 1);
