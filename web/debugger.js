/**
 * CPUを直接覗くデバッガ。
 *
 * ## なぜ子ウインドウなのか
 *
 * 画面の横に並べたいからである。エミュレータの画面を見ながらレジスタを追う、
 * というのがデバッグの姿勢で、**同じページの中で場所を取り合うと両方が狭くなる**。
 * 子ウインドウなら別のディスプレイにも置ける。
 *
 * ## ただしポップアップは塞がれる
 *
 * クリックから開いても、環境によっては `window.open` が拒まれる
 * (実際にこの実装を確かめている最中に塞がれた)。塞がれたときに
 * 「デバッガが開かない」で終わるのは道具として失格なので、
 * **同じ中身をページ内のパネルに出す**ように落ちる。
 *
 * 中身を組み立てる [`mount`] は「どの document に、どこへ」だけを受け取り、
 * 子ウインドウかページ内かを知らない。
 *
 * ## 親子の通し方
 *
 * `window.open()` で開いた同一オリジンの窓は、親からDOMを直接触れる。
 * postMessage で値を送り合う必要はない。
 *
 *   親 (main.js) ──── Emulator (wasm) を持っている
 *    │  ・止まったときに onStop() を呼ぶ
 *    │  ・走っている間は10Hzで render() を回す
 *    ↓
 *   子 (この窓/パネル) ── 表示するだけ。操作は親のコールバックを呼ぶ
 *
 * ## 機械はメインスレッドとは限らない (32bit Linux)
 *
 * Linuxの機械はワーカーの中に居る。親が渡す `emu()` は、そのときは
 * **同じメソッド名で Promise を返す代役** (linux-machine.js の覗き見RPC) になる。
 * だからここでは機械のメソッドを**全部 await で呼ぶ** — 同期の機械
 * (ELKS/FreeDOS) は値を await してもそのまま解決するので、どちらが
 * 相手でも同じコードで済む。await の間に窓が閉じられることがあるので、
 * 返事を受けたら `open` を見てから描く。
 *
 * **wasmのメモリを直接見ない。** wasmの線形メモリは伸びるとJS側の参照が
 * 無効になる (`terminal.js` に同じ注意がある)。渡すのは組み立て済みのJSONと、
 * そのつど取り直したバイト列だけにする。
 */

const CSS = `
  /* **色は本体のトークンから借りる。** デバッガだけ別の色を持つと、
     ライト/ダークを切り替えたときにここだけ取り残される (実際そうなっていた)。
     var() の第2引数は**子ウインドウ用の落としどころ** — 別documentには
     本体の :root が無いので、そこでは暗い既定で立ち上がる
     (開くときに親の実際の値を写すので、通常は第1引数が効く) */
  .rx-dbg {
    --d-bg: var(--card, #101310);
    --d-head: var(--card-head, #151915);
    --d-line: var(--line, #232a23);
    --d-fg: var(--fg, #d7ddd7);
    --d-dim: var(--dim, #7d867d);
    --d-key: var(--link, #7fb2ff);
    --d-hit: var(--amber, #fbbf24);
    --d-ok: var(--green, #4ade80);
    --d-btn: var(--btn, #141814);
    --d-btn-hover: var(--btn-hover, #1a201a);
    --d-field: var(--field, #0d100d);
    --d-radius: var(--radius, .6rem);
    background: var(--d-bg); color: var(--d-fg);
    font: 13px/1.5 ui-monospace, Menlo, Consolas, monospace;
    /* 縦に伸ばす。**下に地の色が覗くのを防ぐ** — 子ウインドウでは
       パネルが窓より短いと、下半分が地のままになる */
    display: flex; flex-direction: column; min-height: 100vh;
  }
  /* 余った高さを食う欄。いちばん下に置いて、メモリダンプで埋める */
  .rx-dbg .grow { flex: 1 1 auto; display: flex; flex-direction: column;
                  min-height: 0; border-bottom: none; }
  .rx-dbg .grow pre { flex: 1 1 auto; overflow: auto; min-height: 0; }
  .rx-dbg .note { color: var(--d-dim); font-size: 11.5px; margin: 0 0 8px; }
  .rx-dbg h2 { margin: 0 0 8px; font-size: 12px; color: var(--d-dim); font-weight: 600;
               letter-spacing: .04em; }
  .rx-dbg header { position: sticky; top: 0; background: var(--d-head); padding: 10px 12px 6px;
                   border-bottom: 1px solid var(--d-line); z-index: 1; }
  /* ヘッダを掴んで動かせる。ボタンや入力の上では掴ませない */
  .rx-dbg.panel header { cursor: move; }
  .rx-dbg.panel header button, .rx-dbg.panel header input { cursor: pointer; }
  .rx-dbg.dragging { user-select: none; }
  .rx-dbg .row { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; }
  /* ボタンは本体の op ボタンと同じ顔 — アイコン + ラベルを横に並べる */
  .rx-dbg button { background: var(--d-btn); color: var(--d-fg); border: 1px solid var(--d-line);
                   border-radius: var(--d-radius); padding: .35rem .6rem; font: inherit;
                   cursor: pointer; display: inline-flex; align-items: center; gap: .35rem; }
  .rx-dbg button:hover { background: var(--d-btn-hover); border-color: var(--line-lit, #2f3a2f); }
  .rx-dbg button svg { width: 1rem; height: 1rem; flex: none; }
  .rx-dbg input { background: var(--d-field); color: var(--d-fg); border: 1px solid var(--d-line);
                  border-radius: var(--d-radius); padding: .3rem .45rem; font: inherit; width: 8em; }
  /* **プレースホルダが切れない幅**。"0x7c00 or 07c0:0000" が入る */
  .rx-dbg input.wide { width: 13em; }
  .rx-dbg section { padding: 10px 12px; border-bottom: 1px solid var(--d-line); }
  .rx-dbg .state { font-weight: 600; }
  .rx-dbg .state.stopped { color: var(--d-hit); }
  .rx-dbg .state.running { color: var(--d-ok); }
  .rx-dbg table { border-collapse: collapse; }
  .rx-dbg td { padding: 1px 10px 1px 0; white-space: pre; }
  .rx-dbg .k { color: var(--d-dim); }
  .rx-dbg .v { color: var(--d-key); }
  .rx-dbg .changed { color: var(--d-hit); }
  .rx-dbg .hex { color: var(--d-dim); font-size: 12px; }
  .rx-dbg .asm { color: var(--d-ok); }
  /* **必ず1行。** 折り返すと下の欄が丸ごとずれる */
  .rx-dbg .why { margin: 6px 0 0; color: var(--d-hit); height: 1.4em;
                 white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .rx-dbg .hint { color: var(--d-dim); margin: 6px 0 0; font-size: 12px; }
  .rx-dbg pre { margin: 0; white-space: pre; overflow-x: auto; }
  .rx-dbg .list { color: var(--d-dim); margin: 6px 0 0; }
  .rx-dbg code { color: var(--d-key); }

  /* ページ内に落ちたときだけ効く。画面の右に浮かせる */
  .rx-dbg.panel { position: fixed; top: 12px; right: 12px; width: 480px;
                  /* 子ウインドウ用の min-height:100vh を打ち消す。
                     残すと max-height と競合して窓からはみ出す */
                  min-height: 0; height: calc(100vh - 24px); overflow: auto;
                  z-index: 9999; border: 1px solid var(--d-line); border-radius: 8px;
                  box-shadow: 0 8px 32px rgba(0,0,0,.5); }
  /* 最小化: ヘッダ (タイトル+状態+操作ボタン) だけ残して畳む。
     裏の画面を覗きたいとき用。操作ボタンは残るので畳んだまま Step もできる */
  /* インラインで焼いた height (縦リサイズの結果) より勝たせる。
     でないと畳んでも大きな空パネルが残る */
  .rx-dbg.panel.min { height: auto !important; overflow: hidden; }
  .rx-dbg.panel.min section, .rx-dbg.panel.min .why { display: none; }
  /* リサイズつまみ。パネルは右上に固定なので、左端=横幅・下端=高さ・
     左下角=両方 を変える */
  .rx-dbg .resize { position: absolute; z-index: 2; }
  .rx-dbg .resize.x { left: -3px; top: 0; width: 8px; height: 100%; cursor: ew-resize; }
  .rx-dbg .resize.y { left: 0; bottom: -3px; width: 100%; height: 8px; cursor: ns-resize; }
  .rx-dbg .resize.xy { left: -3px; bottom: -3px; width: 14px; height: 14px;
                       cursor: nesw-resize; }
  .rx-dbg .resize:hover { background: color-mix(in srgb, var(--d-ok) 30%, transparent); }
  /* スクロールバーをデバッガの地の色に合わせる。既定の明るいバーが
     暗いパネルから浮くので */
  .rx-dbg, .rx-dbg pre, .rx-dbg .grow pre {
    scrollbar-width: thin; scrollbar-color: var(--d-dim) transparent;
  }
  .rx-dbg ::-webkit-scrollbar, .rx-dbg::-webkit-scrollbar { width: 11px; height: 11px; }
  .rx-dbg ::-webkit-scrollbar-track, .rx-dbg::-webkit-scrollbar-track { background: transparent; }
  .rx-dbg ::-webkit-scrollbar-thumb, .rx-dbg::-webkit-scrollbar-thumb {
    background: var(--d-dim); border-radius: 5px; border: 2px solid var(--d-bg);
  }
  .rx-dbg ::-webkit-scrollbar-thumb:hover, .rx-dbg::-webkit-scrollbar-thumb:hover {
    background: var(--d-fg);
  }
  .rx-dbg .hbtn { padding: .2rem .45rem; margin-left: 4px; }
  .rx-dbg .close { margin-left: auto; }
`;

/** 本体と同じ描き方のアイコン (24x24・線・currentColor)。**絵はここ1箇所** */
const ICON = (d, extra = '') =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"` +
  ` stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}${extra}</svg>`;
const I = {
  play: ICON('<path d="M7 4.5v15l12-7.5z"/>'),
  pause: ICON('<path d="M9 5v14M15 5v14"/>'),
  step: ICON('<path d="M5 5l9 7-9 7z"/><path d="M19 5v14"/>'),
  restart: ICON('<path d="M3 12a9 9 0 1 0 2.6-6.4"/><path d="M3 4v5h5"/>'),
  close: ICON('<path d="M6 6l12 12M18 6L6 18"/>'),
  min: ICON('<path d="M6 12h12"/>'),
};


const HTML = `
  <header>
    <!-- 状態はタイトル行に置く。ボタンの列に混ぜると、ボタンが増えたときに
         折り返して**状態が下の行へ落ちる** (Restart を足して実際にそうなった) -->
    <div class="row">
      <h2 style="margin:0">rustx86 debugger</h2>
      <span class="state" id="rxState">—</span>
      <button class="hbtn" id="rxMin" title="最小化 (裏の画面を覗く)">${I.min}</button>
      <button class="close" id="rxClose" hidden>${I.close}Close</button>
    </div>
    <div class="row" style="margin-top:8px">
      <button id="rxCont">${I.play}Continue</button>
      <button id="rxPause">${I.pause}Pause</button>
      <button id="rxStep">${I.step}Step 1</button>
      <button id="rxRestart" hidden>${I.restart}Restart</button>
    </div>
    <p class="why" id="rxWhy"></p>
  </header>

  <section>
    <h2>Registers</h2>
    <p class="note">オレンジは前回から変わったところ。
      <code>executed</code> は<strong>本当に実行した命令数</strong>、
      <code>steps</code> は機械を進めた回数で、
      <strong>HLTの間も装置を動かすために進む</strong>。
      2つの差がそのまま暇にしていた時間で、<code>goto</code> の座標は
      <code>steps</code> のほう。</p>
    <table id="rxRegs"></table>
  </section>

  <section>
    <h2>Mode</h2>
    <p class="note">リアルモードか保護モードか。保護モードでは
      <strong>セレクタの裏の隠しレジスタ</strong> (base と Dビット) も出す。
      保護モードで死ぬときの手掛かりは大抵ここにある。</p>
    <pre id="rxMode"></pre>
  </section>

  <section>
    <h2>Next instruction</h2>
    <p class="note">CS:IP と、これから実行する命令 (逆アセンブル) と生バイト。幅はCSのDビットで16/32を切り替える。</p>
    <pre id="rxHere"></pre>
  </section>

  <section>
    <h2>Memory</h2>
    <p class="note">既定は <code>0x400</code> から。BIOSデータエリアの256バイトで、
      キー待ち行列・修飾キー・カーソル・ビデオモード・CRTCのポート番号が
      ここに並んでいる。<strong>リアルモードでいちばん情報の詰まった1ページ</strong>。
      番地は線形 — ページング有効 (Linux) ならページ表を通る。カーネルは
      <code>0xc0000000</code> から上に住む。未マップは <code>ff</code> で見える。</p>
    <div class="row">
      <input id="rxMa" value="0x400">
      <input id="rxMl" value="256" style="width:5em">
      <label class="note" style="margin:0"><input type="checkbox" id="rxLive"
        checked style="width:auto"> live</label>
    </div>
    <pre id="rxMem" style="margin-top:6px; max-height:16em; overflow:auto"></pre>
  </section>

  <section>
    <h2>Watchpoints</h2>
    <p class="note">機械を止めて、<strong>どの命令がやったか</strong>まで言う。</p>
    <div class="row">
      <input id="rxBp" class="wide" placeholder="0x7c00 or 07c0:0000">
      <button id="rxAddBp">Break on execute</button>
    </div>
    <div class="row" style="margin-top:6px">
      <input id="rxWp" placeholder="0x450">
      <button id="rxAddWp">Break on write</button>
      <input id="rxIo" placeholder="0x3d5">
      <button id="rxAddIo">Break on I/O</button>
      <button id="rxClr">Clear all</button>
    </div>
    <p class="list" id="rxWatches"></p>
    <p class="note">見どころ: <code>0x450</code> カーソル位置 /
      <code>0x417</code> 修飾キー / <code>0x41a</code> キー待ち行列 /
      ポート <code>0x3d5</code> CRTC (ハードウェアスクロール)</p>
  </section>

  <section>
    <h2>Execution history</h2>
    <p class="note">実際に実行した直近N命令。<strong>倒れた場所は犯行現場ではない</strong>
      ときに使う。費用がかかるので既定では残さない。
      <strong>HLTの間は伸びない</strong>ので、命令数と最後の1行が離れていたら
      その間ずっとアイドルしていたということ。</p>
    <div class="row">
      <button id="rxRec">Start recording</button>
      <button id="rxShowT">Show</button>
    </div>
    <pre id="rxTrace" style="margin-top:6px; max-height:11em; overflow:auto"></pre>
  </section>

`;

/**
 * 1フレームぶんずつ走らせる小さな実行ループ。
 *
 * **デバッガを載せたいページが、実行ループを自前で書かなくて済むように**
 * ここに置く。`main.js` の Machine は自前の (画面描画と絡んだ) ループを
 * 既に持っているので使わない。ベンチのように「計測のときは一息に走らせ、
 * デバッグのときだけ刻む」ページ向けである。
 *
 * 刻むこと自体は計測を歪めるので、**計測経路では使わない**。
 */
export class SlicedRunner {
  constructor(emu, { chunk = 200_000, perFrame = 2_000_000, onStop } = {}) {
    this.emu = emu;
    this.chunk = chunk;
    this.perFrame = perFrame;
    this.onStop = onStop;
    this.running = false;
  }

  get paused() {
    return !this.running;
  }

  start() {
    if (this.running) return;
    this.running = true;
    this.#schedule();
  }

  stop() {
    this.running = false;
  }

  // タブが裏に回ると requestAnimationFrame は発火しない。
  // machine.js と同じ理由でタイマに落とす
  #schedule() {
    if (document.hidden) setTimeout(() => this.#frame(), 16);
    else requestAnimationFrame(() => this.#frame());
  }

  #frame() {
    if (!this.running) return;
    for (let done = 0; done < this.perFrame; done += this.chunk) {
      this.emu.run_slice(this.chunk);
      if (this.emu.is_stopped()) {
        this.running = false;
        this.onStop?.(this.emu.take_stop());
        return;
      }
      if (this.emu.halted()) break;
    }
    this.#schedule();
  }
}

const R16 = ['AX', 'CX', 'DX', 'BX', 'SP', 'BP', 'SI', 'DI'];
const SR = ['ES', 'CS', 'SS', 'DS', 'FS', 'GS'];

/** `0x7c00` も `07c0:0000` も受ける。実機の資料は後者で書かれている */
export function parseAddr(s) {
  if (!s) return null;
  s = String(s).trim();
  const m = s.match(/^(?:0x)?([0-9a-f]{1,4}):(?:0x)?([0-9a-f]{1,4})$/i);
  if (m) return (parseInt(m[1], 16) << 4) + parseInt(m[2], 16);
  const v = parseInt(s.replace(/^0x/i, ''), 16);
  return Number.isNaN(v) ? null : v;
}

export class Debugger {
  /**
   * @param {object} host 親が渡す取っ手。
   *   emu()       いまのEmulator (再起動で入れ替わるので**毎回聞く**)
   *   isPaused()  走っているか
   *   setPaused(v)
   */
  constructor(host) {
    this.host = host;
    this.root = null; // 中身の入れ物 (.rx-dbg)
    this.doc = null;
    this.win = null; // 子ウインドウのとき
    this.prev = {};
    this.timer = null;
    this.lastWhy = '';
  }

  get open() {
    return !!this.root && (!this.win || !this.win.closed);
  }

  /** 開く。既に開いていれば前に出す */
  show() {
    if (this.open) {
      this.win?.focus();
      return;
    }
    // まず子ウインドウを試し、塞がれたらページ内へ落ちる
    const w = window.open('', 'rustx86-dbg', 'width=520,height=880');
    if (w && !w.closed) {
      this.win = w;
      this.doc = w.document;
      w.document.title = 'rustx86 デバッガ';
      w.document.head.innerHTML = '<meta charset="utf-8">';
      // **色は親から写す。** 子ウインドウには本体の :root が無いので、
      // そのままだと var() の落としどころ (暗い既定) で固まり、
      // 本体をライトにしてもここだけ暗いままになる
      this.mirrorTheme(w.document);
      this.root = this.mount(w.document, w.document.body);
      w.document.body.style.margin = '0';
      w.addEventListener('unload', () => this.hide());
    } else {
      this.win = null;
      this.doc = document;
      this.root = this.mount(document, document.body);
      this.root.classList.add('panel');
      this.$('rxClose').hidden = false;
      this.$('rxClose').onclick = () => this.hide();
      this.setupPanelControls();
    }
    // 開いている間は命令数を数えさせる。見張るものが無いと元締めが切れて
    // **命令数が0のまま**になり、何も壊れていないのに壊れて見える
    this.host.emu()?.set_counting(true);
    // 走っている間も見えるように、人間が読める速さで更新する。
    // 毎フレームは要らない — レジスタは1秒に数回読めれば足りる
    this.timer = setInterval(() => {
      // 本体のテーマ切替に追随する (子ウインドウは親の :root を見られない)
      if (this.win) this.mirrorTheme(this.win.document);
      this.render();
    }, 100);
    this.render();
  }

  hide() {
    this.host.emu()?.set_counting(false);
    clearInterval(this.timer);
    this.timer = null;
    this.root?.remove();
    this.root = null;
    if (this.win && !this.win.closed) this.win.close();
    this.win = null;
  }

  /** ページ内パネルだけの操作: 最小化 と 横幅リサイズ */
  setupPanelControls() {
    // 最小化: ヘッダだけ残して畳む。裏の画面を覗きたいとき。
    // 操作ボタンはヘッダに残るので、畳んだまま Step も打てる
    // ヘッダのドラッグで移動。右固定 (right:12px) のままだと動かせないので、
    // ドラッグを始めた瞬間に left/top 固定へ切り替える
    const header = this.root.querySelector('header');
    header.addEventListener('mousedown', (e) => {
      // ボタン・入力の上なら掴まない (最小化やStepを潰さない)
      if (e.target.closest('button, input, select')) return;
      e.preventDefault();
      const r = this.root.getBoundingClientRect();
      this.root.style.left = r.left + 'px';
      this.root.style.top = r.top + 'px';
      this.root.style.right = 'auto';
      this.root.classList.add('dragging');
      const dx = e.clientX - r.left;
      const dy = e.clientY - r.top;
      const move = (ev) => {
        // 画面からはみ出さないよう軽く留める (掴んだ帯は残す)
        const x = Math.max(-r.width + 60, Math.min(window.innerWidth - 60, ev.clientX - dx));
        const y = Math.max(0, Math.min(window.innerHeight - 30, ev.clientY - dy));
        this.root.style.left = x + 'px';
        this.root.style.top = y + 'px';
      };
      const up = () => {
        this.root.classList.remove('dragging');
        this.doc.removeEventListener('mousemove', move);
        this.doc.removeEventListener('mouseup', up);
      };
      this.doc.addEventListener('mousemove', move);
      this.doc.addEventListener('mouseup', up);
    });

    const min = this.$('rxMin');
    min.onclick = () => {
      const on = this.root.classList.toggle('min');
      min.textContent = on ? '□' : '–';
      min.title = on ? '元に戻す' : '最小化 (裏の画面を覗く)';
    };
    // つまみ3つ。パネルは右上固定なので:
    //   左端 (x)  = 掴んで左へ引くと横幅が広がる
    //   下端 (y)  = 掴んで下へ引くと高さが伸びる
    //   左下角(xy) = 両方いっぺんに
    for (const axis of ['x', 'y', 'xy']) {
      const grip = this.doc.createElement('div');
      grip.className = 'resize ' + axis;
      this.root.appendChild(grip);
      grip.addEventListener('mousedown', (e) => this.startResize(e, axis));
    }
  }

  /** リサイズのドラッグ。axis は 'x' / 'y' / 'xy' */
  startResize(e, axis) {
    e.preventDefault();
    const startX = e.clientX;
    const startY = e.clientY;
    const r = this.root.getBoundingClientRect();
    const startW = r.width;
    const startH = r.height;
    // 高さを手で決めたら calc(100vh) の自動追従はやめる (固定モードへ)
    const move = (ev) => {
      if (axis !== 'y') {
        // 左へ動かす (clientXが減る) と広がる
        this.root.style.width = Math.max(300, Math.min(900, startW + (startX - ev.clientX))) + 'px';
      }
      if (axis !== 'x') {
        // 下へ動かす (clientYが増える) と伸びる
        this.root.style.height =
          Math.max(120, Math.min(window.innerHeight - 24, startH + (ev.clientY - startY))) + 'px';
      }
    };
    const up = () => {
      this.doc.removeEventListener('mousemove', move);
      this.doc.removeEventListener('mouseup', up);
    };
    this.doc.addEventListener('mousemove', move);
    this.doc.addEventListener('mouseup', up);
  }

  /**
   * 親のテーマを子ウインドウへ写す。
   *
   * **色の原本は index.html の :root 1箇所**にしておきたい。子documentに
   * パレットを書き写すと二重管理になるので、**今の計算値をそのまま**
   * 子の :root へ流し込む。テーマを切り替えたら次の更新で追随する
   * (呼び直すだけで済むのが、この方式の利点)
   */
  mirrorTheme(doc) {
    const cs = getComputedStyle(document.documentElement);
    const names = [
      '--bg', '--card', '--card-head', '--line', '--line-lit', '--fg', '--dim',
      '--green', '--amber', '--red', '--sink', '--hover', '--btn', '--btn-hover',
      '--field', '--link', '--radius',
    ];
    const body = names
      .map((n) => `${n}:${cs.getPropertyValue(n).trim()}`)
      .filter((d) => !d.endsWith(':'))
      .join(';');
    if (!body) return;
    let st = doc.getElementById('rxTheme');
    if (!st) {
      st = doc.createElement('style');
      st.id = 'rxTheme';
      (doc.head || doc.body).appendChild(st);
    }
    // 子の地の色も本体に合わせる (窓の余白が白いままだと浮く)
    st.textContent = `:root{${body}} body{background:var(--bg);margin:0}`;
  }

  /** 中身を組み立てる。**子ウインドウかページ内かをここは知らない** */
  mount(doc, parent) {
    const st = doc.createElement('style');
    st.textContent = CSS;
    (doc.head || parent).appendChild(st);
    const root = doc.createElement('div');
    root.className = 'rx-dbg';
    root.innerHTML = HTML;
    parent.appendChild(root);
    this.doc = doc;
    this.root = root;
    this.bind();
    return root;
  }

  $(id) {
    return this.root.querySelector('#' + id);
  }

  bind() {
    const emu = () => this.host.emu();

    this.$('rxPause').onclick = () => {
      this.host.setPaused(!this.host.isPaused());
      this.render();
    };
    this.$('rxStep').onclick = async () => {
      this.host.setPaused(true);
      await emu()?.step_one();
      this.render();
    };
    this.$('rxCont').onclick = async () => {
      // **止まった理由を取り去らないと、また同じ所で止まる**
      await emu()?.take_stop();
      this.lastWhy = '';
      this.host.setPaused(false);
      this.render();
    };

    // **終わった機械を作り直す。** ベンチのワークロードは hlt で終わるので、
    // 一度流し切ると死体を眺めることになる。作り直す道が要る
    this.$('rxRestart').onclick = async () => {
      await this.host.restart?.();
      this.reset();
    };

    const add = (id, field, fn) => {
      this.$(id).onclick = async () => {
        const a = parseAddr(this.$(field).value);
        if (a !== null && emu()) await fn(emu(), a);
        this.render();
      };
    };
    add('rxAddBp', 'rxBp', (e, a) => e.set_break(a >>> 0));
    add('rxAddWp', 'rxWp', (e, a) => e.watch_mem(a >>> 0));
    add('rxAddIo', 'rxIo', (e, a) => e.watch_io(a & 0xffff, true, true));
    this.$('rxClr').onclick = async () => {
      await emu()?.clear_debug();
      this.render();
    };

    // メモリは押さずに見えるようにする。**下に空白を残さない**ためでもあり、
    // 「見張っていないが目は離したくない」番地を眺めるのに向く
    this.$('rxMa').oninput = () => this.dump();
    this.$('rxMl').oninput = () => this.dump();
    this.$('rxRec').onclick = () => {
      emu()?.record_trace(256);
      this.$('rxTrace').textContent =
        '直近256命令を残し始めた。残るのは実行した命令だけ — アイドル (HLT) 中は' +
        '1命令も実行していないので増えない。キーを打つなどして働かせてから Show';
    };
    this.$('rxShowT').onclick = () => this.showTrace();
  }

  async render() {
    if (!this.open || this.rendering) return;
    this.rendering = true;
    try {
      await this.#render();
    } finally {
      this.rendering = false;
    }
  }

  async #render() {
    const emu = this.host.emu();
    if (!emu) {
      this.$('rxState').textContent = 'no machine';
      this.$('rxState').className = 'state stopped';
      // **残っている値を消す。** 古い機械の値を今の値として見せない
      this.clearView();
      this.$('rxHere').textContent = '覗く機械がまだ無い';
      this.prev = {};
      return;
    }
    // ワーカー越しなら1つずつ往復させず、まとめて頼んで揃うのを待つ
    const [cj, stoppedNow, wj] = await Promise.all([
      emu.cpu_json(),
      emu.is_stopped(),
      emu.watches_json(),
    ]);
    if (!this.open) return; // 待っている間に窓が閉じられた
    if (!cj) return; // 機械が畳まれた (次の render が no machine を出す)
    const c = JSON.parse(cj);
    const stopped = stoppedNow || !!this.lastWhy;
    const paused = this.host.isPaused();

    const st = this.$('rxState');
    // **HLT を隠さない。** ワークロードが終わって止まっているだけなのに
    // 「paused」と出ると、レジスタが動かないのがバグに見える。
    // ベンチは hlt で終わるので、必ずここへ来る
    const trapped = !!c.trap;
    st.textContent = trapped ? 'trapped' : c.halted ? 'halted' : stopped ? 'stopped' : paused ? 'paused' : 'running';
    st.className = 'state ' + (trapped || c.halted || stopped || paused ? 'stopped' : 'running');
    // 未実装で止まったら理由を出す。**機械は生きている**ので、この状態で
    // レジスタもスタックもメモリも覗ける (それがこの停止の値打ち)
    if (trapped) this.setWhy('未実装: ' + c.trap);
    this.$('rxPause').textContent = paused ? 'Resume' : 'Pause';
    this.$('rxRestart').hidden = !this.host.restart;
    if (!stopped) this.setWhy('');

    // レジスタ。**前回と変わったところに色を付ける** — 1命令進めたときに
    // どれが動いたかが目で分かる
    const rows = [];
    const cell = (k, hex) => {
      const ch = this.prev[k] !== undefined && this.prev[k] !== hex;
      this.prev[k] = hex;
      return `<td class="k">${k}</td><td class="${ch ? 'changed' : 'v'}">${hex}</td>`;
    };
    for (let i = 0; i < 4; i++) {
      rows.push(
        '<tr>' +
          cell('E' + R16[i], hex32(c.regs[i])) +
          cell('E' + R16[i + 4], hex32(c.regs[i + 4])) +
          '</tr>',
      );
    }
    rows.push('<tr><td colspan="4" style="height:6px"></td></tr>');
    for (let i = 0; i < 3; i++) {
      rows.push(
        '<tr>' +
          cell(SR[i * 2], hex16(c.sregs[i * 2])) +
          cell(SR[i * 2 + 1], hex16(c.sregs[i * 2 + 1])) +
          '</tr>',
      );
    }
    rows.push(
      `<tr>${cell('IP', hex16(c.ip))}${cell('FL', hex16(c.flags))}</tr>`,
      `<tr><td class="k">flags</td><td class="v" colspan="3">${c.flagNames || '—'}</td></tr>`,
      `<tr><td class="k">executed</td><td class="v" colspan="3">${fmtInstr(c.executed)}</td></tr>`,
      // 差 (HLTで空回りした回数) は**書かない**。2行を見比べれば分かる導出値で、
      // 同じことを3回言うことになるうえ、行が伸びて折り返す
      `<tr><td class="k">steps</td><td class="v" colspan="3">${fmtInstr(c.instr)}</td></tr>`,
    );
    this.$('rxRegs').innerHTML = rows.join('');

    // モード。保護モードに入ったら CR0/GDTR と隠しレジスタが読める
    const segFmt = (n, s) =>
      `${n}=${hex16(s.sel)}→base=${hex32(s.base)} ${s.big ? '32' : '16'}bit`;
    const machineLine = `<span class="k">${esc(c.machine)}</span> · RAM ${c.ramMb}MB<br>`;
    this.$('rxMode').innerHTML = machineLine + (c.pe
      ? `<span class="changed">protected</span> (CPL${c.cpl}) CR0=${hex32(c.cr0)} ` +
        `paging=${c.pg ? '<span class="changed">on</span>' : 'off'} CR3=${hex32(c.cr3)}<br>` +
        `GDTR=${hex32(c.gdtrBase)}+${hex16(c.gdtrLimit)} ` +
        `IDTR=${hex32(c.idtrBase)}+${hex16(c.idtrLimit)} CR2=${hex32(c.cr2)}<br>` +
        `${segFmt('CS', c.cs)}  ${segFmt('DS', c.ds)}  ${segFmt('SS', c.ss)}`
      : `real  CR0=${hex32(c.cr0)}`);

    this.$('rxHere').innerHTML =
      `<span class="v">${hex16(c.sregs[1])}:${hex16(c.ip)}</span>  ` +
      `<span class="asm">${esc(c.asm)}</span>${c.halted ? '  [HLT]' : ''}<br>` +
      `<span class="hex">${c.bytes}</span>`;

    const w = JSON.parse(wj);
    const f = (a, n) => (a.length ? `${n}: ` + a.map((v) => '0x' + v.toString(16)).join(' ') : '');
    this.$('rxWatches').textContent =
      [f(w.code, 'execute'), f(w.mem, 'write'), f(w.ioR, 'I/O read'), f(w.ioW, 'I/O write')]
        .filter(Boolean)
        .join('  /  ') || 'nothing watched';

    // メモリは最後に置いた欄を埋めるので、走っている間も追いかける
    if (this.$('rxLive').checked) await this.dump(emu);
  }

  /**
   * 見ている機械が入れ替わったときに呼ぶ。
   *
   * **前の機械の残りかすを持ち越さない。** レジスタの「変わった」判定は
   * 前回の値との比較なので、持ち越すと切り替えた直後に全部オレンジになる。
   * 止まった理由も、もう存在しない機械の話になる。
   *
   * 見張り (ブレークポイント) は**新しい機械には付いていない** — Emulator ごと
   * 作り直されるため。画面の一覧もそれに合わせて空になる。
   */
  reset() {
    this.prev = {};
    this.lastWhy = '';
    if (!this.open) return;
    this.clearView();
    // 新しい機械にも数えさせる。開いている間は命令数が動くべきである
    this.host.emu()?.set_counting(true);
    this.render();
  }

  /**
   * 表示を空にする。
   *
   * **古い値を残したまま「機械が無い」と言ってはいけない。** レジスタ欄に
   * 前の機械の値が残っていると、読む側にはそれが今の値に見える。
   * ベンチへ切り替えたときに実際にそう見えた (ELKSのCS:IPが居座った)。
   */
  clearView() {
    for (const id of ['rxRegs', 'rxHere', 'rxMem', 'rxTrace']) {
      this.$(id).textContent = '';
    }
    this.setWhy('');
  }

  /**
   * 止まった理由を出す。**ここに出るのは止まった理由だけ**である。
   *
   * 状態の説明 (HLTなど) は書かない — 状態語で分かるものを長い文で
   * 繰り返すと、折り返して下の欄がずれる。実際に2行になって崩れた。
   * 収まらない場合も切り詰めて1行に保ち、全文は title で読ませる
   */
  setWhy(text) {
    const el = this.$('rxWhy');
    el.textContent = text;
    el.title = text;
  }

  /** 親が「止まった」と気づいたときに呼ぶ。理由の文字列はここで預かる */
  onStop(why) {
    this.lastWhy = why;
    if (this.open) {
      this.setWhy('-> ' + why);
      this.render();
    }
  }

  async dump(emu = this.host.emu()) {
    const a = parseAddr(this.$('rxMa').value);
    const len = Math.min(parseInt(this.$('rxMl').value, 10) || 256, 4096);
    if (a === null || !emu) return;
    // **毎回取り直す。** wasmのメモリが伸びると前の参照は無効になる
    const b = await emu.read_mem(a >>> 0, len);
    if (!b || !this.open) return;
    const out = [];
    for (let r = 0; r * 16 < len; r++) {
      const base = a + r * 16;
      let hex = '';
      let txt = '';
      for (let i = 0; i < 16; i++) {
        const v = b[r * 16 + i] ?? 0;
        hex += v.toString(16).padStart(2, '0') + ' ';
        txt += v >= 0x20 && v < 0x7f ? String.fromCharCode(v) : '.';
      }
      out.push(`${base.toString(16).padStart(7, '0')}  ${hex} |${txt}|`);
    }
    this.$('rxMem').textContent = out.join('\n');
  }

  async showTrace() {
    const emu = this.host.emu();
    if (!emu) return;
    const tj = await emu.trace_json();
    if (!tj || !this.open) return;
    const t = JSON.parse(tj);
    if (!t.length) {
      this.$('rxTrace').textContent =
        'まだ何も残していない。Start recording のあと、機械に仕事をさせる' +
        ' (アイドル中は1命令も実行しない — キー入力やコマンドで起こす)';
      return;
    }
    this.$('rxTrace').innerHTML = t
      .slice(-40)
      .map((s) => `${String(s.i).padStart(12)}: <span class="v">${hex16(s.cs)}:${hex16(s.ip)}</span>  ` +
                  `<span class="asm">${esc(s.asm)}</span>`)
      .join('<br>');
  }
}

/**
 * 命令数の見せ方。
 *
 * 走らせている間は1フレームぶん (固定命令数) ずつしか進まないので、
 * **下の桁は常にゼロ**で、桁を全部出しても読めないだけである。だから
 * まず大きさを出す。
 *
 * ただし正確な桁を捨ててはいけない。この数は
 * **`goto` で巻き戻すときの座標**であり、`Step 1` では1ずつ動く。
 * 丸めた数を打ち直しても、そこへは戻れない。だから両方出す。
 */
function fmtInstr(n) {
  if (n < 1e6) return n.toLocaleString();
  const [v, u] = n >= 1e9 ? [n / 1e9, 'G'] : [n / 1e6, 'M'];
  return `<strong>${v.toFixed(2)} ${u}</strong>` +
    `<span class="k" style="margin-left:.6em">${n.toLocaleString()}</span>`;
}

// HTMLに出す前に最低限エスケープする (逆アセンブルは信頼できるが念のため)
const esc = (s) => (s || '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const hex16 = (v) => (v >>> 0).toString(16).padStart(4, '0');
const hex32 = (v) => (v >>> 0).toString(16).padStart(8, '0');
