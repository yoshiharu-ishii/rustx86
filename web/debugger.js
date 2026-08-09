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
 * **wasmのメモリを直接見ない。** wasmの線形メモリは伸びるとJS側の参照が
 * 無効になる (`terminal.js` に同じ注意がある)。渡すのは組み立て済みのJSONと、
 * そのつど取り直したバイト列だけにする。
 */

const CSS = `
  .rx-dbg { background: #0b0e14; color: #c9d1d9;
            font: 13px/1.5 ui-monospace, Menlo, Consolas, monospace;
            /* 縦に伸ばす。**下に地の色が覗くのを防ぐ** — 子ウインドウでは
               パネルが窓より短いと、下半分が白いままになる */
            display: flex; flex-direction: column; min-height: 100vh; }
  /* 余った高さを食う欄。いちばん下に置いて、メモリダンプで埋める */
  .rx-dbg .grow { flex: 1 1 auto; display: flex; flex-direction: column;
                  min-height: 0; border-bottom: none; }
  .rx-dbg .grow pre { flex: 1 1 auto; overflow: auto; min-height: 0; }
  .rx-dbg .note { color: #6e7681; font-size: 11.5px; margin: 0 0 8px; }
  .rx-dbg h2 { margin: 0 0 8px; font-size: 12px; color: #8b949e; font-weight: 600;
               letter-spacing: .04em; }
  .rx-dbg header { position: sticky; top: 0; background: #0b0e14; padding: 10px 12px 6px;
                   border-bottom: 1px solid #1f2733; z-index: 1; }
  .rx-dbg .row { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; }
  .rx-dbg button { background: #1f2733; color: #c9d1d9; border: 1px solid #30363d;
                   border-radius: 5px; padding: 4px 10px; font: inherit; cursor: pointer; }
  .rx-dbg button:hover { background: #2b3441; }
  .rx-dbg input { background: #0d1117; color: #c9d1d9; border: 1px solid #30363d;
                  border-radius: 5px; padding: 4px 6px; font: inherit; width: 8em; }
  .rx-dbg section { padding: 10px 12px; border-bottom: 1px solid #1f2733; }
  .rx-dbg .state { font-weight: 600; }
  .rx-dbg .state.stopped { color: #f0883e; }
  .rx-dbg .state.running { color: #3fb950; }
  .rx-dbg table { border-collapse: collapse; }
  .rx-dbg td { padding: 1px 10px 1px 0; white-space: pre; }
  .rx-dbg .k { color: #8b949e; }
  .rx-dbg .v { color: #79c0ff; }
  .rx-dbg .changed { color: #f0883e; }
  .rx-dbg .hex { color: #7ee787; }
  /* **必ず1行。** 折り返すと下の欄が丸ごとずれる */
  .rx-dbg .why { margin: 6px 0 0; color: #f0883e; height: 1.4em;
                 white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .rx-dbg .hint { color: #6e7681; margin: 6px 0 0; font-size: 12px; }
  .rx-dbg pre { margin: 0; white-space: pre; overflow-x: auto; }
  .rx-dbg .list { color: #8b949e; margin: 6px 0 0; }
  .rx-dbg code { color: #79c0ff; }

  /* ページ内に落ちたときだけ効く。画面の右に浮かせる */
  .rx-dbg.panel { position: fixed; top: 12px; right: 12px; width: 480px;
                  /* 子ウインドウ用の min-height:100vh を打ち消す。
                     残すと max-height と競合して窓からはみ出す */
                  min-height: 0; height: calc(100vh - 24px); overflow: auto;
                  z-index: 9999; border: 1px solid #30363d; border-radius: 8px;
                  box-shadow: 0 8px 32px rgba(0,0,0,.5); }
  .rx-dbg .close { margin-left: auto; }
`;

const HTML = `
  <header>
    <!-- 状態はタイトル行に置く。ボタンの列に混ぜると、ボタンが増えたときに
         折り返して**状態が下の行へ落ちる** (Restart を足して実際にそうなった) -->
    <div class="row">
      <h2 style="margin:0">rustx86 debugger</h2>
      <span class="state" id="rxState">—</span>
      <button class="close" id="rxClose" hidden>Close</button>
    </div>
    <div class="row" style="margin-top:8px">
      <button id="rxPause">Pause</button>
      <button id="rxStep">Step 1</button>
      <button id="rxCont">Continue</button>
      <button id="rxRestart" hidden>Restart</button>
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
    <p class="note">CS:IP と、これから実行するバイト列。逆アセンブラはまだ無い。</p>
    <pre id="rxHere"></pre>
  </section>

  <section>
    <h2>Watchpoints</h2>
    <p class="note">機械を止めて、<strong>どの命令がやったか</strong>まで言う。</p>
    <div class="row">
      <input id="rxBp" placeholder="0x7c00 or 07c0:0000">
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

  <section class="grow">
    <h2>Memory</h2>
    <p class="note">既定は <code>0x400</code> から。BIOSデータエリアの256バイトで、
      キー待ち行列・修飾キー・カーソル・ビデオモード・CRTCのポート番号が
      ここに並んでいる。<strong>リアルモードでいちばん情報の詰まった1ページ</strong>。</p>
    <div class="row">
      <input id="rxMa" value="0x400">
      <input id="rxMl" value="256" style="width:5em">
      <label class="note" style="margin:0"><input type="checkbox" id="rxLive"
        checked style="width:auto"> live</label>
    </div>
    <pre id="rxMem" style="margin-top:6px"></pre>
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
    }
    // 開いている間は命令数を数えさせる。見張るものが無いと元締めが切れて
    // **命令数が0のまま**になり、何も壊れていないのに壊れて見える
    this.host.emu()?.set_counting(true);
    // 走っている間も見えるように、人間が読める速さで更新する。
    // 毎フレームは要らない — レジスタは1秒に数回読めれば足りる
    this.timer = setInterval(() => this.render(), 100);
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
    this.$('rxStep').onclick = () => {
      this.host.setPaused(true);
      emu()?.step_one();
      this.render();
    };
    this.$('rxCont').onclick = () => {
      // **止まった理由を取り去らないと、また同じ所で止まる**
      emu()?.take_stop();
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
      this.$(id).onclick = () => {
        const a = parseAddr(this.$(field).value);
        if (a !== null && emu()) fn(emu(), a);
        this.render();
      };
    };
    add('rxAddBp', 'rxBp', (e, a) => e.set_break(a >>> 0));
    add('rxAddWp', 'rxWp', (e, a) => e.watch_mem(a >>> 0));
    add('rxAddIo', 'rxIo', (e, a) => e.watch_io(a & 0xffff, true, true));
    this.$('rxClr').onclick = () => {
      emu()?.clear_debug();
      this.render();
    };

    // メモリは押さずに見えるようにする。**下に空白を残さない**ためでもあり、
    // 「見張っていないが目は離したくない」番地を眺めるのに向く
    this.$('rxMa').oninput = () => this.dump();
    this.$('rxMl').oninput = () => this.dump();
    this.$('rxRec').onclick = () => {
      emu()?.record_trace(256);
      this.$('rxTrace').textContent = '直近256命令を残し始めた';
    };
    this.$('rxShowT').onclick = () => this.showTrace();
  }

  render() {
    if (!this.open) return;
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
    const c = JSON.parse(emu.cpu_json());
    const stopped = emu.is_stopped() || !!this.lastWhy;
    const paused = this.host.isPaused();

    const st = this.$('rxState');
    // **HLT を隠さない。** ワークロードが終わって止まっているだけなのに
    // 「paused」と出ると、レジスタが動かないのがバグに見える。
    // ベンチは hlt で終わるので、必ずここへ来る
    st.textContent = c.halted ? 'halted' : stopped ? 'stopped' : paused ? 'paused' : 'running';
    st.className = 'state ' + (c.halted || stopped || paused ? 'stopped' : 'running');
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
    this.$('rxMode').innerHTML = c.pe
      ? `<span class="changed">protected</span> (CPL${c.cpl}) CR0=${hex32(c.cr0)} ` +
        `paging=${c.pg ? '<span class="changed">on</span>' : 'off'} CR3=${hex32(c.cr3)}<br>` +
        `GDTR=${hex32(c.gdtrBase)}+${hex16(c.gdtrLimit)} ` +
        `IDTR=${hex32(c.idtrBase)}+${hex16(c.idtrLimit)} CR2=${hex32(c.cr2)}<br>` +
        `${segFmt('CS', c.cs)}  ${segFmt('DS', c.ds)}  ${segFmt('SS', c.ss)}`
      : `real  CR0=${hex32(c.cr0)}`;

    this.$('rxHere').innerHTML =
      `<span class="v">${hex16(c.sregs[1])}:${hex16(c.ip)}</span>  ` +
      `<span class="hex">${c.bytes}</span>${c.halted ? '  [HLT]' : ''}`;

    const w = JSON.parse(emu.watches_json());
    const f = (a, n) => (a.length ? `${n}: ` + a.map((v) => '0x' + v.toString(16)).join(' ') : '');
    this.$('rxWatches').textContent =
      [f(w.code, 'execute'), f(w.mem, 'write'), f(w.ioR, 'I/O read'), f(w.ioW, 'I/O write')]
        .filter(Boolean)
        .join('  /  ') || 'nothing watched';

    // メモリは最後に置いた欄を埋めるので、走っている間も追いかける
    if (this.$('rxLive').checked) this.dump();
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

  dump() {
    const emu = this.host.emu();
    const a = parseAddr(this.$('rxMa').value);
    const len = Math.min(parseInt(this.$('rxMl').value, 10) || 256, 4096);
    if (a === null || !emu) return;
    // **毎回取り直す。** wasmのメモリが伸びると前の参照は無効になる
    const b = emu.read_mem(a >>> 0, len);
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

  showTrace() {
    const emu = this.host.emu();
    if (!emu) return;
    const t = JSON.parse(emu.trace_json());
    if (!t.length) {
      this.$('rxTrace').textContent = 'まだ何も残していない。Start recording を押してから走らせる';
      return;
    }
    this.$('rxTrace').textContent = t
      .slice(-40)
      .map((s) => `${String(s.i).padStart(12)}: ${hex16(s.cs)}:${hex16(s.ip)}  ${s.b}`)
      .join('\n');
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

const hex16 = (v) => (v >>> 0).toString(16).padStart(4, '0');
const hex32 = (v) => (v >>> 0).toString(16).padStart(8, '0');
