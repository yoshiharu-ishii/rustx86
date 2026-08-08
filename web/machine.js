// エミュレートされた機械を持って、回す。
//
// Rust側の `Machine` (core/src/lib.rs) に対応する層で、**画面もキーも知らない**。
// 外に見せるのは「今の画面 (VRAMの生バイト)」「カーソル位置」「キーを押された」
// という素の口だけで、それをどう見せるかは [`Terminal`](./terminal.js) の仕事。
//
// ここが持っているのは**時間の刻み方**である。1フレームで何命令進めるか、
// 画面を何命令ごとに覗くか — 実機なら水晶が決めることを、ブラウザでは
// ここが決める。

import init, { Emulator, cp437_table, install_panic_hook } from './pkg/rustx86_wasm.js?v=7';

/** 1フレームで進める命令数。実機の8086より遥かに速いが、起動を待たずに済む */
const INSTRUCTIONS_PER_FRAME = 3_000_000;

/**
 * 何命令ごとに画面を覗くか。
 *
 * まとめて進めてから覗くと、その間に何十行もスクロールしていて流れた行を
 * 追えない。逆に細かすぎると重い。**覗くのは安く、描くのは高い**ので、
 * 覗くほうだけ細かく回す。
 */
const CHUNK = 6_000;

let wasmMemory = null;

// wasmのインポート解決はインスタンス化の時点で行われるので、**init より前に**置く
globalThis.__rustx86_panic ??= msg => console.error(msg);

/** WASMを読み込む。ページの最初に一度だけ */
export async function loadWasm() {
  // glue と .wasm 本体の両方にバージョンを付ける。
  // 片方だけ新しいと「その関数は無い」と言われる (実際に踏んだ)
  const wasm = await init({
    module_or_path: new URL('./pkg/rustx86_wasm_bg.wasm?v=7', import.meta.url),
  });
  wasmMemory = wasm.memory;
  // **パニックの中身を拾えるようにする。** これが無いとJS側には
  // `RuntimeError: unreachable` としか見えず、「何が未実装で止まったか」という
  // このエミュレータで一番役に立つ情報が消える
  install_panic_hook();
}

/**
 * パニックが起きたときに呼ばれる関数を差し替える。
 *
 * wasm側のフックは `globalThis.__rustx86_panic` を呼ぶ。パニック後の
 * インスタンスには触れないので、**受け取れるのはこの一度きり**である。
 */
export function onPanic(fn) {
  globalThis.__rustx86_panic = fn;
}

/** VRAMの1バイトを何の絵にするかの表 (CP437)。**Rust側と同じものを使う** */
export function charset() {
  return cp437_table();
}

export class Machine {
  /** @param {Uint8Array} image ディスクイメージ (フロッピー) */
  constructor(image) {
    this.emu = Emulator.from_disk(image);
    this.running = false;
    /** 直前のカーソル位置。動いたかどうかの判定に使う */
    this.lastCursor = [-1, -1];
    /** 直近の実行速度 (MIPS)。教材として「今どれくらい出ているか」を見せる */
    this.mips = 0;
    this.executed = 0;
    this.lastMeasure = performance.now();
    /** 画面が変わったときに呼ばれる。(cells, cursorRow, cursorCol, redraw) => void */
    this.onFrame = null;
  }

  /** テキストVRAMをそのまま見る (コピーしない) */
  vram() {
    return new Uint8Array(
      wasmMemory.buffer,
      this.emu.text_vram_ptr(),
      this.emu.text_vram_len(),
    );
  }

  cursor() {
    return [this.emu.cursor_row(), this.emu.cursor_col()];
  }

  /** 物理キーの上げ下げを渡す。文字への変換はゲストのOSがやる */
  key(code, down) {
    return this.emu.key(code, down);
  }

  /** 1文字を打ち込む (JP配列のとき)。押して離すまでをRust側が組み立てる */
  typeChar(ch) {
    this.emu.type_text(ch);
  }

  /** 文字列を打ち込む (貼り付け用)。物理キーに直せないのでASCIIで送る */
  paste(text) {
    this.emu.type_text(text.replace(/\r\n?/g, '\n'));
  }

  get paused() {
    return !this.running;
  }

  /**
   * 状態をまるごと書き出す (CPU・装置・メモリ・ディスク)。
   *
   * 数MBを確保するのでwasmのリニアメモリが伸びることがある。伸びると
   * **それまでにJS側へ渡した参照は無効になる**ので、書き出したあとは
   * 新しい参照で描き直させる。
   */
  saveState() {
    const bytes = this.emu.save_state();
    this.onFrame?.(this.vram(), ...this.cursor(), true);
    return bytes;
  }

  /** 書き出した状態へ戻す。時計の基準も入れ直す */
  loadState(bytes) {
    this.emu.load_state(bytes);
    this.lastCursor = [-1, -1];
    this.lastMeasure = performance.now();
    this.executed = 0;
    this.onFrame?.(this.vram(), ...this.cursor(), true);
  }

  start() {
    if (this.running) return;
    this.running = true;
    this.#schedule();
  }

  stop() {
    this.running = false;
  }

  /**
   * 次のフレームを予約する。
   *
   * requestAnimationFrame はタブが非表示だと発火しない。それだけに頼ると
   * 裏に回した瞬間にゲストOSの時間が止まる。タイマで回して動き続けさせる。
   */
  #schedule() {
    if (document.hidden) setTimeout(() => this.#frame(), 16);
    else requestAnimationFrame(() => this.#frame());
  }

  #frame() {
    if (!this.running) return;
    let changed = false;
    this.executed += INSTRUCTIONS_PER_FRAME;
    const now = performance.now();
    if (now - this.lastMeasure >= 500) {
      this.mips = this.executed / (now - this.lastMeasure) / 1000;
      this.executed = 0;
      this.lastMeasure = now;
    }
    for (let done = 0; done < INSTRUCTIONS_PER_FRAME; done += CHUNK) {
      try {
        this.emu.run_slice(CHUNK);
      } catch (e) {
        // wasmがパニックした。**ここで止めて、描き直さずに抜ける。**
        //
        // 描き直すと最後の絵が消えてしまう。「どこまで行けたか」が見えることが
        // このエミュレータの一番の情報なので、画面は倒れた瞬間のまま残す。
        // パニックの中身は machine.js のフックが先に受け取っている。
        this.running = false;
        this.crashed = true;
        this.onCrash?.(e);
        return;
      }
      const [row, col] = this.cursor();
      // **カーソルが動いただけでも描き直す。**
      // viで矢印を押してもVRAMは変わらないので、文字の変化だけを見ていると
      // カーソルが画面上で固まったままになる (これでviが使い物にならなかった)。
      const moved = row !== this.lastCursor[0] || col !== this.lastCursor[1];
      if (this.emu.take_vram_dirty() || moved) {
        changed = true;
        this.lastCursor = [row, col];
        // 覗くのは細かく (スクロールを取りこぼさないため)
        this.onFrame?.(this.vram(), row, col, false);
      }
    }
    // 描かせるのは1フレームに1回だけ
    if (changed) this.onFrame?.(this.vram(), ...this.cursor(), true);
    this.#schedule();
  }
}
