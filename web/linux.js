// ブラウザで 32bit Linux を起動する画面の配線。
//
//   1. カーネル (vmlinuz-lts) と initramfs を fetch する
//   2. ワーカー (linux-worker.js) を起こし、wasm で起動させる
//   3. ワーカーのシリアル出力を ANSI端末 (ansi.js) へ、端末のキーをワーカーへ
//
// **重い計算は全部ワーカー**。ここは配線と描画だけなので、起動中も画面は
// なめらかに更新され、キーも届く。

import { AnsiTerminal } from './ansi.js';

const $ = (id) => document.getElementById(id);
const term = new AnsiTerminal($('screen'), { cols: 80, rows: 24 });

let worker = null;
let booted = false;
let paused = false;

// 端末の入力 → ワーカーへ (UTF-8バイト列にして送る)
term.onData = (s) => {
  if (!worker || !booted) return;
  const bytes = new TextEncoder().encode(s);
  worker.postMessage({ type: 'input', bytes: bytes.buffer }, [bytes.buffer]);
};

// 描画ループ (端末の dirty を見て 60fps で描く)
function draw() {
  term.render();
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);

function setStatus(msg, err = false) {
  const el = $('status');
  el.textContent = msg;
  el.classList.toggle('err', err);
}

async function fetchWithProgress(url, label) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: ${r.status}`);
  const total = +r.headers.get('content-length') || 0;
  const reader = r.body.getReader();
  const chunks = [];
  let got = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    got += value.length;
    if (total) setStatus(`${label} を読み込み中… ${((got / total) * 100) | 0}%`);
    else setStatus(`${label} を読み込み中… ${(got / 1e6).toFixed(1)}MB`);
  }
  const buf = new Uint8Array(got);
  let off = 0;
  for (const c of chunks) { buf.set(c, off); off += c.length; }
  return buf;
}

async function boot() {
  $('boot').disabled = true;
  $('ram').disabled = true;
  term.reset();
  booted = false;

  let kernel, initrd;
  try {
    setStatus('カーネルを読み込み中…');
    kernel = await fetchWithProgress('./vmlinuz-lts', 'カーネル');
    // initramfs は無くてもよい (無ければルートFS無しで止まる)
    try {
      initrd = await fetchWithProgress('./initramfs-mini', 'initramfs');
    } catch {
      initrd = null;
    }
  } catch (e) {
    setStatus(`イメージが読めない: ${e.message}。tools/fetch-images.sh linux と make-mini-initramfs.sh で用意する`, true);
    $('boot').disabled = false;
    $('ram').disabled = false;
    return;
  }

  setStatus('ワーカーを起動し、カーネルを展開中… (シェルまで1〜2分)');

  // 前のワーカーがあれば止める
  if (worker) worker.terminate();
  worker = new Worker('./linux-worker.js', { type: 'module' });

  worker.onmessage = (e) => {
    const msg = e.data;
    switch (msg.type) {
      case 'ready': {
        const ramMb = +$('ram').value;
        // 転送可能オブジェクトで渡す (コピーを避ける)
        worker.postMessage(
          { type: 'boot', kernel: kernel.buffer, initrd: initrd?.buffer, cmdline: 'console=ttyS0', ramMb },
          initrd ? [kernel.buffer, initrd.buffer] : [kernel.buffer],
        );
        booted = true;
        $('pause').disabled = false;
        $('screen').focus();
        break;
      }
      case 'serial':
        term.write(new Uint8Array(msg.bytes));
        break;
      case 'status':
        $('gauge').textContent = msg.mips ? `${msg.mips.toFixed(1)} MIPS` : '';
        if (booted) setStatus('起動中… (画面をクリックしてキー入力できる)');
        break;
      case 'trap':
        setStatus(`停止: ${msg.reason}`, true);
        $('pause').disabled = true;
        break;
    }
  };
}

$('boot').addEventListener('click', boot);
$('pause').addEventListener('click', () => {
  if (!worker || !booted) return;
  paused = !paused;
  worker.postMessage({ type: paused ? 'pause' : 'resume' });
  $('pause').textContent = paused ? '再開' : '一時停止';
});

// クリックで端末にフォーカス (キー入力できるように)
$('screen').addEventListener('click', () => $('screen').focus());
