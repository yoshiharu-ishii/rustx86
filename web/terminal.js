// canvasで動く端末。
//
// **エミュレータのことは何も知らない。** 受け取るのは「今の画面 (文字と属性)」と
// 「カーソル位置」だけで、返すのは「押されたキー」と「選択された文字列」である。
// 中身がELKSでもDOSでも、あるいはただのテキストでも同じように動く。
// `machine.js` (機械) や `main.js` (繋ぎ役) から切り離してあるのはそのため。
//
// 持っている機能:
//   - VGAテキスト (80x25、文字+属性) の描画
//   - スクロールバック (ホイール / スクロールバーのドラッグ)
//   - カーソル (点滅)
//   - マウスドラッグでの選択と、Cmd/Ctrl+C でのコピー
//   - 物理キー (KeyboardEvent.code) の通知

/** macOS Terminal.app の Homebrew テーマ。黒地に緑のリン光管 */
const HOMEBREW = {
  bg: '#000000',
  fg: '#00ff00',
  dim: '#00aa00',
  bright: '#7cff7c',
  cursor: '#23ff18',
  selection: 'rgba(0, 255, 0, 0.30)',
  scrollTrack: 'rgba(0, 255, 0, 0.10)',
  scrollThumb: 'rgba(0, 255, 0, 0.45)',
  banner: 'rgba(0, 90, 0, 0.85)',
};

/**
 * VGAの16色を Homebrew に寄せた対応表。
 *
 * 既定色 (7) と明るい白 (15) を緑にするのが肝で、これで普段の文字が緑になる。
 * 色を捨てて全部緑にしてしまうと、OSが色分けした情報 (エラーの赤など) が
 * 読み取れなくなるので、他の色は残す。
 */
const PALETTE = [
  '#000000', '#0044aa', '#00aa00', '#00aaaa',
  '#aa2200', '#aa00aa', '#aa5500', HOMEBREW.fg,
  '#004400', '#3388ff', '#33ff33', '#33ffff',
  '#ff5555', '#ff55ff', '#ffff55', HOMEBREW.bright,
];

const CELL_W = 9;
const CELL_H = 16;
const SCROLLBAR_W = 10;
/** カーソルの点滅周期 (ミリ秒) */
const BLINK_MS = 530;

export class Terminal {
  /**
   * @param {HTMLCanvasElement} canvas
   * @param {{cols?: number, rows?: number, scrollback?: number}} opts
   */
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
    this.cols = opts.cols ?? 80;
    this.rows = opts.rows ?? 25;
    this.scrollbackLimit = opts.scrollback ?? 1000;

    /** 画面外へ流れた行 (文字列) */
    this.scrollback = [];
    /** 同じ行の属性 (色)。**履歴も色付きで読めるように控える** */
    this.scrollbackAttrs = [];
    /** 今の画面 25行 (文字列)。スクロール検出と選択に使う */
    this.screen = [];
    /**
     * 今の画面の生バイト (文字+属性)。**自前の領域に写しを持つ。**
     *
     * wasmのメモリを直接見る参照を持ち続けてはいけない。wasm側で大きな確保が
     * あるとリニアメモリが伸び、**それまでの参照は無効になる**。
     * 実際、状態の保存 (数MBを確保する) をした瞬間に画面が真っ黒になった。
     * 写すのは4000バイトなので、抱えている危険に比べれば安い。
     */
    this.cells = null;
    /** 前回の生バイト。スクロールの検出に使う */
    this.prevCells = null;

    /** 何行さかのぼって見ているか (0 = 最新) */
    this.offset = 0;
    this.cursor = { row: 0, col: 0, visible: true };
    this.selection = null; // {a:{row,col}, b:{row,col}} — 表示上の座標

    /** キーが押された/離されたときに呼ばれる。(code, down) => boolean */
    this.onKey = null;
    /** 貼り付けられたときに呼ばれる。(text) => void */
    this.onPaste = null;
    /** 文字として打たれたときに呼ばれる (JP配列のとき)。(ch) => void */
    this.onChar = null;

    /**
     * キーボード配列。
     *
     * **スキャンコードはキーの「位置」なので、配列とは無関係である。**
     * ずれるのはその先 — ゲストのOSが位置から文字を決める段階で、
     * ELKSはUS配列の対応表しか持っていない。
     *
     * だからJIS配列の実機で `@` のキー (US配列でいう `[` の位置) を押すと、
     * ELKSは `[` を出す。合わせるには**位置ではなく文字を送る**しかない。
     *
     * - `us`: 位置 (KeyboardEvent.code) をそのまま送る。実機に忠実
     * - `jp`: ブラウザが解釈した文字 (KeyboardEvent.key) を送る。
     *   ゲストのUS配列に合わせて組み立て直すので、見たままが入る
     */
    this.layout = opts.layout ?? 'jp';

    canvas.width = this.cols * CELL_W + SCROLLBAR_W;
    canvas.height = this.rows * CELL_H;
    this.#bindEvents();

    this.blinkOn = true;
    setInterval(() => {
      this.blinkOn = !this.blinkOn;
      if (this.offset === 0) this.draw();
    }, BLINK_MS);
  }

  /** 端末の中身を空にする */
  reset() {
    this.scrollback.length = 0;
    this.scrollbackAttrs.length = 0;
    this.screen = [];
    this.prevCells = null;
    this.offset = 0;
    this.selection = null;
  }

  /**
   * 画面を更新する。`cells` は 文字,属性 が交互に並ぶ生バイト列。
   *
   * ここでスクロールも検出する。上の数行がそっくり動いていれば
   * 流れた行を控える — VRAMには今見えている分しか無いためである。
   */
  /**
   * 画面の状態を取り込む。**描画はしない。**
   *
   * スクロールの検出は細かく回す必要がある (まとめて進めてから見ると、
   * その間に何十行も流れていて追えない)。一方で描画は高い。
   * だから安い方だけを細かく呼べるように分けてある。
   */
  sample(cells, cursorRow, cursorCol) {
    this.cursor.row = cursorRow;
    this.cursor.col = cursorCol;

    const rowBytes = this.cols * 2;
    const size = rowBytes * this.rows;
    if (!this.cells) this.cells = new Uint8Array(size);
    this.cells.set(cells.subarray(0, size));
    if (!this.prevCells) {
      this.prevCells = new Uint8Array(size);
      this.prevCells.set(this.cells);
      return;
    }
    const shift = this.#detectScroll(this.prevCells, this.cells, rowBytes);
    if (shift > 0) {
      // 流れた行は**スクロール前の画面**から取る。
      // 文字列を作るのはここだけなので、毎回作る必要がない
      const before = this.#rowsFrom(this.prevCells);
      const attrs = this.#attrsFrom(this.prevCells);
      for (let i = 0; i < shift; i++) {
        this.scrollback.push(before[i]);
        this.scrollbackAttrs.push(attrs[i]);
      }
      while (this.scrollback.length > this.scrollbackLimit) {
        this.scrollback.shift();
        this.scrollbackAttrs.shift();
      }
      if (this.offset > 0) this.offset = Math.min(this.offset + shift, this.scrollback.length);
    }
    this.prevCells.set(this.cells);
  }

  /** 控えた行 + 今の画面 */
  allLines() {
    return [...this.scrollback, ...this.screen];
  }

  /** 選択されている文字列 (無ければ空) */
  selectedText() {
    if (!this.selection) return '';
    const { a, b } = this.#orderedSelection();
    const lines = this.#visibleLines();
    const out = [];
    for (let r = a.row; r <= b.row; r++) {
      const line = (lines[r] ?? '').padEnd(this.cols, ' ');
      const from = r === a.row ? a.col : 0;
      const to = r === b.row ? b.col : this.cols;
      out.push(line.slice(from, to).replace(/\s+$/, ''));
    }
    return out.join('\n');
  }

  draw() {
    const { ctx } = this;
    if (this.cells) this.screen = this.#rowsFrom(this.cells);
    ctx.fillStyle = HOMEBREW.bg;
    ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.textBaseline = 'top';
    ctx.font = `${CELL_H}px ui-monospace, SFMono-Regular, Menlo, monospace`;

    if (this.offset === 0 && this.cells) {
      this.#drawLive();
    } else {
      this.#drawHistory();
    }
    this.#drawSelection();
    this.#drawScrollbar();
  }

  // ---------- 描画 ----------

  #drawLive() {
    const { ctx, cells } = this;
    for (let row = 0; row < this.rows; row++) {
      for (let col = 0; col < this.cols; col++) {
        const i = (row * this.cols + col) * 2;
        const ch = cells[i];
        const attr = cells[i + 1];
        const x = col * CELL_W;
        const y = row * CELL_H;
        const bg = PALETTE[(attr >> 4) & 7];
        if (bg !== PALETTE[0]) {
          ctx.fillStyle = bg;
          ctx.fillRect(x, y, CELL_W, CELL_H);
        }
        if (ch >= 0x20 && ch < 0x7f) {
          ctx.fillStyle = PALETTE[attr & 0x0f];
          ctx.fillText(String.fromCharCode(ch), x, y);
        }
      }
    }
    if (this.cursor.visible && this.blinkOn) {
      ctx.fillStyle = HOMEBREW.cursor;
      ctx.fillRect(this.cursor.col * CELL_W, this.cursor.row * CELL_H + CELL_H - 2, CELL_W, 2);
    }
  }

  #drawHistory() {
    const { ctx } = this;
    const { lines, attrs } = this.#visibleWithAttrs();
    for (let row = 0; row < this.rows; row++) {
      const line = lines[row];
      if (!line) continue;
      const at = attrs[row];
      for (let col = 0; col < line.length; col++) {
        // 属性を控えていない行 (今の画面ぶん) は既定色で描く
        ctx.fillStyle = at ? PALETTE[at[col] & 0x0f] : HOMEBREW.fg;
        ctx.fillText(line[col], col * CELL_W, row * CELL_H);
      }
    }
    ctx.fillStyle = HOMEBREW.banner;
    ctx.fillRect(0, 0, this.cols * CELL_W, CELL_H);
    ctx.fillStyle = HOMEBREW.bright;
    ctx.fillText(`▲ ${this.offset}行前  (キーを打つと最新へ)`, 4, 0);
  }

  #drawSelection() {
    if (!this.selection) return;
    const { a, b } = this.#orderedSelection();
    this.ctx.fillStyle = HOMEBREW.selection;
    for (let r = a.row; r <= b.row; r++) {
      const from = r === a.row ? a.col : 0;
      const to = r === b.row ? b.col : this.cols;
      this.ctx.fillRect(from * CELL_W, r * CELL_H, (to - from) * CELL_W, CELL_H);
    }
  }

  /** 右端のスクロールバー。**これがあればログを別枠に出す必要が無い** */
  #drawScrollbar() {
    const { ctx } = this;
    const x = this.cols * CELL_W;
    const h = this.canvas.height;
    ctx.fillStyle = HOMEBREW.scrollTrack;
    ctx.fillRect(x, 0, SCROLLBAR_W, h);

    const total = this.scrollback.length + this.rows;
    const ratio = Math.min(1, this.rows / total);
    const thumbH = Math.max(20, h * ratio);
    // offset=0 (最新) が一番下
    const maxOffset = this.scrollback.length;
    const pos = maxOffset === 0 ? 1 : 1 - this.offset / maxOffset;
    const y = (h - thumbH) * pos;
    ctx.fillStyle = HOMEBREW.scrollThumb;
    ctx.fillRect(x + 1, y, SCROLLBAR_W - 2, thumbH);
  }

  // ---------- 内部 ----------

  /** バイト列から属性だけ取り出す (色を保った履歴のため) */
  #attrsFrom(cells) {
    const out = [];
    for (let row = 0; row < this.rows; row++) {
      const a = new Uint8Array(this.cols);
      for (let col = 0; col < this.cols; col++) {
        a[col] = cells[(row * this.cols + col) * 2 + 1];
      }
      out.push(a);
    }
    return out;
  }

  #rowsFrom(cells) {
    const out = [];
    for (let row = 0; row < this.rows; row++) {
      let line = '';
      for (let col = 0; col < this.cols; col++) {
        const ch = cells[(row * this.cols + col) * 2];
        line += ch >= 0x20 && ch < 0x7f ? String.fromCharCode(ch) : ' ';
      }
      out.push(line.replace(/\s+$/, ''));
    }
    return out;
  }

  /**
   * 何行スクロールしたかを返す。
   *
   * 画面全体の一致は見ない。カーソル行は常に書き換わっているので、
   * 全体一致を条件にするとほとんどの瞬間で不成立になる。
   * 上の3行がそろって動いていればスクロールとみなす。
   */
  #detectScroll(prev, now, rowBytes) {
    // **上の数行だけを見る。** 画面全体の一致を条件にすると、スクロールの途中
    // (4000バイトのmemmoveの最中) に覗いたときに一致せず、取りこぼす。
    // 上から順に動くので、先頭が一致していればスクロールしたと判断してよい。
    //
    // ただし**同じ内容の行が並んでいると、どんなずれ幅でも一致してしまう**。
    // vi の画面は `~` だらけなので、これを弾かないと描き直すたびに
    // スクロールとみなされ履歴がゴミで埋まる (vi を開くだけで22行入った)。
    // だから「一致した行が全部同じ中身ではないこと」も要求する。
    const NEED = 3;
    for (let shift = 1; shift <= this.rows - NEED; shift++) {
      let ok = true;
      let content = false;
      let varied = false;
      let first = null;
      for (let r = 0; r < NEED && ok; r++) {
        const a = (r + shift) * rowBytes;
        const b = r * rowBytes;
        let text = '';
        for (let i = 0; i < rowBytes; i += 2) {
          if (prev[a + i] !== now[b + i]) {
            ok = false;
            break;
          }
          const ch = now[b + i];
          text += String.fromCharCode(ch);
          if (ch > 0x20) content = true;
        }
        if (!ok) break;
        if (first === null) first = text;
        else if (text !== first) varied = true;
      }
      if (ok && content && varied) return shift;
    }
    return 0;
  }

  /** 今表示している25行 */
  /** 今表示している25行を、属性付きで返す */
  #visibleWithAttrs() {
    const all = this.allLines();
    const allAttrs = [...this.scrollbackAttrs, ...new Array(this.screen.length).fill(null)];
    const start = Math.max(0, all.length - this.rows - this.offset);
    return {
      lines: all.slice(start, start + this.rows),
      attrs: allAttrs.slice(start, start + this.rows),
    };
  }

  #visibleLines() {
    if (this.offset === 0) return this.screen;
    const all = this.allLines();
    const start = Math.max(0, all.length - this.rows - this.offset);
    return all.slice(start, start + this.rows);
  }

  #orderedSelection() {
    const { a, b } = this.selection;
    const before = a.row < b.row || (a.row === b.row && a.col <= b.col);
    return before ? { a, b } : { a: b, b: a };
  }

  #cellAt(ev) {
    const r = this.canvas.getBoundingClientRect();
    const scale = this.canvas.width / r.width;
    const x = (ev.clientX - r.left) * scale;
    const y = (ev.clientY - r.top) * scale;
    return {
      row: Math.max(0, Math.min(this.rows - 1, Math.floor(y / CELL_H))),
      col: Math.max(0, Math.min(this.cols, Math.round(x / CELL_W))),
      onScrollbar: x >= this.cols * CELL_W,
    };
  }

  scrollTo(offset) {
    const next = Math.max(0, Math.min(this.scrollback.length, offset));
    if (next !== this.offset) {
      this.offset = next;
      this.draw();
    }
  }

  #bindEvents() {
    const c = this.canvas;

    c.addEventListener('wheel', e => {
      e.preventDefault();
      this.scrollTo(this.offset + (e.deltaY > 0 ? -3 : 3));
    }, { passive: false });

    let dragging = null;
    c.addEventListener('mousedown', e => {
      const p = this.#cellAt(e);
      if (p.onScrollbar) {
        dragging = 'scrollbar';
        this.#scrollbarTo(e);
      } else {
        dragging = 'select';
        this.selection = { a: { row: p.row, col: p.col }, b: { row: p.row, col: p.col } };
        this.draw();
      }
      c.focus();
      e.preventDefault();
    });
    window.addEventListener('mousemove', e => {
      if (dragging === 'select') {
        const p = this.#cellAt(e);
        this.selection.b = { row: p.row, col: p.col };
        this.draw();
      } else if (dragging === 'scrollbar') {
        this.#scrollbarTo(e);
      }
    });
    window.addEventListener('mouseup', () => {
      dragging = null;
    });

    // 貼り付け。canvas は編集可能な要素ではないので paste は document に来る。
    // **文字列は物理キーに直せない**ので、こちらはASCIIとして送る
    // (Shiftの上げ下げはゲスト側の都合に合わせてRustが組み立てる)
    document.addEventListener('paste', e => {
      if (document.activeElement !== c) return;
      const text = e.clipboardData?.getData('text');
      if (text) {
        this.onPaste?.(text);
        e.preventDefault();
      }
    });

    c.addEventListener('keydown', e => {
      // コピーは端末が受け取る (ゲストへは渡さない)
      if ((e.metaKey || e.ctrlKey) && e.code === 'KeyC' && this.selectedText()) {
        this.#copy(this.selectedText());
        this.selection = null;
        this.draw();
        e.preventDefault();
        return;
      }
      // 打ったら最新へ戻る
      if (this.offset !== 0) this.scrollTo(0);

      if (this.#sendKey(e, true)) e.preventDefault();
    });

    c.addEventListener('keyup', e => {
      if (this.#sendKey(e, false)) e.preventDefault();
    });
  }

  /**
   * キーをゲストへ流す。配列によって「位置」と「文字」を使い分ける。
   *
   * 修飾キーと、文字に対応しないキー (Enter/Esc/矢印など) は
   * **どちらの配列でも位置で送る** — その並びは配列によらず同じだからである。
   */
  #sendKey(e, down) {
    const isModifier = /^(Control|Alt|Meta|Shift)/.test(e.code);
    const printable = e.key.length === 1;

    if (this.layout === 'us' || !printable || e.ctrlKey || e.altKey || e.metaKey) {
      // JP配列でも Shift は握りつぶす。文字を送る側 (下) が自分で組み立てるので、
      // 両方から送ると二重になる
      if (this.layout === 'jp' && /^Shift/.test(e.code)) return true;
      return this.onKey?.(e.code, down) ?? false;
    }
    if (isModifier) return this.onKey?.(e.code, down) ?? false;
    // 文字は押したときだけ送る (離すときの相方は文字を送る側が付ける)
    if (down) this.onChar?.(e.key);
    return true;
  }

  /**
   * 文字列をクリップボードへ。
   *
   * `navigator.clipboard` は**セキュアな配信元でしか存在しない**。
   * localhost は該当するが、LAN内のIPアドレスで開くと消える。
   * 黙って何も起きないのが一番困るので、古いやり方に落とす。
   */
  #copy(text) {
    if (navigator.clipboard) {
      navigator.clipboard.writeText(text).catch(() => this.#copyFallback(text));
    } else {
      this.#copyFallback(text);
    }
  }

  #copyFallback(text) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand('copy');
    } catch {
      /* ここまで来たら諦める */
    }
    ta.remove();
    this.canvas.focus();
  }

  #scrollbarTo(ev) {
    const r = this.canvas.getBoundingClientRect();
    const scale = this.canvas.height / r.height;
    const y = (ev.clientY - r.top) * scale;
    const ratio = 1 - Math.max(0, Math.min(1, y / this.canvas.height));
    this.scrollTo(Math.round(ratio * this.scrollback.length));
  }
}
