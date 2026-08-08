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

// ---------- スナップショット ----------
//
// 機械の状態は Rust 側がコンパクトなバイナリで書き出す (連長圧縮済み)。
// **JSONで束ねるのはここの仕事**で、いつ・何のイメージから取ったのかという
// 人間向けの情報を添える。中身をJSONの数値配列にすると1MBが数MBに膨れるので、
// バイナリは Base64 の文字列1本にして入れる。

const SNAP_FORMAT = 'rustx86-snapshot';
const SNAP_KEY = 'rustx86.snapshot';

/**
 * gzip をかけてから Base64 にする。
 *
 * 連長圧縮 (Rust側) が潰せるのは**ゼロの海**だけで、ディスクイメージのような
 * 実データには効かない。1.44MBのフロッピーがそのまま乗ると 3.5MB になり、
 * localStorage (5MB程度) に1個しか入らなかった。
 * 汎用の圧縮を通すと数分の1になる。
 */
async function gzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

async function gunzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

const toBase64 = bytes => {
  let s = '';
  // 一度に渡すと引数が多すぎて落ちるので刻む
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
};

const fromBase64 = b64 => Uint8Array.from(atob(b64), c => c.charCodeAt(0));

async function snapshotJson(name) {
  const bytes = machine.saveState();
  const packed = await gzip(bytes);
  return JSON.stringify({
    format: SNAP_FORMAT,
    version: 1,
    created: new Date().toISOString(),
    image: name ?? 'unknown',
    bytes: bytes.length,
    encoding: 'gzip+base64',
    state: toBase64(packed),
  });
}

async function applySnapshotJson(text) {
  const o = JSON.parse(text);
  if (o.format !== SNAP_FORMAT) throw new Error('rustx86 のスナップショットではない');
  let bytes = fromBase64(o.state);
  if (o.encoding === 'gzip+base64') bytes = await gunzip(bytes);
  machine.loadState(bytes);
  return o;
}

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
  $('snap').disabled = !on;
  $('snapfile').disabled = !on;
  $('restore').disabled = !on || !localStorage.getItem(SNAP_KEY);
}

/** 最後に起動したイメージの名前。スナップショットに添える */
let lastLabel = '';

function boot(image, label) {
  lastLabel = label;
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

$('snap').addEventListener('click', async () => {
  if (!machine) return;
  try {
    const json = await snapshotJson(lastLabel);
    localStorage.setItem(SNAP_KEY, json);
    setStatus(`状態を保存した (${(json.length / 1024).toFixed(0)} KB、この端末に残る)`);
    syncControls();
  } catch (e) {
    // localStorage は数MBで埋まる。落ちた理由を隠さない
    setStatus(`保存できない: ${e.message}`, true);
  }
});

$('restore').addEventListener('click', async () => {
  const json = localStorage.getItem(SNAP_KEY);
  if (!machine || !json) return;
  try {
    const o = await applySnapshotJson(json);
    term.reset();
    setStatus(`${o.created} の状態に戻した (${o.image})`);
    $('screen').focus();
  } catch (e) {
    setStatus(`復元できない: ${e.message}`, true);
  }
});

$('snapfile').addEventListener('click', async () => {
  if (!machine) return;
  const blob = new Blob([await snapshotJson(lastLabel)], { type: 'application/json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = `rustx86-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
  a.click();
  URL.revokeObjectURL(a.href);
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
  // 落とされたものがスナップショットならそこへ戻る。ディスクなら起動する
  if (f.name.endsWith('.json')) {
    if (!machine) {
      setStatus('先にディスクイメージを起動してください', true);
      return;
    }
    try {
      const o = await applySnapshotJson(await f.text());
      term.reset();
      setStatus(`${f.name} の状態に戻した (${o.image}、${o.created})`);
      $('screen').focus();
    } catch (err) {
      setStatus(`復元できない: ${err.message}`, true);
    }
    return;
  }
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
