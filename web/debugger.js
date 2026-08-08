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
  .rx-dbg .why { margin: 6px 0 0; color: #f0883e; min-height: 1.4em; }
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
    <div class="row">
      <h2 style="margin:0">rustx86 debugger</h2>
      <button class="close" id="rxClose" hidden>Close</button>
    </div>
    <div class="row" style="margin-top:8px">
      <button id="rxPause">Pause</button>
      <button id="rxStep">Step 1</button>
      <button id="rxCont">Continue</button>
      <span class="state" id="rxState">—</span>
    </div>
    <p class="why" id="rxWhy"></p>
  </header>

  <section>
    <h2>Registers</h2>
    <p class="note">Orange = changed since the last update.</p>
    <table id="rxRegs"></table>
  </section>

  <section>
    <h2>Next instruction</h2>
    <p class="note">CS:IP and the raw bytes about to run. No disassembler yet.</p>
    <pre id="rxHere"></pre>
  </section>

  <section>
    <h2>Watchpoints</h2>
    <p class="note">Stop the machine and report <em>which instruction</em> did it.</p>
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
    <p class="note">Worth watching: <code>0x450</code> cursor position /
      <code>0x417</code> shift flags / <code>0x41a</code> keyboard queue /
      port <code>0x3d5</code> CRTC (hardware scrolling)</p>
  </section>

  <section>
    <h2>Execution history</h2>
    <p class="note">The last N instructions actually executed &mdash; use it when the
      crash site is not the crime scene. Off by default; it costs to record.
      <strong>Nothing is added while the CPU is halted</strong>, so a gap between the
      instruction count and the last entry means the machine was idle.</p>
    <div class="row">
      <button id="rxRec">Start recording</button>
      <button id="rxShowT">Show</button>
    </div>
    <pre id="rxTrace" style="margin-top:6px; max-height:11em; overflow:auto"></pre>
  </section>

  <section class="grow">
    <h2>Memory</h2>
    <p class="note">Starts at <code>0x400</code>, the BIOS Data Area &mdash; 256 bytes
      that hold the keyboard queue, the shift flags, the cursor and the video mode.
      In real mode it is the single most informative page of memory there is.</p>
    <div class="row">
      <input id="rxMa" value="0x400">
      <input id="rxMl" value="256" style="width:5em">
      <label class="note" style="margin:0"><input type="checkbox" id="rxLive"
        checked style="width:auto"> live</label>
    </div>
    <pre id="rxMem" style="margin-top:6px"></pre>
  </section>
`;

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
      this.$('rxTrace').textContent = 'Recording the last 256 instructions…';
    };
    this.$('rxShowT').onclick = () => this.showTrace();
  }

  render() {
    if (!this.open) return;
    const emu = this.host.emu();
    if (!emu) {
      this.$('rxState').textContent = 'no machine';
      return;
    }
    const c = JSON.parse(emu.cpu_json());
    const stopped = emu.is_stopped() || !!this.lastWhy;
    const paused = this.host.isPaused();

    const st = this.$('rxState');
    st.textContent = stopped ? 'stopped' : paused ? 'paused' : 'running';
    st.className = 'state ' + (stopped || paused ? 'stopped' : 'running');
    this.$('rxPause').textContent = paused ? 'Resume' : 'Pause';

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
      `<tr><td class="k">instrs</td><td class="v" colspan="3">${c.instr.toLocaleString()}</td></tr>`,
    );
    this.$('rxRegs').innerHTML = rows.join('');

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

  /** 親が「止まった」と気づいたときに呼ぶ。理由の文字列はここで預かる */
  onStop(why) {
    this.lastWhy = why;
    if (this.open) {
      this.$('rxWhy').textContent = '-> ' + why;
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
      this.$('rxTrace').textContent = 'Nothing recorded. Press "Start recording", then run.';
      return;
    }
    this.$('rxTrace').textContent = t
      .slice(-40)
      .map((s) => `${String(s.i).padStart(12)}: ${hex16(s.cs)}:${hex16(s.ip)}  ${s.b}`)
      .join('\n');
  }
}

const hex16 = (v) => (v >>> 0).toString(16).padStart(4, '0');
const hex32 = (v) => (v >>> 0).toString(16).padStart(8, '0');
