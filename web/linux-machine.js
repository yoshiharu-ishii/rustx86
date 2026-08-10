// 32bit Linux をメインページの端末ペインで回す部品。
//
//   1. カーネル (vmlinuz-lts) と initramfs を fetch する
//   2. ワーカー (linux-worker.js) を起こし、wasm で起動させる
//   3. ワーカーのシリアル出力を ANSI端末 (ansi.js) へ、端末のキーをワーカーへ
//
// **重い計算は全部ワーカー**。ここは配線と描画だけなので、起動中も画面は
// なめらかに更新され、キーも届く。
//
// ベンチ (bench.js) と同じ「mount して取っ手を返す」形にしてある。
// 元は独立ページ (linux.html) だったが、「左のメニューで選ぶと真ん中の端末で
// 動く」というこのアプリの作法に合わせて、ページではなく部品にした。

import { AnsiTerminal } from './ansi.js';

// 端末は**一度作ったら使い回す**。AnsiTerminal はコンストラクタで canvas に
// keydown を張り、外す口が無い。選び直すたびに作ると同じ canvas に
// リスナが積もり、キーが二重三重に届く
let sharedTerm = null;

/**
 * Linux 一式を `canvas` に組み立てる。
 *
 * @param {HTMLCanvasElement} canvas シリアル端末を描く先
 * @param {object} opts
 *   onStatus(msg, err)  状態表示の依頼 (ページの status 欄へ)
 *   onState()           起動/停止など、ボタンの見た目に関わる変化の通知
 * @returns 操作するための取っ手
 */
export function mountLinux(canvas, opts = {}) {
  const term = (sharedTerm ??= new AnsiTerminal(canvas, { cols: 80, rows: 24 }));
  term.reset();
  const status = (msg, err = false) => opts.onStatus?.(msg, err);

  let worker = null;
  let booted = false;
  let paused = false;
  /** イメージ取得〜ワーカー起動の間。二度押しと再入を防ぐ */
  let busy = false;
  let mips = 0;
  /** アイドル (HLT待ち) か。ワーカーが実時間に間を合わせている印 */
  let idle = false;

  // 端末の入力 → ワーカーへ (UTF-8バイト列にして送る)。
  // onData は毎回張り替える — 前回 mount の閉包は古いワーカーを見ている
  term.onData = (s) => {
    if (!worker || !booted) return;
    const bytes = new TextEncoder().encode(s);
    worker.postMessage({ type: 'input', bytes: bytes.buffer }, [bytes.buffer]);
  };

  // 描画ループ (端末の dirty を見て 60fps で描く)。
  // **取り外されたら止める** — 回しっぱなしだと裏で描き続ける
  let alive = true;
  (function draw() {
    if (!alive) return;
    term.render();
    requestAnimationFrame(draw);
  })();

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
      if (total) status(`${label} を読み込み中… ${((got / total) * 100) | 0}%`);
      else status(`${label} を読み込み中… ${(got / 1e6).toFixed(1)}MB`);
    }
    const buf = new Uint8Array(got);
    let off = 0;
    for (const c of chunks) { buf.set(c, off); off += c.length; }
    return buf;
  }

  async function boot() {
    if (busy) return;
    busy = true;
    booted = false;
    paused = false;
    mips = 0;
    term.reset();
    opts.onState?.();

    let kernel, initrd;
    try {
      kernel = await fetchWithProgress('./vmlinuz-lts', 'カーネル');
      // initramfs は無くてもよい (無ければルートFS無しで止まる)
      try {
        initrd = await fetchWithProgress('./initramfs-mini', 'initramfs');
      } catch {
        initrd = null;
      }
    } catch (e) {
      status(
        `イメージが読めない: ${e.message}。` +
          'tools/fetch-images.sh linux と make-mini-initramfs.sh で作り、web/ に置く',
        true,
      );
      busy = false;
      opts.onState?.();
      return;
    }
    if (!alive) return; // fetch の間に別のマシンへ切り替えられた

    status('ワーカーを起動し、カーネルを展開中… (シェルまで1〜2分)');

    // 前のワーカーがあれば止める
    if (worker) worker.terminate();
    worker = new Worker('./linux-worker.js', { type: 'module' });

    worker.onmessage = (e) => {
      const msg = e.data;
      switch (msg.type) {
        case 'ready': {
          // 転送可能オブジェクトで渡す (コピーを避ける)
          worker.postMessage(
            { type: 'boot', kernel: kernel.buffer, initrd: initrd?.buffer, cmdline: 'console=ttyS0', ramMb: 128 },
            initrd ? [kernel.buffer, initrd.buffer] : [kernel.buffer],
          );
          booted = true;
          busy = false;
          status('起動中… 画面をクリックするとキー入力できます');
          canvas.focus();
          opts.onState?.();
          break;
        }
        case 'serial':
          term.write(new Uint8Array(msg.bytes));
          break;
        case 'status':
          mips = msg.mips || 0;
          idle = !!msg.idle;
          break;
        case 'trap':
          booted = false;
          status(`停止: ${msg.reason} — 画面は倒れた瞬間のまま`, true);
          opts.onState?.();
          break;
      }
    };
  }

  return {
    boot,
    get booted() { return booted; },
    get busy() { return busy; },
    get paused() { return paused; },
    get mips() { return mips; },
    get idle() { return idle; },
    setPaused(v) {
      if (!worker || !booted || paused === v) return;
      paused = v;
      worker.postMessage({ type: v ? 'pause' : 'resume' });
    },
    /** 取り外す。**走らせっぱなしにしない** — ワーカーが裏でCPUを食い続ける */
    destroy() {
      alive = false;
      worker?.terminate();
      worker = null;
      term.onData = null;
    },
  };
}
