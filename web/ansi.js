// ANSI (VT100) 端末 — Linux のシリアルコンソール用。
//
// ## なぜ VGAテキスト端末 (terminal.js) と別なのか
//
// ELKS/FreeDOS は 0xB8000 のテキストVRAM に「文字+属性」を直接置く。画面は
// 80×25 の格子で、こちらはその格子を写すだけでよかった。
//
// Linux のシリアルコンソールは違う。バイト列が流れてくるだけで、**カーソルを
// どこへ動かし、どの色で、何を書くかは全部エスケープシーケンスで指示される**。
// `\x1b[H` でホームへ、`\x1b[2J` で全消し、`\x1b[1;31m` で赤太字。
// vi も snake も、この作法で画面を描く。だから受け手側に**状態機械**が要る。
//
// 使い勝手は terminal.js に合わせてある: スクロールバック1000行 (ホイールと
// 右端のスクロールバー、キーを打つと最新へ)、カーソルの点滅。
// **シリアルコンソールを外から覗いている**という意味づけなので、
// 流れて消えた行を端末側が控えるのは実物の端末エミュレータと同じ振る舞い。
//
// ## 実装の範囲
//
// busybox・vi・snake が実際に吐くものだけ実装する。全 VT100 は追わない:
//   - カーソル移動 (CUP/CUU/CUD/CUF/CUB)、絶対位置 `\x1b[y;xH`
//   - 消去 (ED `\x1b[2J`、EL `\x1b[K`)
//   - SGR (色・太字・反転)  ・ カーソル表示/非表示 `\x1b[?25l/h`
//   - `\r` `\n` `\b` `\t` と、80桁での折り返し

import { IS_MAC, isClipboardCombo } from './terminal.js';

const CELL_W = 9;
const CELL_H = 16;
const SCROLLBAR_W = 10;
const FONT = '13px ui-monospace, Menlo, monospace';
/** カーソルの点滅周期 (terminal.js と同じ) */
const BLINK_MS = 530;

// xterm の16色 (0-7 通常、8-15 明色) を、VGA端末と同じ Homebrew 緑燐光に寄せる。
// **既定色 (7) と明るい白 (15) だけを緑にする**のが肝 — 全部緑にすると
// ゲストが色分けした情報 (ls の青いディレクトリ、エラーの赤) が読めなくなる
const PALETTE = [
  '#000000', '#cd0000', '#00cd00', '#cdcd00', '#3388ff', '#cd00cd', '#00cdcd', '#00ff00',
  '#7f7f7f', '#ff5555', '#55ff55', '#ffff55', '#5555ff', '#ff55ff', '#55ffff', '#7cff7c',
];

const SCROLL = {
  banner: 'rgba(0, 90, 0, 0.85)',
  bannerText: '#7cff7c',
  track: 'rgba(0, 255, 0, 0.10)',
  thumb: 'rgba(0, 255, 0, 0.45)',
};

export class AnsiTerminal {
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
    this.cols = opts.cols ?? 80;
    this.rows = opts.rows ?? 24;
    this.scrollbackLimit = opts.scrollback ?? 1000;
    canvas.width = this.cols * CELL_W + SCROLLBAR_W;
    canvas.height = this.rows * CELL_H;

    // 各セル: {ch, fg, bg, bold, inv}
    this.grid = null;
    /** 画面上端から流れて消えた行の控え (セルごと持つので色も残る) */
    this.scrollback = [];
    /** 何行さかのぼって見ているか (0 = 最新) */
    this.offset = 0;
    /** ドラッグで選んだ範囲 {a:{row,col}, b:{row,col}} — 表示上の座標 */
    this.selection = null;
    this.reset();

    // パーサの状態
    this.state = 'text'; // 'text' | 'esc' | 'csi'
    this.params = '';
    this.dirty = true;

    // 入力: キーが来たら onData(str) を呼ぶ (端末→ゲスト)
    this.onData = null;
    /** 画素の顔 (drawRgb) が出ている間は、文字ではなく**物理キー**を渡す。
     *  (code, down) => void。シェルが tty0 (fbcon) に居るときは、入力も
     *  PS/2 キーボード (8042) からカーネルのVTへ入るのが実機の道 */
    this.onKey = null;
    /** 文字で送る口 (JIS 配列のとき)。位置 (onKey) だと US の表で読まれて記号がずれる */
    this.onChar = null;
    /** キー配列 'jp' | 'us'。画素/VGA の顔のときだけ効く (シリアルは文字そのものを送るので無関係) */
    this.layout = 'jp';
    /** マウスの動き (画素の顔で捕獲中)。(dx, dy, buttons) => void。
     *  dx/dy は相対移動 (pointer lock の movementX/Y)、buttons は bit0=左 bit1=右 bit2=中 */
    this.onMouse = null;
    /** 捕獲の出入りの合図 (true=捕獲した / false=解放した)。表示側が枠や案内を出す */
    this.onCapture = null;
    this.captured = false;
    // クリップボードは**VGA端末と同じ取っ手**にする (行き先は main.js が決める)。
    // ここで onData へ直に流すと、取り消しも状態表示も無い別経路が生まれる
    /** 貼り付けられたときに呼ばれる (⌘V など、中身が届く経路)。(text) => void */
    this.onPaste = null;
    /** 貼り付けの組みが押されたときに呼ばれる (クリップボードは呼び手が読む) */
    this.onPasteRequest = null;
    /** コピーの組みが押されたときに呼ばれる (何をコピーするかは呼び手が決める) */
    this.onCopyRequest = null;
    this._utf8 = []; // 受信UTF-8の途中バイト

    canvas.tabIndex = 0;
    canvas.style.outline = 'none';
    canvas.addEventListener('keydown', (e) => {
      // **コピー/ペーストの組みはゲストへ渡さない** (VGA端末と同じ判断を使う)。
      // 素の Ctrl+C はゲストのもの — シリアルでも SIGINT の鍵である
      if (isClipboardCombo(e, 'KeyC')) {
        e.preventDefault();
        this.onCopyRequest?.();
        return;
      }
      if (isClipboardCombo(e, 'KeyV')) {
        // ⌘V はブラウザが paste 事象をくれるので、そちらに任せる
        if (!IS_MAC) {
          e.preventDefault();
          this.onPasteRequest?.();
        }
        return;
      }
      // 画素の顔が出ている = シェルは tty0。文字にせず位置 (code) を渡す
      if ((this.gfxOn || this.vgaOn) && this.onKey) {
        e.preventDefault();
        this.#sendKey(e, true);
        return;
      }
      this._key(e);
    });
    canvas.addEventListener('keyup', (e) => {
      if ((this.gfxOn || this.vgaOn) && this.onKey) {
        e.preventDefault();
        this.#sendKey(e, false);
      }
    });

    // ---- ポインタの捕獲 (画素の顔のとき) ----
    //
    // 抜け道は4系統 (設計は rustx86-gfx-plan): ①ホストキー Ctrl+Alt+G
    // ②Esc (窓モードではブラウザが強制解除する — 戦わず利用する)
    // ③全画面ではEsc長押し (ブラウザの既定) ④自動解放 (blur・機械の停止)。
    // **捕獲中は必ず見た目が変わり、抜け方がその場に書いてある** (onCapture)
    // 捕獲は2段: pointer lock が取れればそれ (相対移動が生で来る)。取れない
    // 環境 (pointer-lock を許さない iframe、埋め込みの閲覧面) では**ソフト捕獲**
    // — canvas 上の移動を前回の座標との差で相対化して渡す。ポインタは
    // 画面の外へ出られるが、入力の経路は同じなので動作の確認はできる
    // ---- マウス (FB の顔): 相対デバイスのまま絶対位置に見せる ----
    //
    // **捕獲しない。** canvas の上にホストのカーソルがある間だけゲストへ届き、
    // 枠を出れば自然に抜ける (VNC の絶対モードの触り心地)。脱出キーは要らず、
    // Esc はずっと vi のもの。
    // PS/2 は相対しか送れないので、X 側で加速を切り (xorg.conf の
    // AccelerationScheme none) 「送った差分 = 動く画素」にした上で、**枠に入る
    // たび/押す直前に左上の角へ押し込んで位置を合わせ直す** — X は 0,0 で止める
    // ので、そこからホスト座標ぶん進めれば揃う。ズレても次の入り直しで消える。
    // 以前の捕獲 (pointer lock / ホストキー Ctrl+Alt+Shift+G) は、加速でズレる
    // から閉じ込めるしかなかった形で、根を切ったので要らなくなった (2026-08-22)
    let sent = null; // ゲストのカーソルが居るはずの canvas 画素 (整数)
    const guestXY = (e) => {
      const r = canvas.getBoundingClientRect();
      const k = canvas.width / r.width;
      const clamp = (v, hi) => Math.max(0, Math.min(hi - 1, Math.round(v)));
      return { x: clamp((e.clientX - r.left) * k, canvas.width), y: clamp((e.clientY - r.top) * k, canvas.height) };
    };
    // PS/2 の1パケットは ±255 まで (素子もそこで切る) — 大きい差分は分けて送る
    const send = (dx, dy, buttons) => {
      // 入力の道筋を追う覗き窓 (コンソールで `__rx86dbg = true`)。
      // 「画面は出るのに操作できない」を切り分けるのに要る — 送っていないのか、
      // 送っているのにゲストが無視しているのかは、ここを見ないと区別できない
      if (globalThis.__rx86dbg) console.log('[dbg] mouse', dx, dy, buttons, 'onMouse=', !!this.onMouse);
      while (dx || dy) {
        const sx = Math.max(-255, Math.min(255, dx)), sy = Math.max(-255, Math.min(255, dy));
        this.onMouse?.(sx, sy, buttons);
        dx -= sx;
        dy -= sy;
      }
    };
    const resync = (p, buttons) => {
      // 左上の角へ画面より多く押し込む (X は 0,0 で止める) → 座標ぶん進める
      const over = Math.max(canvas.width, canvas.height) + 255;
      send(-over, -over, buttons);
      send(p.x, p.y, buttons);
      sent = p;
    };
    const move = (e) => {
      if (!this.gfxOn || !this.onMouse) return;
      const p = guestXY(e), b = e.buttons & 7;
      if (!sent) { resync(p, b); return; }
      const dx = p.x - sent.x, dy = p.y - sent.y;
      if (dx || dy) { send(dx, dy, b); sent = p; }
    };
    canvas.addEventListener('mousemove', move);
    canvas.addEventListener('mouseenter', (e) => { sent = null; move(e); });
    canvas.addEventListener('mouseleave', () => { sent = null; });
    canvas.addEventListener('mousedown', (e) => {
      if (!this.gfxOn || !this.onMouse) return;
      e.preventDefault();
      canvas.focus();
      // 押す直前に合わせ直す (ゲストが自分でカーソルを飛ばしていてもここで揃う)。
      // 角への押し込みは**押す前のボタン状態**で行い、押下は揃ってから
      const pressed = [1, 4, 2][e.button] ?? 0; // DOM button → PS/2 bit (左1 中4 右2)
      resync(guestXY(e), (e.buttons & 7) & ~pressed);
      this.onMouse(0, 0, e.buttons & 7);
    });
    canvas.addEventListener('mouseup', (e) => {
      if (!this.gfxOn || !this.onMouse) return;
      e.preventDefault();
      this.onMouse(0, 0, e.buttons & 7);
    });
    canvas.addEventListener('contextmenu', (e) => { if (this.gfxOn) e.preventDefault(); });
    /** 互換: 捕獲は無くなったので何もしない (表示側が停止時に呼ぶ) */
    this.releaseCapture = () => {};
    canvas.addEventListener('paste', (e) => {
      const t = e.clipboardData?.getData('text');
      if (t) this.onPaste?.(t);
      e.preventDefault();
    });

    // 過去を見る: ホイールと右端のスクロールバー (terminal.js と同じ操作感)
    canvas.addEventListener(
      'wheel',
      (e) => {
        e.preventDefault();
        this.scrollTo(this.offset + (e.deltaY > 0 ? -3 : 3));
      },
      { passive: false },
    );
    // ドラッグで選ぶ (VGA端末と同じ操作感)。**選んだ範囲だけがコピーの対象**
    let dragging = null;
    canvas.addEventListener('mousedown', (e) => {
      if (this.gfxOn) return; // 画素の顔では選択もスクロールバーも無い (マウスはゲストへ)
      // 右ボタンでは選択を触らない (選んで右を押してコピー、が成り立つように)
      if (e.button !== 0) return;
      const p = this._cellAt(e);
      if (p.onScrollbar) {
        dragging = 'scrollbar';
        this._scrollbarTo(e);
      } else {
        dragging = 'select';
        this.selection = { a: { row: p.row, col: p.col }, b: { row: p.row, col: p.col } };
        this.dirty = true;
      }
      canvas.focus();
      e.preventDefault();
    });
    window.addEventListener('mousemove', (e) => {
      if (dragging === 'select') {
        const p = this._cellAt(e);
        this.selection.b = { row: p.row, col: p.col };
        this.dirty = true;
      } else if (dragging === 'scrollbar') {
        this._scrollbarTo(e);
      }
    });
    window.addEventListener('mouseup', () => {
      dragging = null;
    });

    // カーソルの点滅 (VGA端末と同じ周期)
    this.blinkOn = true;
    setInterval(() => {
      this.blinkOn = !this.blinkOn;
      if (this.offset === 0 && this.cursorVisible) this.dirty = true;
    }, BLINK_MS);
  }

  reset() {
    this._leaveGfx();
    this.vgaOn = false;
    this._setRows(this.textRows ?? this.rows);
    this.grid = Array.from({ length: this.rows }, () =>
      Array.from({ length: this.cols }, () => this._blank()),
    );
    this.scrollback = [];
    this.offset = 0;
    this.cx = 0;
    this.cy = 0;
    this.fg = 7;
    this.bg = 0;
    this.bold = false;
    this.inv = false;
    this.cursorVisible = true;
    this.selection = null;
    this.dirty = true;
  }

  /**
   * 画素/VGA の顔でキーをゲストへ。配列によって「位置」と「文字」を使い分ける
   * (terminal.js の VGA 機と同じ規則)。ゲストの Linux は US の表しか持たないので、
   * JIS の実機で記号を打つと位置では `:` が `'` に化ける — 文字 (e.key) から
   * US の位置を逆算して送る。修飾キー・文字でないキー・スペース・Ctrl/Alt 付きは
   * 配列によらず位置で送る (DOOM の「押されているか」の判定はスペースの押下/解放が要る)
   */
  #sendKey(e, down) {
    // 同上 (キー側)。gfxOn/vgaOn がどちらも偽なら、キーはシリアルへ流れている
    if (globalThis.__rx86dbg) console.log('[dbg] sendKey', e.code, down, 'gfxOn=', this.gfxOn, 'onKey=', !!this.onKey);
    const printable = e.key.length === 1;
    if (this.layout === 'us' || !printable || e.code === 'Space' || e.ctrlKey || e.altKey || e.metaKey) {
      if (this.layout === 'jp' && /^Shift/.test(e.code)) return; // 文字側が自分で Shift を組む
      this.onKey(e.code, down);
      return;
    }
    if (down) this.onChar?.(e.key === '¥' ? '\\' : e.key);
  }

  _blank() {
    return { ch: ' ', fg: 7, bg: 0, bold: false, inv: false };
  }

  /** 画素の顔 (drawRgb) を畳んで文字の升目に戻す */
  _leaveGfx() {
    this.gfxOn = false;
    if (this.textSize) {
      // 文字の顔へ戻す: canvas の寸法も升目に戻す
      this.canvas.width = this.textSize.w;
      this.canvas.height = this.textSize.h;
      this.canvas.classList.remove('fb');
      this.canvas.style.removeProperty('--fbw');
      this.canvas.style.removeProperty('--fbh');
      this.canvas.style.removeProperty('--fbscale');
      this.canvas.style.imageRendering = '';
      this.fitObserver?.disconnect();
      this.gfx = null;
    }
  }

  /** 行数を変える (シリアル端末は 24 行、VGA のテキストは 25 行)。canvas の高さも合わせる */
  _setRows(n) {
    this.textRows ??= this.rows;
    if (n === this.rows && this.canvas.height === n * CELL_H) return;
    this.rows = n;
    this.canvas.height = n * CELL_H;
    if (this.textSize) this.textSize.h = n * CELL_H;
    // CSS の aspect-ratio は 24 行前提 (730/384) なので、違う行数は inline で言う
    if (n === this.textRows) this.canvas.style.removeProperty('aspect-ratio');
    else this.canvas.style.aspectRatio = `${this.canvas.width} / ${this.canvas.height}`;
    this.grid = Array.from({ length: n }, () => Array.from({ length: this.cols }, () => this._blank()));
    this.cy = Math.min(this.cy, n - 1);
    this.dirty = true;
  }

  /** VGA のテキスト VRAM (文字+属性の 2 バイト×80×25) をそのまま升目に写す。
   * ISO 機 (BIOS 経由の起動) の画面はシリアルでなくここ — 文字は CP437 の表で
   * Unicode に、属性は fg 下位 3bit + 明るさ (bold) / bg 3bit に読み替える。
   * 出ている間、キーは文字でなく位置 (onKey) でゲストへ行く (画素の顔と同じ) */
  showVga(cells, row, col, charset) {
    if (charset) this.vgaCharset = charset;
    const cs = this.vgaCharset;
    this._leaveGfx();
    this.vgaOn = true;
    this._setRows(25);
    this.scrollback = [];
    this.offset = 0;
    for (let y = 0; y < 25; y++) {
      const line = this.grid[y];
      for (let x = 0; x < this.cols; x++) {
        const i = (y * this.cols + x) * 2;
        const ch = cells[i];
        const attr = cells[i + 1];
        const cell = line[x];
        cell.ch = ch === 0 || ch === 32 ? ' ' : cs ? cs[ch] ?? ' ' : String.fromCharCode(ch);
        cell.fg = attr & 7;
        cell.bold = (attr & 8) !== 0;
        cell.bg = (attr >> 4) & 7;
        cell.inv = false;
      }
    }
    this.cx = Math.min(col, this.cols - 1);
    this.cy = Math.min(row, 24);
    this.cursorVisible = row < 25;
    this.dirty = true;
  }

  // ---- 端末自体の状態の写し (機械のスナップショットに添える) ----
  //
  // 機械の状態にシリアルの**履歴**は入らない (送信済みは状態ではない)。
  // でも「状態を復元」で画面とカーソルが保存時の姿に戻らないと、
  // VGA機 (画面がメモリの中にある) と使い勝手が揃わない。
  // だから端末側の姿は端末側で控えて、機械の控えと一緒に出し入れする。

  snapshot() {
    const copyRows = (rows) => rows.map((row) => row.map((c) => ({ ...c })));
    return {
      grid: copyRows(this.grid),
      scrollback: copyRows(this.scrollback),
      cx: this.cx,
      cy: this.cy,
      fg: this.fg,
      bg: this.bg,
      bold: this.bold,
      inv: this.inv,
      cursorVisible: this.cursorVisible,
    };
  }

  restore(snap) {
    const copyRows = (rows) => rows.map((row) => row.map((c) => ({ ...c })));
    this.grid = copyRows(snap.grid);
    this.scrollback = copyRows(snap.scrollback);
    this.cx = snap.cx;
    this.cy = snap.cy;
    this.fg = snap.fg;
    this.bg = snap.bg;
    this.bold = snap.bold;
    this.inv = snap.inv;
    this.cursorVisible = snap.cursorVisible;
    this.offset = 0;
    this.state = 'text';
    this.params = '';
    this._utf8 = [];
    this.dirty = true;
  }

  /** 全文 (履歴+画面) をテキストで。「ログを保存」の材料 */
  allText() {
    const rowText = (row) =>
      row
        .map((c) => c.ch)
        .join('')
        .replace(/\s+$/, '');
    return [...this.scrollback, ...this.grid].map(rowText).join('\n');
  }

  // ---- 受信: ゲスト → 画面 ----

  write(bytes) {
    for (const b of bytes) this._byte(b);
    this.dirty = true;
  }

  _byte(b) {
    // UTF-8 の連続バイトを組む (日本語のバナー等)
    if (this.state === 'text') {
      if (b >= 0x80) {
        this._utf8.push(b);
        // 先頭バイトから長さを決める
        const lead = this._utf8[0];
        const len = lead >= 0xf0 ? 4 : lead >= 0xe0 ? 3 : 2;
        if (this._utf8.length >= len) {
          const s = new TextDecoder().decode(new Uint8Array(this._utf8));
          this._utf8 = [];
          for (const ch of s) this._putch(ch);
        }
        return;
      } else if (this._utf8.length) {
        this._utf8 = []; // 壊れた列は捨てる
      }
    }

    const c = String.fromCharCode(b);
    switch (this.state) {
      case 'text':
        if (b === 0x1b) this.state = 'esc';
        else this._control(b, c);
        break;
      case 'esc':
        if (c === '[') {
          this.state = 'csi';
          this.params = '';
        } else if (c === 'M') {
          this._reverseLineFeed();
          this.state = 'text';
        } else {
          this.state = 'text'; // 未対応の2文字エスケープは捨てる
        }
        break;
      case 'csi':
        if (b >= 0x30 && b <= 0x3f) {
          this.params += c; // パラメータ (数字・; ・ ?)
        } else {
          this._csi(c);
          this.state = 'text';
        }
        break;
    }
  }

  _control(b, c) {
    switch (b) {
      case 0x0a: // LF
        this._newline();
        break;
      case 0x0d: // CR
        this.cx = 0;
        break;
      case 0x08: // BS
        if (this.cx > 0) this.cx--;
        break;
      case 0x09: // TAB
        this.cx = Math.min(this.cols - 1, (this.cx + 8) & ~7);
        break;
      case 0x07: // BEL
        break;
      default:
        if (b >= 0x20) this._putch(c);
    }
  }

  _putch(ch) {
    if (this.cx >= this.cols) {
      this.cx = 0;
      this._newline();
    }
    this.grid[this.cy][this.cx] = {
      ch,
      fg: this.fg,
      bg: this.bg,
      bold: this.bold,
      inv: this.inv,
    };
    this.cx++;
  }

  _newline() {
    this.cy++;
    if (this.cy >= this.rows) {
      // 上端から消える行を控える (シリアルを外から覗く端末の流儀)。
      // vi や snake はカーソル移動で描き直すのでここは通らず、
      // 履歴に積もるのはシェルと dmesg の流れだけ — それでよい
      const gone = this.grid.shift();
      this.scrollback.push(gone);
      while (this.scrollback.length > this.scrollbackLimit) {
        this.scrollback.shift();
      }
      if (this.offset > 0) {
        this.offset = Math.min(this.offset + 1, this.scrollback.length);
      }
      this.grid.push(Array.from({ length: this.cols }, () => this._blank()));
      this.cy = this.rows - 1;
    }
  }

  _reverseLineFeed() {
    this.cy--;
    if (this.cy < 0) {
      this.grid.pop();
      this.grid.unshift(Array.from({ length: this.cols }, () => this._blank()));
      this.cy = 0;
    }
  }

  _csi(final) {
    const priv = this.params.startsWith('?');
    const nums = (priv ? this.params.slice(1) : this.params)
      .split(';')
      .map((x) => (x === '' ? 0 : parseInt(x, 10)));
    const n = nums[0] || 0;

    switch (final) {
      case 'H': // CUP: 絶対位置 (1始まり)
      case 'f': {
        this.cy = Math.max(0, Math.min(this.rows - 1, (nums[0] || 1) - 1));
        this.cx = Math.max(0, Math.min(this.cols - 1, (nums[1] || 1) - 1));
        break;
      }
      case 'A': this.cy = Math.max(0, this.cy - (n || 1)); break;
      case 'B': this.cy = Math.min(this.rows - 1, this.cy + (n || 1)); break;
      case 'C': this.cx = Math.min(this.cols - 1, this.cx + (n || 1)); break;
      case 'D': this.cx = Math.max(0, this.cx - (n || 1)); break;
      case 'G': this.cx = Math.max(0, Math.min(this.cols - 1, (n || 1) - 1)); break;
      case 'd': this.cy = Math.max(0, Math.min(this.rows - 1, (n || 1) - 1)); break;
      case 'J': // ED: 画面消去
        this._erase(n, true);
        break;
      case 'K': // EL: 行消去
        this._erase(n, false);
        break;
      case 'm': // SGR: 色・属性
        this._sgr(nums);
        break;
      case 'h':
        if (priv && n === 25) this.cursorVisible = true;
        break;
      case 'l':
        if (priv && n === 25) this.cursorVisible = false;
        break;
      case 'r': // スクロール領域: 全域として無視 (busyboxは使わない)
      case 'n': // デバイス状態問い合わせ (\x1b[6n) — 位置を返す
        if (n === 6) {
          this.onData?.(`\x1b[${this.cy + 1};${this.cx + 1}R`);
        }
        break;
    }
  }

  _erase(mode, screen) {
    const blank = () => this._blank();
    if (screen) {
      // 0: カーソル以降, 1: カーソルまで, 2: 全部
      if (mode === 2) {
        for (const row of this.grid) for (let x = 0; x < this.cols; x++) row[x] = blank();
      } else if (mode === 0) {
        for (let x = this.cx; x < this.cols; x++) this.grid[this.cy][x] = blank();
        for (let y = this.cy + 1; y < this.rows; y++)
          for (let x = 0; x < this.cols; x++) this.grid[y][x] = blank();
      } else if (mode === 1) {
        for (let x = 0; x <= this.cx; x++) this.grid[this.cy][x] = blank();
        for (let y = 0; y < this.cy; y++)
          for (let x = 0; x < this.cols; x++) this.grid[y][x] = blank();
      }
    } else {
      const row = this.grid[this.cy];
      if (mode === 2) for (let x = 0; x < this.cols; x++) row[x] = blank();
      else if (mode === 0) for (let x = this.cx; x < this.cols; x++) row[x] = blank();
      else if (mode === 1) for (let x = 0; x <= this.cx; x++) row[x] = blank();
    }
  }

  _sgr(nums) {
    for (let i = 0; i < nums.length; i++) {
      const p = nums[i];
      if (p === 0) { this.fg = 7; this.bg = 0; this.bold = false; this.inv = false; }
      else if (p === 1) this.bold = true;
      else if (p === 22) this.bold = false;
      else if (p === 7) this.inv = true;
      else if (p === 27) this.inv = false;
      else if (p >= 30 && p <= 37) this.fg = p - 30;
      else if (p === 39) this.fg = 7;
      else if (p >= 40 && p <= 47) this.bg = p - 40;
      else if (p === 49) this.bg = 0;
      else if (p >= 90 && p <= 97) this.fg = p - 90 + 8;
      else if (p >= 100 && p <= 107) this.bg = p - 100 + 8;
    }
  }

  // ---- スクロールバック ----

  scrollTo(offset) {
    const next = Math.max(0, Math.min(this.scrollback.length, offset));
    if (next !== this.offset) {
      this.offset = next;
      this.dirty = true;
    }
  }

  /** 見せる行: 最新なら画面そのもの、遡っていれば履歴+画面の窓 */
  _view() {
    if (this.offset === 0) return this.grid;
    const all = [...this.scrollback, ...this.grid];
    const start = Math.max(0, all.length - this.rows - this.offset);
    return all.slice(start, start + this.rows);
  }

  /** マウスの位置を桁と行に直す (右端の帯の上かどうかも返す) */
  _cellAt(ev) {
    const r = this.canvas.getBoundingClientRect();
    const x = ((ev.clientX - r.left) * this.canvas.width) / r.width;
    const y = ((ev.clientY - r.top) * this.canvas.height) / r.height;
    return {
      row: Math.max(0, Math.min(this.rows - 1, Math.floor(y / CELL_H))),
      col: Math.max(0, Math.min(this.cols, Math.round(x / CELL_W))),
      onScrollbar: x >= this.cols * CELL_W,
    };
  }

  /** 始点と終点を、上から下・左から右の順に直す (逆向きにも引けるので) */
  _orderedSelection() {
    const { a, b } = this.selection;
    const back = b.row < a.row || (b.row === a.row && b.col < a.col);
    return back ? { a: b, b: a } : { a, b };
  }

  /** 何か選ばれているか (中身は組み立てない — ドラッグ中に毎回呼ばれる) */
  hasSelection() {
    if (!this.selection) return false;
    const { a, b } = this.selection;
    return a.row !== b.row || a.col !== b.col;
  }

  /** 選んだ文字列 (何も選んでいなければ空) */
  selectedText() {
    if (!this.selection) return '';
    const { a, b } = this._orderedSelection();
    const view = this._view();
    const out = [];
    for (let r = a.row; r <= b.row; r++) {
      const line = (view[r] ?? [])
        .map((c) => c.ch)
        .join('')
        .padEnd(this.cols, ' ');
      const from = r === a.row ? a.col : 0;
      const to = r === b.row ? b.col : this.cols;
      out.push(line.slice(from, to).replace(/\s+$/, ''));
    }
    return out.join('\n');
  }

  _scrollbarTo(ev) {
    const r = this.canvas.getBoundingClientRect();
    const y = ((ev.clientY - r.top) * this.canvas.height) / r.height;
    const ratio = 1 - Math.max(0, Math.min(1, y / this.canvas.height));
    this.scrollTo(Math.round(ratio * this.scrollback.length));
  }

  // ---- 描画 ----

  /** efifb が描いた一枚を置く。ワーカーが写すついでに canvas の形 (RGBA、
   * fmt='rgba') に詰め替えてくるのが常道で、ここは ImageData を被せて
   * putImageData するだけ (3MB の読み書きをメインでしない — ADR-0028 G1〜G3)。
   * fmt='raw' は保険: 32bpp=[詰め物,R,G,B] / 24bpp=[R,G,B] をここで並べ替える。
   * 出ている間、文字の描き手 (render) は黙る。戻すのは reset() */
  drawRgb(rgb, width, height, bpp = 24, fmt = 'raw') {
    this.gfxOn = true;
    if (!this.gfx || this.gfx.w !== width || this.gfx.h !== height) {
      this.gfx = { w: width, h: height, img: null };
      // **canvas をゲストの解像度に張り替える** (等倍)。文字の升目 (730×384) に
      // 縮めて収めると 8×16 のフォントが潰れる。見た目の大きさは CSS (.fb) が決める
      this.textSize ??= { w: this.canvas.width, h: this.canvas.height };
      this.canvas.width = width;
      this.canvas.height = height;
      this.canvas.classList.add('fb');
      // 見た目の大きさは CSS が決める (--fbw で解像度を伝える)。縮小されるときは
      // pixelated を切る — 非整数の最近傍縮小は文字が欠けて読めない
      this.canvas.style.setProperty('--fbw', String(width));
      this.canvas.style.setProperty('--fbh', String(height));
      // 小さい解像度 (mode 13h の 320×200) は 3 倍まで伸ばす — 1.5 倍だと切手になる
      this.canvas.style.setProperty('--fbscale', width <= 400 ? '3' : '1.5');
      const fit = () => {
        const shown = this.canvas.getBoundingClientRect().width;
        this.canvas.style.imageRendering = shown + 0.5 < width ? 'auto' : 'pixelated';
      };
      fit();
      (this.fitObserver ??= new ResizeObserver(fit)).observe(this.canvas);
    }
    if (fmt === 'rgba') {
      // 届いたバッファをそのまま ImageData に被せる (コピー無し)。バッファは
      // 呼び手が ack でワーカーへ返すので、ImageData はこの一回限り
      const img = new ImageData(new Uint8ClampedArray(rgb.buffer, rgb.byteOffset, width * height * 4), width, height);
      this.ctx.putImageData(img, 0, 0);
      return;
    }
    this.gfx.img ??= new ImageData(width, height);
    const d = this.gfx.img.data;
    if (bpp === 32) {
      // [詰め物, R, G, B] の4バイト (X が扱える形。赤は第2バイト)
      for (let i = 0, o = 0, n = width * height; i < n; i++, o += 4) {
        d[o] = rgb[i * 4 + 1];
        d[o + 1] = rgb[i * 4 + 2];
        d[o + 2] = rgb[i * 4 + 3];
        d[o + 3] = 255;
      }
    } else {
      for (let i = 0, o = 0, n = width * height; i < n; i++, o += 4) {
        d[o] = rgb[i * 3];
        d[o + 1] = rgb[i * 3 + 1];
        d[o + 2] = rgb[i * 3 + 2];
        d[o + 3] = 255;
      }
    }
    this.ctx.putImageData(this.gfx.img, 0, 0);
  }

  render() {
    if (this.gfxOn) return; // 画素の一枚の上に文字の黒地を被せない
    if (!this.dirty) return;
    this.dirty = false;
    const ctx = this.ctx;
    ctx.fillStyle = '#000';
    ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.textBaseline = 'top';

    const view = this._view();

    for (let y = 0; y < view.length; y++) {
      for (let x = 0; x < this.cols; x++) {
        const cell = view[y][x];
        let fg = cell.fg,
          bg = cell.bg;
        if (cell.inv) [fg, bg] = [bg, fg];
        if (bg !== 0) {
          ctx.fillStyle = PALETTE[bg];
          ctx.fillRect(x * CELL_W, y * CELL_H, CELL_W, CELL_H);
        }
        if (cell.ch !== ' ') {
          ctx.font = (cell.bold ? 'bold ' : '') + FONT;
          ctx.fillStyle = PALETTE[cell.bold && fg < 8 ? fg + 8 : fg];
          ctx.fillText(cell.ch, x * CELL_W, y * CELL_H + 1);
        }
      }
    }

    if (this.selection) {
      const { a, b } = this._orderedSelection();
      this.ctx.fillStyle = 'rgba(0, 255, 0, 0.30)';
      for (let r = a.row; r <= b.row; r++) {
        const from = r === a.row ? a.col : 0;
        const to = r === b.row ? b.col : this.cols;
        this.ctx.fillRect(from * CELL_W, r * CELL_H, (to - from) * CELL_W, CELL_H);
      }
    }

    if (this.offset === 0) {
      if (this.cursorVisible && this.blinkOn) {
        ctx.fillStyle = '#23ff18'; // カーソルも燐光色 (VGA端末と同じ)
        ctx.fillRect(this.cx * CELL_W, this.cy * CELL_H + CELL_H - 2, CELL_W, 2);
      }
    } else {
      // 遡り中の目印 (VGA端末と同じ)
      ctx.fillStyle = SCROLL.banner;
      ctx.fillRect(0, 0, this.cols * CELL_W, CELL_H);
      ctx.font = FONT;
      ctx.fillStyle = SCROLL.bannerText;
      ctx.fillText(`▲ ${this.offset}行前  (キーを打つと最新へ)`, 4, 1);
    }

    // 右端のスクロールバー (これがあればログを別枠に出す必要が無い)
    const x = this.cols * CELL_W;
    const h = this.canvas.height;
    ctx.fillStyle = SCROLL.track;
    ctx.fillRect(x, 0, SCROLLBAR_W, h);
    const total = this.scrollback.length + this.rows;
    const thumbH = Math.max(20, (h * this.rows) / total);
    const maxOffset = this.scrollback.length;
    const pos = maxOffset === 0 ? 1 : 1 - this.offset / maxOffset;
    ctx.fillStyle = SCROLL.thumb;
    ctx.fillRect(x + 1, (h - thumbH) * pos, SCROLLBAR_W - 2, thumbH);
  }

  // ---- 入力: キー → ゲスト ----

  _key(e) {
    let s = null;
    const k = e.key;
    if (k === 'Enter') s = '\r';
    else if (k === 'Backspace') s = '\x7f';
    else if (k === 'Tab') s = '\t';
    else if (k === 'Escape') s = '\x1b';
    else if (k === 'ArrowUp') s = '\x1b[A';
    else if (k === 'ArrowDown') s = '\x1b[B';
    else if (k === 'ArrowRight') s = '\x1b[C';
    else if (k === 'ArrowLeft') s = '\x1b[D';
    else if (k === 'Home') s = '\x1b[H';
    else if (k === 'End') s = '\x1b[F';
    else if (k === 'Delete') s = '\x1b[3~';
    else if (e.ctrlKey && k.length === 1) {
      // Ctrl+A..Z → 制御文字 (Ctrl-C=0x03 等)
      const code = k.toUpperCase().charCodeAt(0);
      if (code >= 64 && code <= 95) s = String.fromCharCode(code - 64);
    } else if (k.length === 1 && !e.metaKey) {
      s = k;
    }
    if (s !== null) {
      // 打ったら最新へ戻る (VGA端末と同じ)
      if (this.offset !== 0) this.scrollTo(0);
      this.onData?.(s);
      e.preventDefault();
    }
  }
}
