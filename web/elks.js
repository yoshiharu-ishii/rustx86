// ブラウザでELKSを動かす。
//
// やっていることは3つだけ:
//   1. 毎フレーム一定命令数だけCPUを進める
//   2. テキストVRAM (0xB8000) をcanvasに描く
//   3. キー入力を8042へスキャンコードとして流す
//
// エミュレータ側に手を入れていないのがポイントである。UARTもVRAMも8042も
// 「バイト列の口」を持っているので、CLIと同じ口にブラウザを繋いだだけになる。

import init, { Emulator } from './pkg/rustx86_wasm.js';

const $ = id => document.getElementById(id);
const canvas = $('screen');
const ctx = canvas.getContext('2d', { alpha: false });

/** 1フレームで進める命令数。実機の8086より遥かに速いが、起動を待たずに済む */
const INSTRUCTIONS_PER_FRAME = 3_000_000;

/** CGA 16色。上位4bitが背景、下位4bitが前景 (bit7は本来点滅) */
const PALETTE = [
  '#000000', '#0000aa', '#00aa00', '#00aaaa', '#aa0000', '#aa00aa', '#aa5500', '#aaaaaa',
  '#555555', '#5555ff', '#55ff55', '#55ffff', '#ff5555', '#ff55ff', '#ffff55', '#ffffff',
];

const CELL_W = 9;
const CELL_H = 16;

let emu = null;
let running = false;
let cols = 80;
let rows = 25;

function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

/** wasmのメモリを直接読んで画面を描く (コピーを作らない) */
function draw() {
  const mem = new Uint8Array(wasmMemory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
  ctx.textBaseline = 'top';
  ctx.font = `${CELL_H}px ui-monospace, Menlo, monospace`;
  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      const i = (row * cols + col) * 2;
      const ch = mem[i];
      const attr = mem[i + 1];
      const x = col * CELL_W;
      const y = row * CELL_H;
      ctx.fillStyle = PALETTE[(attr >> 4) & 7];
      ctx.fillRect(x, y, CELL_W, CELL_H);
      if (ch >= 0x20 && ch < 0x7f) {
        ctx.fillStyle = PALETTE[attr & 0x0f];
        ctx.fillText(String.fromCharCode(ch), x, y);
      }
    }
  }
}

let wasmMemory = null;

/**
 * 次のフレームを予約する。
 *
 * requestAnimationFrame は**タブが非表示だと発火しない**。それだけに頼ると
 * 裏に回した瞬間にOSが止まり、戻ってきたときに時計が飛ぶ。
 * 非表示のときはタイマで回して、動き続けるようにする。
 */
function scheduleFrame() {
  if (document.hidden) setTimeout(frame, 16);
  else requestAnimationFrame(frame);
}

function frame() {
  if (!running) return;
  emu.run_slice(INSTRUCTIONS_PER_FRAME);
  // 書き換わったときだけ描く。毎フレーム2000セルを塗り直すのは無駄が大きい
  if (emu.take_vram_dirty()) draw();
  scheduleFrame();
}

function bootWith(bytes) {
  try {
    emu = Emulator.from_disk(bytes);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  cols = emu.text_cols();
  rows = emu.text_rows();
  canvas.width = cols * CELL_W;
  canvas.height = rows * CELL_H;
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, canvas.width, canvas.height);

  running = true;
  // 動作確認用にコンソールから触れるようにしておく (window.__emu)
  window.__emu = emu;
  $('login').disabled = false;
  $('ls').disabled = false;
  setStatus('起動中… 画面をクリックするとキー入力できます');
  canvas.focus();
  scheduleFrame();
}

// --- キー入力 ---
//
// event.key を文字に直して渡している。8042側で「押す/離す」の
// スキャンコード対に変換されるので、こちらは文字を渡すだけでよい

canvas.addEventListener('keydown', e => {
  if (!emu) return;
  let text = null;
  if (e.key.length === 1) text = e.key;
  else if (e.key === 'Enter') text = '\n';
  else if (e.key === 'Backspace') text = '\x08';
  else if (e.key === 'Tab') text = '\t';
  if (text !== null) {
    emu.type_text(text);
    e.preventDefault();
  }
});

$('login').addEventListener('click', () => {
  emu?.type_text('root\n');
  canvas.focus();
});
$('ls').addEventListener('click', () => {
  emu?.type_text('ls\n');
  canvas.focus();
});

$('file').addEventListener('change', async e => {
  const f = e.target.files?.[0];
  if (!f) return;
  setStatus(`${f.name} を読み込み中…`);
  bootWith(new Uint8Array(await f.arrayBuffer()));
});

$('boot').addEventListener('click', async () => {
  setStatus('fd1440.img を取得中…');
  try {
    const r = await fetch('./fd1440.img');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    bootWith(new Uint8Array(await r.arrayBuffer()));
  } catch (e) {
    setStatus(`同じ場所に fd1440.img が見つからない (${e.message})。ファイルを選んでください`, true);
  }
});

const wasm = await init();
wasmMemory = wasm.memory;

// 同じ場所にイメージがあれば自動で起動する
try {
  const r = await fetch('./fd1440.img', { method: 'HEAD' });
  if (r.ok) {
    $('boot').disabled = false;
    $('boot').click();
  } else {
    throw new Error();
  }
} catch {
  $('boot').disabled = false;
  setStatus('ディスクイメージを選ぶと起動します');
}
