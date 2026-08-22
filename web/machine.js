// エミュレートされた機械を持って、回す。
//
// Rust側の `Machine` (core/src/lib.rs) に対応する層で、**画面もキーも知らない**。
// 外に見せるのは「今の画面 (VRAMの生バイト)」「カーソル位置」「キーを押された」
// という素の口だけで、それをどう見せるかは [`Terminal`](./terminal.js) の仕事。
//
// ここが持っているのは**時間の刻み方**である。1フレームで何命令進めるか、
// 画面を何命令ごとに覗くか — 実機なら水晶が決めることを、ブラウザでは
// ここが決める。

import init, { Emulator, cp437_table, install_panic_hook } from './pkg/rustx86_wasm.js';

/**
 * 1ゲストミリ秒に相当する仮想命令数 (linux-worker.js と同じ勘定)。
 * PITの入力 1.193182 MHz × 64命令/クロック ÷ 1000 ≒ 76,364。
 *
 * フレームの予算は**実時間で流れたぶんだけ** — つまりゲストの時計 = 実時間。
 * 以前は「1フレーム3M命令」の固定予算で、これは 60fps で 180M/s ≒
 * **実時間の2.4倍速の時計**だった。ホストが遅いうちは上限に届かず
 * 気づかなかったが、デコードキャッシュで速くなった途端、テトリスの駒が
 * 目で追えない速さで落ちた (実際になった)。ゲームのテンポは
 * ゲストのタイマが決めるもので、エミュレータが速くなっても変わってはいけない
 */
const INSTR_PER_GUEST_MS = (1_193_182 * 64) / 1000;

/** 1フレーム予算の上限。裏タブから戻った直後などの巨大なdtを
    一気に追いつかせると操作不能の早回しになるので、50msぶんで頭打ち */
const MAX_FRAME_BUDGET = Math.round(50 * INSTR_PER_GUEST_MS);

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
  // キャッシュ対策はここには無い。**serve.py が no-store を送る**ので、
  // ファイルは毎回取り直される。以前は ?v=番号 を全ファイルに付けて手で
  // 上げていたが、番号がずれると「その関数は無い」と言われる事故のほうが
  // 多かった (実際に踏んだ)。番号を消せば、揃える作業ごと消える
  const wasm = await init({
    module_or_path: new URL('./pkg/rustx86_wasm_bg.wasm', import.meta.url),
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
    // ISO 9660 (セクタ 16 に "CD001") なら El Torito で起動、それ以外はフロッピー
    const iso = image.length > 0x8006 && image[0x8001] === 0x43 && image[0x8002] === 0x44 && image[0x8003] === 0x30 && image[0x8004] === 0x30 && image[0x8005] === 0x31;
    this.emu = iso ? Emulator.from_iso(image) : Emulator.from_disk(image);
    /** PCI 付きの機械か (ISO は 32bit PC に載る)。NIC の名前 (RTL8029 / NE2000) の根拠 */
    this.pci = iso;
    // RTCを実時刻に合わせる (DOSのDATE/TIMEが今日を示す)。
    // CIのヘッドレス検証は注入しないので、そちらは決定的なまま
    this.emu.set_rtc_unix(Date.now() / 1000);
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

  /** BIOS のハードディスク (INT 13h ドライブ 0x80 = C:) を挿す。起動直後に呼ぶ —
      DOS はプロンプトまでに C: を数えるので、後から挿しても見えない */
  hddAttach(image) {
    this.emu.hdd_attach(image);
  }

  /** テキストVRAMをそのまま見る (コピーしない) */
  vram() {
    return new Uint8Array(
      wasmMemory.buffer,
      this.emu.text_vram_ptr(),
      this.emu.text_vram_len(),
    );
  }

  /** mode 13h フレームバッファ (320x200、1バイト=色番号) をそのまま見る。
      **ビューは毎回作り直す** — wasmメモリがgrowで移動すると古いビューは死ぬ */
  fb() {
    return new Uint8Array(wasmMemory.buffer, this.emu.fb_ptr(), this.emu.fb_len());
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

  /** 生のスキャンコードを流す (ファンクションキーなど文字を持たないキー) */
  sendScancodes(codes) {
    for (const c of codes) this.emu.send_scancode(c);
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
    const now = performance.now();
    // 予算 = 前のフレームから実時間で流れたぶんの仮想時間。
    // ゲストの時計を実時間に繋ぎ止める (速いホストでも遅いホストでも同じ速さ)
    const dt = Math.min(50, this.lastFrame ? now - this.lastFrame : 16);
    this.lastFrame = now;
    const budget = Math.min(MAX_FRAME_BUDGET, Math.max(CHUNK, Math.round(dt * INSTR_PER_GUEST_MS)));
    this.executed += budget;
    if (now - this.lastMeasure >= 500) {
      this.mips = this.executed / (now - this.lastMeasure) / 1000;
      this.executed = 0;
      this.lastMeasure = now;
    }
    for (let done = 0; done < budget; ) {
      try {
        // **実際に進んだ量で数える。** run_slice は送信フレームが出ると
        // 予算を使い切る前に戻る (遅延を削るための早出し) ので、
        // 頼んだ量で足すと進んでいない分まで勘定してしまう。
        // 0 が返るのは「起こせない眠り」— そこで回り続けないよう抜ける
        const ran = this.emu.run_slice(Math.min(CHUNK, budget - done));
        if (ran <= 0) break;
        done += ran;
        // デバッガが止めたら、そこで走るのをやめる。**画面は描き直す** —
        // 止まった瞬間の絵を見たいので (パニックのときと違い、続きがある)
        if (this.emu.is_stopped()) {
          this.running = false;
          this.onFrame?.(this.vram(), ...this.cursor(), true);
          this.onDebugStop?.(this.emu.take_stop());
          return;
        }
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
      if (this.emu.video_mode() === 0x13) {
        // グラフィックモード中はテキストを描かない (最後にFBを一枚描く)。
        // dirtyは消費しておく — テキストへ戻った瞬間の描き直しは
        // モード切替が立てるdirtyが担う
        this.emu.take_vram_dirty();
      } else if (this.emu.take_vram_dirty() || moved) {
        changed = true;
        this.lastCursor = [row, col];
        // 覗くのは細かく (スクロールを取りこぼさないため)
        this.onFrame?.(this.vram(), row, col, false);
      }
    }
    // 予算の大半が早送り (HLT) なら、この機械は暇 — ゲージに「アイドル」と出す
    const skipped = this.emu.take_idle_skipped();
    this.idle = skipped > budget / 2;

    // PCスピーカー (画面と同じポーリングの分業。鳴らすのは main.js)
    this.onTone?.(this.emu.speaker_tone());
    // OPL2: 描き手が実時間ぶんのサンプルを引き出す (core の合成器)
    this.onOpl?.((rate, n) => this.emu.opl_render(rate, n));

    // NIC。フレームの送り出しと注入はスライス境界でまとめて行う
    this.netlink?.pump(this.emu);

    // 描かせるのは1フレームに1回だけ。グラフィックモードはdirtyを見ない —
    // FBはただのRAMで通知が無いので、毎フレーム全読みして描く (64KB。設計は
    // docs/roadmap.md 6a: フック式はゲストの画素ストアをJITから弾いてしまう)
    if (this.emu.video_mode() === 0x13) {
      this.onGfxFrame?.(this.fb(), this.emu.palette());
    } else if (changed) this.onFrame?.(this.vram(), ...this.cursor(), true);
    this.#schedule();
  }
}
