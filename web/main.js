// ページの入口。
//
// **ここが繋ぎ役**である。やることは3つしかない:
//   1. ディスクイメージを手に入れる (同じ場所から取る / ドロップしてもらう)
//   2. [`Machine`](./machine.js) を作って回す
//   3. 機械の画面を [`Terminal`](./terminal.js) へ、端末のキーを機械へ
//
// 機械は画面を知らず、端末は機械を知らない。互いを知っているのはここだけなので、
// 別のOSを載せても、端末を差し替えても、直すのはこのファイルで済む。

import { loadWasm, Machine } from './machine.js';
import { Terminal } from './terminal.js';

const $ = id => document.getElementById(id);
const term = new Terminal($('screen'), { scrollback: 1000 });

let machine = null;
/** 最後に起動したイメージ。再起動に使う */
let lastImage = null;

function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

/** ツールバーの表示を実際の状態に合わせる */
function syncControls() {
  const on = !!machine;
  $('pause').disabled = !on;
  $('pause').textContent = machine?.paused ? '再開' : '一時停止';
  $('boot').disabled = !lastImage;
}

function boot(image, label) {
  machine?.stop();
  try {
    machine = new Machine(image);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  lastImage = image;
  term.reset();
  machine.onFrame = (cells, row, col, redraw) => {
    term.sample(cells, row, col);
    if (redraw) term.draw();
  };
  // 物理キーはそのまま、貼り付けはASCIIとして送る
  term.onKey = (code, down) => machine.key(code, down);
  term.onPaste = text => machine.paste(text);

  window.__machine = machine; // 動作確認用
  window.__term = term;

  machine.start();
  setStatus(`${label} を起動中… 画面をクリックするとキー入力できます`);
  $('screen').focus();
  syncControls();
}

/** 1秒に2回、速度と履歴の深さを出す。教材として「今どれくらい出ているか」を見せる */
setInterval(() => {
  if (!machine) return;
  const parts = [];
  parts.push(machine.paused ? '停止中' : `${machine.mips.toFixed(0)} MIPS`);
  if (term.scrollback.length) parts.push(`履歴 ${term.scrollback.length}行`);
  if (term.offset) parts.push(`▲${term.offset}行前`);
  $('gauge').textContent = parts.join('   ');
}, 500);

// --- 操作 ---

$('pause').addEventListener('click', () => {
  if (!machine) return;
  if (machine.paused) machine.start();
  else machine.stop();
  syncControls();
  $('screen').focus();
});

$('boot').addEventListener('click', () => {
  if (lastImage) boot(lastImage, 'ディスク');
});

$('save').addEventListener('click', () => {
  const blob = new Blob([term.allLines().join('\n')], { type: 'text/plain' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'elks.log';
  a.click();
  URL.revokeObjectURL(a.href);
});

// --- ディスクイメージの受け取り ---

const consoleBox = $('console');
for (const ev of ['dragenter', 'dragover']) {
  consoleBox.addEventListener(ev, e => {
    e.preventDefault();
    consoleBox.classList.add('drop');
  });
}
for (const ev of ['dragleave', 'drop']) {
  consoleBox.addEventListener(ev, () => consoleBox.classList.remove('drop'));
}
consoleBox.addEventListener('drop', async e => {
  e.preventDefault();
  const f = e.dataTransfer?.files?.[0];
  if (!f) return;
  setStatus(`${f.name} を読み込み中…`);
  boot(new Uint8Array(await f.arrayBuffer()), f.name);
});

async function bootFromUrl() {
  setStatus('fd1440.img を取得中…');
  try {
    const r = await fetch('./fd1440.img');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    boot(new Uint8Array(await r.arrayBuffer()), 'fd1440.img');
  } catch (e) {
    setStatus(
      `fd1440.img が見つからない (${e.message})。イメージをここにドロップしてください`,
      true,
    );
  }
}

// 読み込みに失敗すると「読み込み中…」のまま黙って止まる。
// 何が起きたか分からないのが一番困るので、必ず画面に出す
window.addEventListener('error', e => setStatus(`エラー: ${e.message}`, true));
window.addEventListener('unhandledrejection', e => setStatus(`エラー: ${e.reason}`, true));

try {
  await loadWasm();
  setStatus('ディスクイメージをここにドロップすると起動します');
  const head = await fetch('./fd1440.img', { method: 'HEAD' }).catch(() => null);
  if (head?.ok) await bootFromUrl();
  syncControls();
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
