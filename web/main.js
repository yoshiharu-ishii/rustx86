// ページの入口。
//
// **ここが繋ぎ役**である。やることは3つしかない:
//   1. ディスクイメージを手に入れる (同じ場所から取る / 選んでもらう)
//   2. [`Machine`](./machine.js) を作って回す
//   3. 機械の画面を [`Terminal`](./terminal.js) へ、端末のキーを機械へ
//
// 機械は画面を知らず、端末は機械を知らない。互いを知っているのはここだけなので、
// 別のOSを載せても、端末を別の実装に差し替えても、直すのはこのファイルで済む。

import { loadWasm, Machine } from './machine.js';
import { Terminal } from './terminal.js';

const $ = id => document.getElementById(id);
const term = new Terminal($('screen'), { scrollback: 1000 });

let machine = null;

function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

function boot(image) {
  machine?.stop();
  try {
    machine = new Machine(image);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  term.reset();
  // 機械 → 端末 (画面)
  machine.onFrame = (cells, row, col, redraw) => {
    term.sample(cells, row, col);
    if (redraw) term.draw();
  };
  // 端末 → 機械 (キー)。物理キーの識別子のまま渡す
  term.onKey = (code, down) => machine.key(code, down);

  window.__machine = machine; // 動作確認用
  window.__term = term;

  machine.start();
  setStatus('起動中… 画面をクリックするとキー入力できます');
  $('screen').focus();
}

async function bootFromUrl() {
  setStatus('fd1440.img を取得中…');
  try {
    const r = await fetch('./fd1440.img');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    boot(new Uint8Array(await r.arrayBuffer()));
  } catch (e) {
    setStatus(`fd1440.img が見つからない (${e.message})。ファイルを選んでください`, true);
  }
}

$('boot').addEventListener('click', bootFromUrl);

$('file').addEventListener('change', async e => {
  const f = e.target.files?.[0];
  if (!f) return;
  setStatus(`${f.name} を読み込み中…`);
  boot(new Uint8Array(await f.arrayBuffer()));
});

$('save').addEventListener('click', () => {
  const blob = new Blob([term.allLines().join('\n')], { type: 'text/plain' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'elks.log';
  a.click();
  URL.revokeObjectURL(a.href);
});

// 読み込みに失敗すると「読み込み中…」のまま黙って止まる。
// 何が起きたか分からないのが一番困るので、必ず画面に出す
window.addEventListener('error', e => setStatus(`エラー: ${e.message}`, true));
window.addEventListener('unhandledrejection', e => setStatus(`エラー: ${e.reason}`, true));

try {
  await loadWasm();
  $('boot').disabled = false;
  setStatus('ディスクイメージを選ぶと起動します');
  const head = await fetch('./fd1440.img', { method: 'HEAD' }).catch(() => null);
  if (head?.ok) await bootFromUrl();
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
