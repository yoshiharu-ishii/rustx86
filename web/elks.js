// ELKS をブラウザで動かす配線。
//
// **端末の仕事は terminal.js が持つ。** ここがやるのは3つだけ:
//   1. ディスクイメージを読んでエミュレータを作る
//   2. 毎フレーム一定命令数だけ進める
//   3. 画面 (VRAM) とキー入力を端末に繋ぐ
//
// エミュレータ側に画面やキーの都合を持ち込まないので、
// CLIから動かすとき (core/examples/boot.rs) と同じ口をそのまま使っている。

// wasmの取り込みにもバージョンを付ける。
// ブラウザによってはサーバーの no-store を無視してキャッシュを返し、
// 古い wasm のまま「そんな関数は無い」と言われる (実際に踏んだ)。
// wasm-pack を再ビルドしたらここを上げる。
import init, { Emulator } from './pkg/rustx86_wasm.js?v=5';
import { Terminal } from './terminal.js';

const $ = id => document.getElementById(id);
const term = new Terminal($('screen'), { scrollback: 1000 });

/** 1フレームで進める命令数。実機の8086より遥かに速いが、起動を待たずに済む */
const INSTRUCTIONS_PER_FRAME = 3_000_000;
/**
 * 何命令ごとに画面を見るか。
 *
 * まとめて進めてから見ると、その間に何十行もスクロールしていて流れた行を
 * 追えない。逆に細かすぎると重い。読み取りは安く描画は高いので、
 * 読み取りだけ細かく回す。
 */
const CHUNK = 6_000;

let emu = null;
let running = false;
let wasmMemory = null;

function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

/** wasmメモリ上のテキストVRAMをそのまま見る (コピーしない) */
const vram = () => new Uint8Array(wasmMemory.buffer, emu.text_vram_ptr(), emu.text_vram_len());

/**
 * 次のフレームを予約する。
 *
 * requestAnimationFrame はタブが非表示だと発火しない。それだけに頼ると
 * 裏に回した瞬間にゲストOSが止まる。タイマで回して動き続けるようにする。
 */
function scheduleFrame() {
  if (document.hidden) setTimeout(frame, 16);
  else requestAnimationFrame(frame);
}

function frame() {
  if (!running) return;
  let dirty = false;
  for (let done = 0; done < INSTRUCTIONS_PER_FRAME; done += CHUNK) {
    emu.run_slice(CHUNK);
    if (emu.take_vram_dirty()) {
      dirty = true;
      // 取り込みは細かく (スクロールを取りこぼさないため)
      term.sample(vram(), emu.cursor_row(), emu.cursor_col());
    }
  }
  // 描画は1フレームに1回だけ (高いので)
  if (dirty) term.draw();
  scheduleFrame();
}

function bootWith(bytes) {
  try {
    emu = Emulator.from_disk(bytes);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  term.reset();
  // キーは**物理キーの識別子のまま**渡す。文字への変換はゲストのOSがやる
  term.onKey = (code, down) => emu.key(code, down);
  window.__emu = emu; // 動作確認用
  window.__term = term;

  running = true;
  setStatus('起動中… 画面をクリックするとキー入力できます');
  $('screen').focus();
  scheduleFrame();
}

$('save').addEventListener('click', () => {
  const blob = new Blob([term.allLines().join('\n')], { type: 'text/plain' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'elks.log';
  a.click();
  URL.revokeObjectURL(a.href);
});

$('file').addEventListener('change', async e => {
  const f = e.target.files?.[0];
  if (!f) return;
  setStatus(`${f.name} を読み込み中…`);
  bootWith(new Uint8Array(await f.arrayBuffer()));
});

async function bootFromUrl() {
  setStatus('fd1440.img を取得中…');
  try {
    const r = await fetch('./fd1440.img');
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    bootWith(new Uint8Array(await r.arrayBuffer()));
  } catch (e) {
    setStatus(`fd1440.img が見つからない (${e.message})。ファイルを選んでください`, true);
  }
}

$('boot').addEventListener('click', bootFromUrl);

// 読み込みに失敗すると「読み込み中…」のまま黙って止まる。
// 何が起きたか分からないのが一番困るので、必ず画面に出す
window.addEventListener('error', e => setStatus(`エラー: ${e.message}`, true));
window.addEventListener('unhandledrejection', e => setStatus(`エラー: ${e.reason}`, true));

try {
  // wasm本体のURLも明示する。glueだけ新しくても、中身の .wasm が
  // キャッシュから来ると「その関数は無い」と言われる (実際に踏んだ)
  const wasm = await init({
    module_or_path: new URL('./pkg/rustx86_wasm_bg.wasm?v=5', import.meta.url),
  });
  wasmMemory = wasm.memory;
  $('boot').disabled = false;
  setStatus('ディスクイメージを選ぶと起動します');
  const head = await fetch('./fd1440.img', { method: 'HEAD' }).catch(() => null);
  if (head?.ok) await bootFromUrl();
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
