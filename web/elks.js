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

/** スクロールで画面外へ押し出された行の控え (最大 LOG_LINES 行のリングバッファ) */
const LOG_LINES = 1000;
const log = [];

/**
 * 前回のVRAMの写し。スクロールの検出に使う。
 *
 * **文字列ではなくバイト列で持つ**のが要点である。25行を文字列に組み立てるのは
 * 高くつくのでサンプル間隔を詰められず、間隔が空くと一度に何十行も流れて
 * 差分を追えなくなる (実際にそれでログがほぼ空になった)。
 * バイト比較なら安いので、細かく見張れる。
 */
let prevVram = null;
/** 直近のVRAMから作った25行 (ログ表示用) */
let prevRows = null;

/**
 * 画面がスクロールしたかを判定し、押し出された行を控える。
 *
 * VRAMには「今見えている25行」しか無く、流れ去った行はどこにも残らない。
 * カーネルがスクロールすると各行が1つ上へ動くので、**前回の (shift+1) 行目以降が
 * 今回の1行目以降と一致していれば shift 行スクロールした**と分かる。
 */
function captureScroll(vram) {
  const rowBytes = cols * 2;
  const total = rowBytes * rows;
  if (!prevVram) {
    prevVram = new Uint8Array(total);
    prevVram.set(vram.subarray(0, total));
    prevRows = rowsFrom(vram);
    return;
  }

  const shift = detectScroll(prevVram, vram, rowBytes);
  if (shift > 0) {
    for (let i = 0; i < shift && i < prevRows.length; i++) log.push(prevRows[i]);
    while (log.length > LOG_LINES) log.shift();
  }
  prevVram.set(vram.subarray(0, total));
  prevRows = rowsFrom(vram);
}

/**
 * 画面が何行スクロールしたかを返す (0 ならスクロールしていない)。
 *
 * **画面全体の一致は見ない。上から数行だけを見る。**
 * 画面はカーソル行が常に書き換わっているので、全体一致を条件にすると
 * ほとんどの瞬間で不成立になり、比較元が固まってしまう
 * (実際にそれでログが数行しか取れなかった)。
 *
 * スクロールなら上の行がそっくり1つ上へ動くので、**先頭3行の一致**で足りる。
 * 空行どうしが偶然そろうのを避けるため、中身のある行であることも要求する。
 */
function detectScroll(prev, now, rowBytes) {
  const NEED = 3;
  for (let shift = 1; shift <= rows - NEED; shift++) {
    let ok = true;
    let content = false;
    for (let r = 0; r < NEED; r++) {
      const a = (r + shift) * rowBytes;
      const b = r * rowBytes;
      for (let i = 0; i < rowBytes; i += 2) {
        if (prev[a + i] !== now[b + i]) {
          ok = false;
          break;
        }
        if (now[b + i] > 0x20) content = true;
      }
      if (!ok) break;
    }
    if (ok && content) return shift;
  }
  return 0;
}

/** バイト列から25行の文字列を作る。**スクロール検出時と表示時にだけ**呼ぶ */
function rowsFrom(vram) {
  const out = [];
  for (let row = 0; row < rows; row++) {
    let line = '';
    for (let col = 0; col < cols; col++) {
      const ch = vram[(row * cols + col) * 2];
      line += ch >= 0x20 && ch < 0x7f ? String.fromCharCode(ch) : ' ';
    }
    out.push(line.replace(/\s+$/, ''));
  }
  return out;
}

/** 控えた行 + 今の画面 を続きとして返す */
function fullLog() {
  const now = emu ? rowsFrom(vramView()) : [];
  return [...log, ...now].join('\n').replace(/\s+$/, '');
}

/** wasmメモリ上のテキストVRAMをそのまま見る (コピーしない) */
function vramView() {
  return new Uint8Array(wasmMemory.buffer, emu.text_vram_ptr(), emu.text_vram_len());
}

/**
 * wasmのメモリを直接読んで画面を描く (コピーを作らない)。
 *
 * こちらは2000セルを塗る**高い処理**なので、1フレームに1回しか呼ばない。
 * スクロールの追跡は [`readRows`] 側で細かく行う
 */
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
  if (logView && !logView.hidden) logView.textContent = fullLog();
}

let wasmMemory = null;
const logView = $('log');

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

/**
 * 1フレーム分を細切れに進める。
 *
 * まとめて300万命令進めてから画面を見ると、その間に何十行もスクロールしていて
 * **流れ去った行を追えない** (実際にそれでログが空になった)。
 * 読み取りは安いので細かく、描画は高いので最後に1回だけ行う。
 */
const CHUNK = 6_000;

function frame() {
  if (!running) return;
  let dirty = false;
  for (let done = 0; done < INSTRUCTIONS_PER_FRAME; done += CHUNK) {
    emu.run_slice(CHUNK);
    if (emu.take_vram_dirty()) {
      dirty = true;
      captureScroll(vramView());
    }
  }
  if (dirty) draw();
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
  log.length = 0;
  prevRows = null;
  prevVram = null;
  window.__log = log;
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

$('showlog').addEventListener('click', () => {
  logView.hidden = !logView.hidden;
  $('showlog').textContent = logView.hidden ? `ログを見る (${log.length}行)` : 'ログを隠す';
  if (!logView.hidden) {
    logView.textContent = fullLog();
    logView.scrollTop = logView.scrollHeight;
  }
});

$('savelog').addEventListener('click', () => {
  const blob = new Blob([fullLog()], { type: 'text/plain' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'elks-boot.log';
  a.click();
  URL.revokeObjectURL(a.href);
});

$('file').addEventListener('change', async e => {
  const f = e.target.files?.[0];
  if (!f) return;
  setStatus(`${f.name} を読み込み中…`);
  bootWith(new Uint8Array(await f.arrayBuffer()));
});

/** 同じ場所に置かれたイメージから起動する */
async function bootFromUrl() {
  setStatus('fd1440.img を取得中…');
  try {
    const r = await fetch('./fd1440.img');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    bootWith(new Uint8Array(await r.arrayBuffer()));
  } catch (e) {
    setStatus(`fd1440.img が見つからない (${e.message})。ファイルを選んでください`, true);
  }
}

$('boot').addEventListener('click', bootFromUrl);

// 読み込みに失敗すると「読み込み中…」のまま黙って止まる。
// 何が起きたか分からないのが一番困るので、必ず画面に出す
window.addEventListener('error', e => setStatus(`エラー: ${e.message}`, true));
window.addEventListener('unhandledrejection', e => setStatus(`エラー: ${e.reason}`, true));

try {
  const wasm = await init();
  wasmMemory = wasm.memory;
  $('boot').disabled = false;
  setStatus('ディスクイメージを選ぶと起動します');
  // 同じ場所にイメージがあれば自動で起動する。
  // ボタンを疑似クリックせず関数を直接呼ぶ — 起動経路をイベントに依存させない
  const head = await fetch('./fd1440.img', { method: 'HEAD' }).catch(() => null);
  if (head?.ok) await bootFromUrl();
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
