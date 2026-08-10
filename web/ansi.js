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
// ## 実装の範囲
//
// busybox・vi・snake が実際に吐くものだけ実装する。全 VT100 は追わない:
//   - カーソル移動 (CUP/CUU/CUD/CUF/CUB)、絶対位置 `\x1b[y;xH`
//   - 消去 (ED `\x1b[2J`、EL `\x1b[K`)
//   - SGR (色・太字・反転)  ・ カーソル表示/非表示 `\x1b[?25l/h`
//   - `\r` `\n` `\b` `\t` と、80桁での折り返し

const CELL_W = 9;
const CELL_H = 16;
const FONT = '13px ui-monospace, Menlo, monospace';

// xterm の16色 (0-7 通常、8-15 明色) を、VGA端末と同じ Homebrew 緑燐光に寄せる。
// **既定色 (7) と明るい白 (15) だけを緑にする**のが肝 — 全部緑にすると
// ゲストが色分けした情報 (ls の青いディレクトリ、エラーの赤) が読めなくなる
const PALETTE = [
  '#000000', '#cd0000', '#00cd00', '#cdcd00', '#3388ff', '#cd00cd', '#00cdcd', '#00ff00',
  '#7f7f7f', '#ff5555', '#55ff55', '#ffff55', '#5555ff', '#ff55ff', '#55ffff', '#7cff7c',
];

export class AnsiTerminal {
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
    this.cols = opts.cols ?? 80;
    this.rows = opts.rows ?? 24;
    canvas.width = this.cols * CELL_W;
    canvas.height = this.rows * CELL_H;

    // 各セル: {ch, fg, bg, bold, inv}
    this.grid = null;
    this.reset();

    // パーサの状態
    this.state = 'text'; // 'text' | 'esc' | 'csi'
    this.params = '';
    this.dirty = true;

    // 入力: キーが来たら onData(str) を呼ぶ (端末→ゲスト)
    this.onData = null;
    this._utf8 = []; // 受信UTF-8の途中バイト

    canvas.tabIndex = 0;
    canvas.style.outline = 'none';
    canvas.addEventListener('keydown', (e) => this._key(e));
    canvas.addEventListener('paste', (e) => {
      const t = e.clipboardData?.getData('text');
      if (t) this.onData?.(t);
      e.preventDefault();
    });
  }

  reset() {
    this.grid = Array.from({ length: this.rows }, () =>
      Array.from({ length: this.cols }, () => this._blank()),
    );
    this.cx = 0;
    this.cy = 0;
    this.fg = 7;
    this.bg = 0;
    this.bold = false;
    this.inv = false;
    this.cursorVisible = true;
    this.dirty = true;
  }

  _blank() {
    return { ch: ' ', fg: 7, bg: 0, bold: false, inv: false };
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
        if ((b >= 0x30 && b <= 0x3f)) {
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
      this.grid.shift();
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

  // ---- 描画 ----

  render() {
    if (!this.dirty) return;
    this.dirty = false;
    const ctx = this.ctx;
    ctx.fillStyle = '#000';
    ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.textBaseline = 'top';
    for (let y = 0; y < this.rows; y++) {
      for (let x = 0; x < this.cols; x++) {
        const cell = this.grid[y][x];
        let fg = cell.fg, bg = cell.bg;
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
    // カーソル
    if (this.cursorVisible) {
      ctx.fillStyle = '#23ff18'; // カーソルも燐光色 (VGA端末と同じ)
      ctx.fillRect(this.cx * CELL_W, this.cy * CELL_H + CELL_H - 2, CELL_W, 2);
    }
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
      this.onData?.(s);
      e.preventDefault();
    }
  }
}
