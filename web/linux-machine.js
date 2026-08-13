// 32bit Linux をメインページの端末ペインで回す部品。
//
//   1. カーネル (vmlinuz-lts) と initramfs を fetch する
//   2. ワーカー (linux-worker.js) を起こし、wasm で起動させる
//   3. ワーカーのシリアル出力を ANSI端末 (ansi.js) へ、端末のキーをワーカーへ
//
// **重い計算は全部ワーカー**。ここは配線と描画だけなので、起動中も画面は
// なめらかに更新され、キーも届く。
//
// 「mount して取っ手を返す」形にしてある。
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
  /** 「状態を保存」の控え。64MBあるので localStorage ではなくメモリに持つ
      (ページを閉じると消える) */
  let captureWaiters = []; // captureState (書出) の返事待ち
  /** 保存時の端末の姿 (画面・カーソル・履歴)。機械の状態にシリアルの履歴は
      入らないので、端末側の姿は端末側で控える — VGA機と使い勝手を揃えるため */

  // --- デバッガの覗き見RPC ---
  //
  // 機械はワーカーの中に居るので、メインスレッドの Emulator と違って
  // 同期では覗けない。**同じメソッド名で Promise を返す代役**を渡し、
  // デバッガ側は await で呼ぶ (メインスレッドの機械は同期値を await しても
  // そのまま解決するので、呼ぶ側は相手がどちらかを知らずに済む)
  let dbgSeq = 0;
  const dbgPending = new Map();
  function dbgCall(method, ...args) {
    if (!worker) return Promise.resolve(null);
    const id = ++dbgSeq;
    return new Promise((resolve) => {
      dbgPending.set(id, resolve);
      worker.postMessage({ type: 'dbg', id, method, args });
    });
  }
  /** ワーカーを取り替える/畳むとき、待ちぼうけの約束を全部 null で流す */
  function dbgFlush() {
    for (const resolve of dbgPending.values()) resolve(null);
    dbgPending.clear();
  }
  const dbgEmu = Object.fromEntries(
    [
      'cpu_json', 'watches_json', 'trace_json', 'read_mem',
      'set_break', 'watch_mem', 'watch_io', 'clear_debug',
      'step_one', 'take_stop', 'is_stopped', 'set_counting', 'record_trace',
    ].map((m) => [m, (...args) => dbgCall(m, ...args)]),
  );

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

  /**
   * 起動する。既定はスナップショット優先 (数秒でシェル)。
   * `{ full: true }` で**必ずフル起動** — カーネルログが流れる本物の起動を
   * UI (電源ONボタン) から選べるようにする。スナップショットに乗っ取られて
   * フルブートが見られない、を防ぐ
   */
  async function boot({ full = false, snapshot: given = null } = {}) {
    if (busy) return;
    busy = true;
    booted = false;
    paused = false;
    mips = 0;
    term.reset();
    opts.onState?.();

    // まず**起動済みスナップショット**を探す (tools/make-linux-snapshot.sh が作る)。
    // あれば数秒で立つ — 「シンプルなカーネルの起動に1分」への即効薬で、
    // フル起動は電源ONボタンとスナップショット不在時の道
    let snapshot = given; // ファイルからの復元 (Tier 3g) はここから入る
    let kernel = null;
    let initrd = null;
    if (!full && !snapshot) {
      try {
        const gz = await fetchWithProgress('./linux-booted.snap.gz', '起動済みスナップショット');
        status('スナップショットを展開中…');
        const ds = new Blob([gz]).stream().pipeThrough(new DecompressionStream('gzip'));
        snapshot = new Uint8Array(await new Response(ds).arrayBuffer());
      } catch {
        snapshot = null; // 無ければカーネルからのフル起動に落ちる
      }
    }

    if (!snapshot) {
      // **既定は bzImage — 自己解凍ステブごと実行する本物のフル起動。**
      // 実機がやることを全部やるのがこのエミュレータの意味なので、
      // 速さのための近道 (vmlinux直接ロード) は ?kernel=vmlinux の
      // 明示指定に格下げした (経路比較の計測用、docs/reference/perf.md)
      const wantVmlinux = new URLSearchParams(location.search).get('kernel') === 'vmlinux';
      try {
        try {
          if (!wantVmlinux) throw new Error('既定はbzImage');
          // vmlinux (非圧縮ELF): 自己解凍ステブをホスト側の展開で飛ばす近道
          const gz = await fetchWithProgress('./vmlinux-lts.gz', 'カーネル (vmlinux)');
          status('カーネルを展開中… (ホスト側でやる — ゲストにやらせると起動の55%を食う)');
          const ds = new Blob([gz]).stream().pipeThrough(new DecompressionStream('gzip'));
          kernel = new Uint8Array(await new Response(ds).arrayBuffer());
        } catch {
          kernel = await fetchWithProgress('./vmlinuz-lts', 'カーネル (bzImage)');
        }
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
    }
    if (!alive) return; // fetch の間に別のマシンへ切り替えられた

    status(
      snapshot
        ? '起動済みの機械を復元中…'
        : 'ワーカーを起動し、カーネルを展開中… (シェルまで1〜2分)',
    );

    // 前のワーカーがあれば止める (覗き見の待ちも流す)
    if (worker) worker.terminate();
    dbgFlush();
    worker = new Worker('./linux-worker.js', { type: 'module' });

    worker.onmessage = (e) => {
      const msg = e.data;
      switch (msg.type) {
        case 'ready': {
          // 転送可能オブジェクトで渡す (コピーを避ける)
          if (snapshot) {
            worker.postMessage({ type: 'boot', snapshot: snapshot.buffer }, [snapshot.buffer]);
          } else {
            worker.postMessage(
              { type: 'boot', kernel: kernel.buffer, initrd: initrd?.buffer, cmdline: 'console=ttyS0', ramMb: 128 },
              initrd ? [kernel.buffer, initrd.buffer] : [kernel.buffer],
            );
          }
          booted = true;
          busy = false;
          status(
            snapshot
              ? '起動済みの状態から再開した。画面をクリックするとキー入力できます'
              : '起動中… 画面をクリックするとキー入力できます',
          );
          canvas.focus();
          opts.onState?.();
          break;
        }
        case 'serial':
          term.write(new Uint8Array(msg.bytes));
          break;
        case 'state': {
          // captureState (スナップショット書出) の返事。控えは持たない —
          // 保存/復元はファイルに一本化した (Tier 3g)
          const bytes = new Uint8Array(msg.bytes);
          const r = captureWaiters.splice(0);
          for (const resolve of r) resolve(bytes);
          break;
        }
        case 'loaded':
          // 機械が戻った。端末はリセットして以後のシリアルだけを映す
          // (ファイルに端末の見た目は入っていない — Enterで即プロンプトが出る)
          term.reset();
          status('スナップショットの状態に戻した');
          canvas.focus();
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
        case 'dbg-result': {
          const resolve = dbgPending.get(msg.id);
          if (resolve) {
            dbgPending.delete(msg.id);
            resolve(msg.result);
          }
          break;
        }
        case 'dbg-stop':
          // 見張りが機械を止めた。ワーカーのループは既に降りているので、
          // こちらの「一時停止」の帳簿を合わせてからデバッガへ理由を渡す
          paused = true;
          opts.onState?.();
          opts.onDbgStop?.(msg.why);
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
    /** 今の状態を取り出す (スナップショット書出用)。返事が来たら解決 */
    captureState() {
      if (!worker || !booted) return Promise.resolve(null);
      return new Promise((resolve) => {
        captureWaiters.push(resolve);
        worker.postMessage({ type: 'save' });
      });
    },
    /** ファイルから読んだ状態へ戻す (Tier 3g) */
    loadStateBytes(bytes) {
      if (worker && booted) {
        const copy = bytes.slice();
        worker.postMessage({ type: 'load', bytes: copy.buffer }, [copy.buffer]);
      }
    },
    /** デバッガが覗くための代役 (各メソッドが Promise を返す)。起動前は null */
    get dbgEmu() { return worker && booted ? dbgEmu : null; },
    /** 端末が見た全文 (履歴1000行+今の画面)。VGA機の「ログを保存」と同じ意味論。
        スナップショット起動でも画面に見えている分は必ず入る */
    get logText() {
      return term.allText();
    },
    /** 取り外す。**走らせっぱなしにしない** — ワーカーが裏でCPUを食い続ける */
    destroy() {
      alive = false;
      worker?.terminate();
      worker = null;
      dbgFlush();
      term.onData = null;
    },
  };
}
