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
import { ROOTFS } from './machines.js';

// 端末は**一度作ったら使い回す**。AnsiTerminal はコンストラクタで canvas に
// keydown を張り、外す口が無い。選び直すたびに作ると同じ canvas に
// リスナが積もり、キーが二重三重に届く
let sharedTerm = null;

/**
 * initramfs の実物から、要るRAM (MB) を決める。
 *
 * **「置けたか」ではなく「展開しきれるか」で決まる。** カーネルは initramfs を
 * tmpfs へ展開するので、圧縮イメージと展開後の中身がしばらく同時にRAMに載る。
 * 足りないとカーネルは落ちずに "rootfs image is not initramfs (write error)"
 * と言って途中でやめ、**尻切れのルートFSのままシェルまで来てしまう**。
 *
 * gzipなら展開後の大きさは末尾4バイト (ISIZE) に書いてある。Rust側の
 * `initrd_ram_needed` と同じ算数 — 68MiBはカーネルの取り分で実測から決めた値。
 * 下限ちょうどに寄せず25%足すのは、**ぴったりだと「4ファイルだけ欠ける」**
 * という一番気づけない壊れ方を引くため (docs/explanation/pitfalls.md #14)。
 */
function autoRam(initrd) {
  const n = initrd.length;
  const gz = n > 18 && initrd[0] === 0x1f && initrd[1] === 0x8b;
  const unpacked = gz
    ? ((initrd[n - 4] | (initrd[n - 3] << 8) | (initrd[n - 2] << 16) | (initrd[n - 1] << 24)) >>> 0)
    : n;
  const need = n + unpacked + (68 << 20);
  const mb = Math.ceil((need * 1.25) / (64 << 20)) * 64;
  return Math.max(128, Math.min(2048, mb));
}

/**
 * Linux 一式を `canvas` に組み立てる。
 *
 * @param {HTMLCanvasElement} canvas シリアル端末を描く先
 * @param {object} opts
 *   onStatus(msg, err)  状態表示の依頼 (ページの status 欄へ)
 *   onState()           起動/停止など、ボタンの見た目に関わる変化の通知
 *   rootfs()            電源を入れる瞬間に呼ぶ。{name, ramMb} を返す
 *                       (ramMb が 0/未指定なら実物から自動で決める)
 * @returns 操作するための取っ手
 */
export function mountLinux(canvas, opts = {}) {
  const term = (sharedTerm ??= new AnsiTerminal(canvas, { cols: 80, rows: 24 }));
  term.reset();
  // クリップボードの行き先は**呼び手 (main.js) の一本道**。
  // ここで onData へ直に流していたので、VGA機とは別の作法になっていた
  // 画素の顔のときの物理キー。ワーカー越しに 8042 へ (VGA機の key() と同じ経路)
  term.onKey = (code, down) => worker?.postMessage({ type: 'key', code, down });
  // マウス (相対移動。絶対位置に見せる仕掛けは ansi.js)。同じ 8042 の第2ポートへ
  term.onMouse = (dx, dy, buttons) => worker?.postMessage({ type: 'mouse', dx, dy, buttons });
  // (捕獲は無くなった — マウスは画面の上に居る間だけゲストへ届く。念のため口は残す)
  term.onCapture = (on) => {
    canvas.classList.toggle('captured', on);
  };
  term.onPaste = (text) => opts.onPaste?.(text);
  term.onPasteRequest = () => opts.onPasteRequest?.();
  term.onCopyRequest = () => opts.onCopyRequest?.();
  const status = (msg, err = false) => opts.onStatus?.(msg, err);

  let worker = null;
  /** 実際に読んだイメージの名前。**画面の「起動元」に出す** —
      前の機械のラベル (fd2880.img) が残ると、別物を見ていることになる */
  let imageName = '';
  // **表示は実物に合わせる。** つまみ (?initrd= / ?ram=) を足したのに
  // ラベルが固定だと、画面が嘘をつく
  let usedInitrd = 'initramfs-mini';
  /** 挿したディスクのファイル名 (ディスク無しなら空) */
  let usedDisk = '';
  let usedRam = 128;
  let booted = false;
  let paused = false;
  /** イメージ取得〜ワーカー起動の間。二度押しと再入を防ぐ */
  let busy = false;
  let mips = 0;
  /** アイドル (HLT待ち) か。ワーカーが実時間に間を合わせている印 */
  let idle = false;
  /** 起動の定規 (headless.mjs と同じ定義): 機械を組んでからシリアルに
      バナーが出るまでの秒数。尺度は時間で統一 (2026-08-13)。
      bootT0 が非null の間だけ計測中 */
  let bootT0 = null;
  let bootSecs = null;
  let bannerTail = ''; // バナー検出用のシリアル末尾 (全文は持たない)
  const BANNER = 'busybox shell';
  const latin1 = new TextDecoder('latin1');
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
  }

  /** JITのon/offを実行中に切り替える (比較実験用の外部フラグ) */
  function setJit(on) {
    if (worker && booted) worker.postMessage({ type: 'jit', on: !!on });
  };

  // 描画ループ (端末の dirty を見て 60fps で描く)。
  // **取り外されたら止める** — 回しっぱなしだと裏で描き続ける
  let alive = true;
  /** 電源OFFで進む世代。**仕掛かり中のboot()を無効化する** — OFFの直後に
      fetch済みの前のbootがワーカーを立ち上げてくる事故を防ぐ */
  let bootGen = 0;
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
   * 起動する。**既定は電源ONからのフル起動** — カーネルログが流れる本物の
   * 起動がこのエミュレータの意味なので、起動済みスナップショットの自動復帰は
   * やめた (2026-08-13)。復元はユーザーの明示操作 (.rx86snapの書出/復元、
   * Tier 3g) だけ — その場合は `{ snapshot }` でここへ入る
   */
  /**
   * @param {object} [o]
   * @param {Uint8Array} [o.snapshot] 起動済みの状態から戻す
   * @param {Uint8Array} [o.kernel] **持ち込みのカーネル** (ドロップされた
   *   vmlinux / bzImage)。指定があれば取りに行かず、これを起動する
   * @param {string} [o.kernelName] 画面に出す名前
   */
  async function boot({ snapshot: given = null, kernel: givenKernel = null, kernelName = '', iso: givenIso = null, isoName = '' } = {}) {
    if (busy) return;
    const gen = bootGen; // この起動が属する世代 (電源OFFで古くなる)
    busy = true;
    booted = false;
    paused = false;
    mips = 0;
    term.reset();
    opts.onState?.();

    const snapshot = given; // ファイルからの復元 (Tier 3g) だけがここを通る
    let kernel = null;
    let initrd = null;
    // **どのルートFSを載せるかは、電源を入れるこの瞬間に決まる** (NICと同じ)。
    // 選ぶのは画面 (main.js のツールバー) で、URL (`?initrd=` / `?ram=`) は
    // その初期値。RAMは 'auto' なら initrd の実物から決める (下の autoRam)
    const want = opts.rootfs?.() ?? {};
    // 選択はROOTFSの項に解決する。**メモリ型かディスク型かはデータが言う**
    // (initrdだけの項 = 従来のinitramfs起動、disk付きの項 = vdaに挿して
    //  ミニのinitが移り住む)。知らない名前は先頭 (ミニ) に落とす
    const entry = ROOTFS.find(r => r.name === want.name) ?? ROOTFS[0];
    const initrdName = entry.initrd;
    usedInitrd = initrdName;
    usedDisk = entry.disk ?? '';
    // RAMの確定は initrd を読んだ後 (自動のときは中身の大きさが要る)
    let ramMb = want.ramMb || 0;

    // **ISO (El Torito) から起動する道** — ルートFSと排他 (画面のつまみがそうしている)。
    // カーネルも initramfs も ISO の中にあり、BIOS 経由で isolinux が上げる。
    // 持ち込みの ISO (ドロップ) はファイルそのもの、ライブラリの ISO は名前で取る
    let iso = givenIso;
    const isoFile = givenIso ? isoName || 'ISO' : want.iso || '';
    if (!snapshot && !iso && isoFile) {
      imageName = isoFile;
      try {
        iso = await fetchWithProgress(`./${isoFile}`, isoFile);
      } catch (e) {
        status(`${isoFile} が読めない: ${e.message} (tools/images/sh/fetch-images.sh tinycore で web/ に置く)`, true);
        busy = false;
        opts.onState?.();
        return;
      }
    }
    if (iso) {
      imageName = isoFile;
      usedInitrd = '';
      usedDisk = '';
      status(`${isoFile} を起動します (BIOS 経由)`);
    }

    if (iso) {
      // ISO 起動にカーネルの取り寄せは無い
    } else if (!snapshot && givenKernel) {
      // 持ち込みのカーネル。**initramfs はページの隣から借りる** —
      // カーネルだけ落とされても、ルートFSが無ければシェルに着けないので
      kernel = givenKernel;
      imageName = kernelName || 'カーネル';
      status(`${imageName} を起動します`);
      try {
        initrd = await fetchWithProgress(`./${initrdName}`, initrdName);
      } catch {
        initrd = null;
      }
    } else if (!snapshot) {
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
          imageName = 'vmlinux-lts.gz';
          status('カーネルを展開中… (ホスト側でやる — ゲストにやらせると起動の55%を食う)');
          const ds = new Blob([gz]).stream().pipeThrough(new DecompressionStream('gzip'));
          kernel = new Uint8Array(await new Response(ds).arrayBuffer());
        } catch {
          kernel = await fetchWithProgress('./vmlinuz-lts', 'カーネル (bzImage)');
          imageName = 'vmlinuz-lts';
        }
        // initramfs は無くてもよい (無ければルートFS無しで止まる)
        try {
          initrd = await fetchWithProgress(`./${initrdName}`, initrdName);
        } catch {
          initrd = null;
        }
      } catch (e) {
        status(
          `イメージが読めない: ${e.message}。` +
            'tools/images/sh/fetch-images.sh linux と make-mini-initramfs.sh で作り、web/ に置く',
          true,
        );
        busy = false;
        opts.onState?.();
        return;
      }
    }
    if (!alive || gen !== bootGen) return; // fetchの間に切替 or 電源OFF

    // ディスク型ならイメージも取る。**無ければディスク無しで進む** —
    // ミニのシェルには落ちるので、真っ暗になるよりは説明して動かす
    let disk = null;
    if (!snapshot && usedDisk && !iso) {
      try {
        disk = await fetchWithProgress(`./${usedDisk}`, usedDisk);
        // .gz は輸送路の圧縮。**ここ (ホスト) で1回だけ解く** — ゲストの
        // エミュレートされたCPUに読むたび解凍させると cold read の sys が
        // 0.9s→15.6s に化ける (実測は make-gcc-disk.sh の注釈と disk.md)
        if (usedDisk.endsWith('.gz')) {
          const t0 = performance.now();
          const stream = new Blob([disk]).stream().pipeThrough(new DecompressionStream('gzip'));
          disk = new Uint8Array(await new Response(stream).arrayBuffer());
          console.log(`disk: ホスト側で解凍 ${disk.length}B (${Math.round(performance.now() - t0)}ms)`);
        }
      } catch {
        status(`${usedDisk} が無い (tools/images/sh/make-gcc-disk.sh で作って web/ に置く)。ディスク無しで起動します`, true);
        usedDisk = '';
        disk = null;
      }
    }
    if (!alive || gen !== bootGen) return;

    // **RAMは実物を見てから決める。** 自動 (ramMb未指定) のときだけ効く。
    // ディスク型はinitrdがミニなので自然に128MBになる — ディスクの中身は
    // 読んだ分しかページキャッシュに載らず、RAMの頭数に入れなくてよい
    if (!ramMb) ramMb = initrd ? autoRam(initrd) : 128;
    if (iso && ramMb < 128) ramMb = 128; // Tiny Core はカーネル + initrd 展開で 64MB では足りない
    usedRam = ramMb;

    status(
      snapshot
        ? '起動済みの機械を復元中…'
        : iso
          ? 'ワーカーを起動し、ISO から起動中… (isolinux → Linux、シェルまで 1〜2 分)'
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
            // 定規の始点 = 機械を組み始める瞬間 (headless.mjs の t0 と同じ)。
            // fetch は含めない — 測るのは計算の速さで、回線の速さではない
            bootT0 = performance.now();
            bootSecs = null;
            bannerTail = '';
            worker.postMessage(
              {
                type: 'boot',
                // ISO なら kernel/initrd/disk は無い (BIOS が CD から上げる)
                iso: iso?.buffer,
                kernel: kernel?.buffer,
                initrd: initrd?.buffer,
                disk: disk?.buffer,
                // フレームバッファを申告するときは tty0 を**最後の** console= にする。
                // カーネルのログは両方に出るが、/dev/console (= initのシェル) は
                // 最後に書いた方なので、プロンプトが画面 (fbcon) に出る。
                // 逆順 (ttyS0が最後) だとログだけ映ってシェルは見えないシリアルに
                // 居る — 「FBだと触れない」の正体 (2026-08-21)
                cmdline: opts.fb?.() ? 'console=ttyS0 console=tty0' : 'console=ttyS0',
                ramMb,
                // 解像度はルートFSの項が言える (X入りは 1024×768)。既定は 640×480
                lfb: opts.fb?.() ? (entry.lfb ?? { width: 640, height: 480 }) : null,
                // NICを挿すかは電源を入れるこの瞬間に決まる (VGA機と同じ)。
                // macの有無だけで伝える — 線の状態はメイン側の持ち物
                mac: opts.mac?.(),
                // JIT (F1d wasmバックエンド)。起動時の初期値 — 実行中も
                // setJit() でon/offできる (決定性はJIT on/offで不変が門番)
                jit: opts.jit?.() ?? false,
              },
              [iso?.buffer, kernel?.buffer, initrd?.buffer, disk?.buffer].filter(Boolean),
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
        case 'serial': {
          const bytes = new Uint8Array(msg.bytes);
          term.write(bytes);
          // 起動の定規: バナーが流れてきた瞬間に止める (境界跨ぎ対策で末尾を継ぐ)
          if (bootT0 !== null) {
            bannerTail = (bannerTail + latin1.decode(bytes)).slice(-4096);
            if (bannerTail.includes(BANNER)) {
              bootSecs = (performance.now() - bootT0) / 1000;
              bootT0 = null;
              bannerTail = '';
            }
          }
          break;
        }
        case 'text': {
          // ISO 機のテキスト VRAM (80×25、文字+属性)。描き手は端末と同じ canvas
          const cells = new Uint8Array(msg.cells);
          term.showVga(cells, msg.row, msg.col, msg.charset);
          // **ISO の起動の定規は別**: 画面が VGA なのでバナー (シリアル) が来ない。
          // 人間が「着いた」と判断するのと同じ合図 — カーソルの居る行が
          // シェルのプロンプト ($ / #) で終わった瞬間で止める
          if (bootT0 !== null && msg.row < 25) {
            let line = '';
            for (let x = 0; x < 80 && x < msg.col; x++) line += String.fromCharCode(cells[(msg.row * 80 + x) * 2] || 32);
            if (/[$#] ?$/.test(line)) {
              bootSecs = (performance.now() - bootT0) / 1000;
              bootT0 = null;
            }
          }
          break;
        }
        case 'lfb': {
          // efifb が描いた一枚 (ワーカーが RGBA に詰め替え済み、fmt='rgba')。
          // 描き手は端末と同じ canvas
          term.drawRgb(new Uint8Array(msg.bytes), msg.width, msg.height, msg.bpp ?? 24, msg.fmt ?? 'raw');
          // 描いたらバッファを返す (背圧 — ワーカーはこれが戻るまで次を送らない)
          worker?.postMessage({ type: 'lfb-ack', bytes: msg.bytes }, [msg.bytes]);
          break;
        }
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
        case 'net-tx':
          // ゲストが送ったフレーム。行き先 (WebSocket) はメインの持ち物
          for (const f of msg.frames) opts.onNetTx?.(new Uint8Array(f));
          break;
        case 'tone':
          // PCスピーカー。鳴らすのはメイン (WebAudioはワーカーから触れない)
          opts.onTone?.(msg.hz);
          break;
        case 'trap':
          term.releaseCapture?.();
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
    setJit,
    get booted() { return booted; },
    get busy() { return busy; },
    get paused() { return paused; },
    get mips() { return mips; },
    get idle() { return idle; },
    /** 起動〜バナーの秒数 (headless.mjs と同じ定義)。未到達なら null */
    get bootSecs() { return bootSecs; },
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
    /** 文字列をゲストへ流す (貼り付け)。シリアルは受け側の行列が深いので一息でよい */
    send(text) {
      term.onData?.(text);
    },
    /** 外から届いたEthernetフレームをゲストへ (注入はワーカーのスライス境界) */
    netInject(frame) {
      if (worker && booted) worker.postMessage({ type: 'net-rx', frames: [frame.buffer] }, [frame.buffer]);
    },
    /** 実際に読んだイメージの名前 (まだ読んでいなければ空) */
    /** 実際に使った initramfs の名前 (?initrd= で差し替わる) */
    get initrdName() {
      return usedInitrd;
    },
    /** 挿したディスクのファイル名 (ディスク無しなら空) */
    get diskName() {
      return usedDisk;
    },
    /** 実際に渡したRAM (MB。?ram= で変わる) */
    get ramMb() {
      return usedRam;
    },
    get imageName() {
      return imageName;
    },
    /** 何か選ばれているか (中身は組み立てない) */
    hasSelection() {
      return term.hasSelection();
    },
    /** ドラッグで選んだ文字列 (何も選んでいなければ空) */
    selectedText() {
      return term.selectedText();
    },
    /** 電源を切る。**機械 (この取っ手) は残る** — ルートFSを選び直して
        もう一度 boot() できる。取り外し (destroy) とは別の顔 */
    powerOff() {
      bootGen += 1; // 仕掛かり中のbootを無効化
      worker?.terminate();
      worker = null;
      booted = false;
      busy = false;
      paused = false;
      dbgFlush();
      term.reset();
      status('電源を切りました — ルートFSを選び直して、電源でまた起動できます');
      opts.onState?.();
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
