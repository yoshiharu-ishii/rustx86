// ページの入口。
//
// **ここが繋ぎ役**である。やることは3つしかない:
//   1. ディスクイメージを手に入れる (同じ場所から取る / ドロップしてもらう)
//   2. [`Machine`](./machine.js) を作って回す
//   3. 機械の画面を [`Terminal`](./terminal.js) へ、端末のキーを機械へ
//
// 機械は画面を知らず、端末は機械を知らない。互いを知っているのはここだけなので、
// 別のOSを載せても、端末を差し替えても、直すのはこのファイルで済む。

import { loadWasm, charset, onPanic, Machine } from './machine.js';
import { Terminal } from './terminal.js';
import { MACHINES, byGroup, statusLabel } from './machines.js';
import { Debugger } from './debugger.js?v=15';
import { mountBench } from './bench.js?v=5';

const $ = id => document.getElementById(id);
const term = new Terminal($('screen'), { scrollback: 1000 });

let machine = null;
/** 最後に起動したイメージ。再起動に使う */
let lastImage = null;

// ---------- スナップショット ----------
//
// 機械の状態は Rust 側がコンパクトなバイナリで書き出す (連長圧縮済み)。
// **JSONで束ねるのはここの仕事**で、いつ・何のイメージから取ったのかという
// 人間向けの情報を添える。中身をJSONの数値配列にすると1MBが数MBに膨れるので、
// バイナリは Base64 の文字列1本にして入れる。

const SNAP_FORMAT = 'rustx86-snapshot';
const SNAP_KEY = 'rustx86.snapshot';

/**
 * gzip をかけてから Base64 にする。
 *
 * 連長圧縮 (Rust側) が潰せるのは**ゼロの海**だけで、ディスクイメージのような
 * 実データには効かない。1.44MBのフロッピーがそのまま乗ると 3.5MB になり、
 * localStorage (5MB程度) に1個しか入らなかった。
 * 汎用の圧縮を通すと数分の1になる。
 */
async function gzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

async function gunzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

const toBase64 = bytes => {
  let s = '';
  // 一度に渡すと引数が多すぎて落ちるので刻む
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
};

const fromBase64 = b64 => Uint8Array.from(atob(b64), c => c.charCodeAt(0));

async function snapshotJson(name) {
  const bytes = machine.saveState();
  const packed = await gzip(bytes);
  return JSON.stringify({
    format: SNAP_FORMAT,
    version: 1,
    created: new Date().toISOString(),
    image: name ?? 'unknown',
    bytes: bytes.length,
    encoding: 'gzip+base64',
    state: toBase64(packed),
  });
}

async function applySnapshotJson(text) {
  const o = JSON.parse(text);
  if (o.format !== SNAP_FORMAT) throw new Error('rustx86 のスナップショットではない');
  let bytes = fromBase64(o.state);
  if (o.encoding === 'gzip+base64') bytes = await gunzip(bytes);
  machine.loadState(bytes);
  return o;
}

function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

/** ツールバーの表示を実際の状態に合わせる */
function syncControls() {
  const on = !!machine;
  // ベンチには端末が無いので、端末向けの操作は伏せる。
  // デバッガだけは**どちらでも使える**
  for (const id of ['boot', 'pause', 'snap', 'restore', 'snapfile', 'save']) {
    $(id).hidden = !!bench;
  }
  // 配列の選択と MIPS の表示も端末のもの
  $('layout').closest('.sel').hidden = !!bench;
  $('gauge').hidden = !!bench;
  $('debug').disabled = !on && !bench;
  if (bench) return;
  $('pause').disabled = !on;
  $('pause').textContent = machine?.paused ? '再開' : '一時停止';
  $('boot').disabled = !lastImage;
  $('snap').disabled = !on;
  $('snapfile').disabled = !on;
  $('restore').disabled = !on || !localStorage.getItem(SNAP_KEY);
}

/** 最後に起動したイメージの名前。スナップショットに添える */
let lastLabel = '';

function boot(image, label) {
  lastLabel = label;
  machine?.stop();
  try {
    machine = new Machine(image);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  lastImage = image;
  term.reset();
  machine.onFrame = (cells, row, col, redraw) => {
    term.sample(cells, row, col);
    if (redraw) term.draw();
    advanceScript();
  };
  // デバッガが止めたら、理由を子ウインドウへ。**開いていなくても状態表示は出す**
  machine.onDebugStop = (why) => {
    dbg.onStop(why);
    setStatus(`デバッガが止めた: ${why}`);
    syncControls();
  };
  // 物理キーはそのまま、貼り付けはASCIIとして送る
  term.onKey = (code, down) => machine.key(code, down);
  term.onChar = ch => machine.typeChar(ch);
  term.onPaste = text => machine.paste(text);

  // 動作確認用の窓口。手元で開いているときだけ出す
  if (['localhost', '127.0.0.1'].includes(location.hostname)) {
    window.__machine = machine;
    window.__term = term;
  }

  machine.start();
  setStatus(`${label} を起動中… 画面をクリックするとキー入力できます`);
  $('screen').focus();
  syncControls();
}

/** 1秒に2回、速度と履歴の深さを出す。教材として「今どれくらい出ているか」を見せる */
setInterval(() => {
  if (!machine) return;
  const parts = [];
  parts.push(machine.paused ? '停止中' : `${machine.mips.toFixed(0)} MIPS`);
  if (term.scrollback.length) parts.push(`履歴 ${term.scrollback.length}行`);
  if (term.offset) parts.push(`▲${term.offset}行前`);
  $('gauge').textContent = parts.join('   ');
}, 500);

// --- キーボード配列 ---
//
// 既定はJIS。**スキャンコードはキーの位置なので配列とは無関係**だが、
// ゲスト (ELKS) はUS配列の対応表しか持たないため、JIS配列の実機では
// 見たままの文字が入らない。JISのときは位置ではなく文字を送って辻褄を合わせる。

const LAYOUT_KEY = 'rustx86.layout';
const layoutSel = $('layout');
term.layout = localStorage.getItem(LAYOUT_KEY) || 'jp';
layoutSel.value = term.layout;
layoutSel.addEventListener('change', () => {
  term.layout = layoutSel.value;
  localStorage.setItem(LAYOUT_KEY, term.layout);
  $('screen').focus();
});

// --- 操作 ---

// --- デバッガの子ウインドウ ---
//
// Emulator は再起動のたびに作り直されるので、**参照を握らせず毎回聞かせる**。
// 握らせると再起動後に古い機械を覗き続けることになる
/** ベンチを選んでいるときの取っ手 (選んでいなければ null) */
let bench = null;

// **いま動いている機械**を見せる。OSとベンチで持ち主が違うので、
// 参照を握らず毎回聞く
const dbg = new Debugger({
  emu: () => (bench ? bench.emu : machine?.emu) ?? null,
  isPaused: () => (bench ? bench.paused : machine?.paused) ?? true,
  setPaused: (v) => {
    if (bench) {
      bench.setPaused(v);
      return;
    }
    if (!machine) return;
    if (v) machine.stop();
    else machine.start();
    syncControls();
  },
});

$('debug').addEventListener('click', async () => {
  // ベンチのデバッグ機械は求められて初めて作る (計測だけしたい人に costs を払わせない)
  if (bench) await bench.ensureDebugMachine();
  dbg.show();
  dbg.reset();
});

$('pause').addEventListener('click', () => {
  if (!machine) return;
  if (machine.paused) machine.start();
  else machine.stop();
  syncControls();
  $('screen').focus();
});

$('boot').addEventListener('click', () => {
  if (lastImage) boot(lastImage, 'ディスク');
});

$('snap').addEventListener('click', async () => {
  if (!machine) return;
  try {
    const json = await snapshotJson(lastLabel);
    localStorage.setItem(SNAP_KEY, json);
    setStatus(`状態を保存した (${(json.length / 1024).toFixed(0)} KB、この端末に残る)`);
    syncControls();
  } catch (e) {
    // localStorage は数MBで埋まる。落ちた理由を隠さない
    setStatus(`保存できない: ${e.message}`, true);
  }
});

$('restore').addEventListener('click', async () => {
  const json = localStorage.getItem(SNAP_KEY);
  if (!machine || !json) return;
  try {
    const o = await applySnapshotJson(json);
    term.reset();
    setStatus(`${o.created} の状態に戻した (${o.image})`);
    $('screen').focus();
  } catch (e) {
    setStatus(`復元できない: ${e.message}`, true);
  }
});

$('snapfile').addEventListener('click', async () => {
  if (!machine) return;
  const blob = new Blob([await snapshotJson(lastLabel)], { type: 'application/json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = `rustx86-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
  a.click();
  URL.revokeObjectURL(a.href);
});

$('save').addEventListener('click', () => {
  const blob = new Blob([term.allLines().join('\n')], { type: 'text/plain' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  // 箱ではなく**中身**の名前を付ける。載せたOSが何であれ辻褄が合う
  a.download = `${(lastLabel || 'console').replace(/\.\w+$/, '')}.log`;
  a.click();
  URL.revokeObjectURL(a.href);
});

// --- ディスクイメージの受け取り ---

const consoleBox = $('console');
for (const ev of ['dragenter', 'dragover']) {
  consoleBox.addEventListener(ev, e => {
    e.preventDefault();
    consoleBox.classList.add('drop');
  });
}
for (const ev of ['dragleave', 'drop']) {
  consoleBox.addEventListener(ev, () => consoleBox.classList.remove('drop'));
}
consoleBox.addEventListener('drop', async e => {
  e.preventDefault();
  const f = e.dataTransfer?.files?.[0];
  if (!f) return;
  // 落とされたものがスナップショットならそこへ戻る。ディスクなら起動する
  if (f.name.endsWith('.json')) {
    if (!machine) {
      setStatus('先にディスクイメージを起動してください', true);
      return;
    }
    try {
      const o = await applySnapshotJson(await f.text());
      term.reset();
      setStatus(`${f.name} の状態に戻した (${o.image}、${o.created})`);
      $('screen').focus();
    } catch (err) {
      setStatus(`復元できない: ${err.message}`, true);
    }
    return;
  }
  setStatus(`${f.name} を読み込み中…`);
  boot(new Uint8Array(await f.arrayBuffer()), f.name);
});

// ---------- 起動シナリオ ----------
//
// **選んだらプロンプトまで自動で進む。**
//
// FreeDOSの起動フロッピーは本来インストーラを立ち上げる。素のプロンプトへ降りるには
// 起動時に F5 を打ち、聞かれるシェルの場所を答える必要がある — DOSの定石だが、
// 知らなければ辿り着けない。「押す瞬間を当てて長いパスを打て」は動くとは言えない。
//
// 画面に出る文字列を合図にして進める。**何命令目で打つかではなく画面を見てから打つ**のは、
// 起動にかかる時間が環境で変わるためで、人間が画面を見て打つのと同じ手順である。

/** 実行中のシナリオ。`{steps, at, queue}` */
let script = null;

function startScript(steps) {
  script = steps?.length ? { steps, at: 0, queue: [] } : null;
}

function advanceScript() {
  if (!script || !machine) return;
  // 打ちかけの文字が残っていれば、**1フレームに1文字だけ**送る。
  // まとめて送るとBIOSの待ち行列 (16枠) がゲストの読み出しより速く埋まって取りこぼす
  if (script.queue.length) {
    machine.typeChar(script.queue.shift());
    return;
  }
  const step = script.steps[script.at];
  if (!step) {
    script = null;
    return;
  }
  if (!term.screenText().includes(step.when)) return;

  script.at++;
  if (typeof step.send === 'string') {
    script.queue = [...step.send];
  } else if (step.send?.scancodes) {
    machine.sendScancodes(step.send.scancodes);
  }
  setStatus(
    script.at < script.steps.length
      ? `自動で進めています (${script.at}/${script.steps.length})…`
      : '自動起動が終わりました。画面をクリックすると打てます',
  );
}

// ---------- マシン選択 ----------
//
// **一覧は [`machines.js`](./machines.js) が持つデータで、ここは描画と起動だけ。**
// 未実装のものも灰色で並べる — この教材は「どこまで行けて、なぜ止まるか」が
// 見えている方が価値があるので、ロードマップを画面に出しておく。

/** 今選んでいるマシン */
let current = null;

function renderMachines() {
  const nav = $('machines');
  nav.textContent = '';
  for (const [group, list] of byGroup()) {
    const h = document.createElement('h2');
    h.textContent = group;
    nav.append(h);
    for (const m of list) {
      const b = document.createElement('button');
      b.dataset.id = m.id;
      b.disabled = m.status === 'todo';
      b.title = m.note ?? '';
      b.innerHTML =
        `<span class="name"><span class="dot ${m.status}"></span>${m.label}</span>` +
        `<span class="meta">${m.sub ?? ''}${m.sub ? ' · ' : ''}${statusLabel(m.status)}</span>`;
      b.querySelector('.meta').style.display = 'block';
      b.addEventListener('click', () => select(m));
      nav.append(b);
    }
  }
}

function markCurrent(id) {
  for (const b of $('machines').querySelectorAll('button')) {
    b.setAttribute('aria-current', String(b.dataset.id === id));
  }
}

/** 選ばれたマシンの説明と取得先を出す */
function showNote(m) {
  const el = $('machineNote');
  el.textContent = '';
  if (!m) return;
  const note = document.createElement('span');
  note.textContent = m.note ?? '';
  el.append(note);
  if (m.source) {
    el.append(' 取得先: ');
    const a = document.createElement('a');
    a.href = m.source;
    a.textContent = m.sourceLabel ?? m.source;
    a.target = '_blank';
    a.rel = 'noreferrer';
    el.append(a);
    if (m.file) el.append(` (${m.file} としてこのページと同じ場所に置く)`);
  }
}

async function select(m) {
  // **切り替えたら前の機械は捨てる。**
  //
  // OSもベンチも同じCPUを回している。片方を残したまま次を始めると、
  // 裏で走り続けて画面にも出ず、計測を汚し、デバッガは古い機械を覗く。
  // 「選び直したらまっさらから」を守る
  machine?.stop();
  bench?.destroy();
  bench = null;
  $('benchPane').hidden = true;
  $('screen').hidden = false;

  current = m;
  markCurrent(m.id);
  showNote(m);

  if (m.kind === 'bench') {
    machine = null;
    lastImage = null;
    term.reset();
    $('screen').hidden = true;
    $('benchPane').hidden = false;
    bench = mountBench($('benchPane'), {
      onStop: (why) => dbg.onStop(why),
    });
    setStatus('実行速度ベンチ。「計測する」で始める');
    syncControls();
    // 見ている機械が入れ替わったので、前の残りかすを捨てる
    dbg.reset();
    return;
  }

  await bootFromUrl(m);
  startScript(m.script);
  dbg.reset();
}

async function bootFromUrl(m = current) {
  if (!m?.image) return;
  setStatus(`${m.label} を取得中…`);
  try {
    const r = await fetch(m.image);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    term.reset();
    boot(new Uint8Array(await r.arrayBuffer()), m.image.replace('./', ''));
  } catch (e) {
    setStatus(
      `${m.image.replace('./', '')} が見つからない (${e.message})。イメージをここにドロップしてください`,
      true,
    );
  }
}

/** 置いてあるイメージのうち、最初に見つかったものを選ぶ */
async function selectFirstAvailable() {
  for (const m of MACHINES) {
    if (!m.image) continue;
    const head = await fetch(m.image, { method: 'HEAD' }).catch(() => null);
    if (head?.ok) {
      await select(m);
      return true;
    }
  }
  return false;
}

// 読み込みに失敗すると「読み込み中…」のまま黙って止まる。
// 何が起きたか分からないのが一番困るので、必ず画面に出す。
//
// ただし**wasmのパニックだけは例外**である。パニックは必ず
// `RuntimeError: unreachable` として後から飛んでくるが、それは中身の無い包装で、
// 本当の理由 (`unimplemented opcode 0x66 at ...`) は先にフックが受け取っている。
// **後から来る包装で上書きしてはいけない。**
let panicMessage = null;

function reportError(text) {
  if (panicMessage && /unreachable|wasm/i.test(text)) return;
  setStatus(`エラー: ${text}`, true);
}
window.addEventListener('error', e => reportError(e.message));
window.addEventListener('unhandledrejection', e => reportError(String(e.reason)));

try {
  await loadWasm();
  // 文字の表はwasmが読めてから受け取る。**CLIの確認表示と同じ表**なので、
  // 「CLIでは出るのにブラウザでは化ける」が起きない
  term.charset = [...charset()];
  // **パニックの中身を画面に出す。** 「何が未実装で止まったか」が
  // このエミュレータで一番役に立つ情報なので、コンソールに埋もれさせない
  onPanic(msg => {
    const m = /unimplemented opcode (\S+) at (\S+)/.exec(msg);
    const detail = m
      ? `未実装の命令 ${m[1]} で停止 (${m[2]})`
      : msg.replace(/^panicked at [^:]+:\d+:\d+:\s*/, '');
    panicMessage = detail;
    setStatus(`停止: ${detail} — 画面は倒れた瞬間のまま`, true);
  });
  renderMachines();
  setStatus('左からマシンを選ぶか、ディスクイメージをここにドロップしてください');
  await selectFirstAvailable();
  syncControls();
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
