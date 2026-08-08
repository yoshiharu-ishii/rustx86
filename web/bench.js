// 実行速度ベンチ (WASM)。
//
// 計測は performance.now() で行う。Rust側の std::time::Instant は
// wasm32-unknown-unknown では動かない (プラットフォームの時計に触れない) ため。
//
// ここはエミュレータ本体とは独立した計測ツールとして置いている。
// 任意のブートセクタを、HLTまで or 命令数固定で流せる。

import init, { Emulator } from './pkg/rustx86_wasm.js?v=10';
import { Debugger, SlicedRunner } from './debugger.js?v=13';

const $ = id => document.getElementById(id);
const $status = $('status');
const $results = $('results');
const $tbody = $results.querySelector('tbody');
const $summary = $('summary');
const $run = $('run');
const $file = $('file');
const $runs = $('runs');
const $limit = $('limit');
const $untilHalt = $('untilHalt');

/** ファイル選択のラジオに連動して file input を有効化する */
for (const radio of document.querySelectorAll('input[name=src]')) {
  radio.addEventListener('change', () => {
    $file.disabled = radio.value !== 'file' || !radio.checked;
    if (!$file.disabled) $file.focus();
  });
}

/**
 * DOMの更新をブラウザに反映させてから重い処理へ入る。
 *
 * requestAnimationFrame は**タブが非表示だと発火しない**。それだけに頼ると
 * バックグラウンドのタブで計測が永久に止まる (実際に踏んだ) ので、
 * タイマーとの競争にして必ず進むようにする。
 */
const paint = () => Promise.race([
  new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))),
  new Promise(r => setTimeout(r, 50)),
]);

/** 選択中のワークロードを読む。埋め込みなら null (Emulator.bench() を使う) */
async function selectedSector() {
  const src = document.querySelector('input[name=src]:checked').value;
  if (src === 'builtin') return null;
  const f = $file.files?.[0];
  if (!f) throw new Error('ファイルが選ばれていない');
  const buf = new Uint8Array(await f.arrayBuffer());
  if (buf.length !== 512) {
    throw new Error(`ブートセクタは512バイトである必要がある (${buf.length}バイト)`);
  }
  return buf;
}

/**
 * 計測前の空転。捨てる実行なので結果は返さない。
 *
 * 段階が2つあるのは**原因を切り分けた名残**である。当初「1回目が遅いのは
 * wasmリニアメモリの初回確保のせい」と考えたが、確保だけを済ませる軽い空転では
 * まったく直らなかった (1回目 41.1 MIPS のまま)。確保は原因ではない。
 *
 * 実測で分かっているのは次の2点:
 *
 * - **軽い空転 (確保だけ) では直らない**。だから空転は1回まるごと走らせる
 * - **ネイティブでも同じ形が出る** (1回目 101.8、以降 107〜110)。
 *   ただし落ち込みは7%程度。ホスト側のCPU立ち上がり (周波数制御や
 *   コア割り当て) が少なくとも一部を占めている
 *
 * 追試の途中で「同じページでも間が空くとまた遅くなる」ように見えたが、
 * これは**タブが非表示になっていたこと**による汚染だった (`measure` 参照)。
 * 交絡を潰しきれていないので、残りの原因は特定できていないものとして扱う。
 */
function warmUp(mode, sector, limit) {
  if (mode === 'none') return;
  const emu = sector === null ? Emulator.bench() : new Emulator(sector);
  // 軽い: エミュレータを1個作って少しだけ回す。メモリ確保 (1MB + 64K) は
  //       ここで済む。**これでは直らない**ことが分かっているので、原因切り分け用
  // 完全: 1回まるごと走らせて捨てる。これは効く
  emu.run(mode === 'light' ? 1000 : limit);
}

/**
 * 1回分の計測。この間メインスレッドは止まる。
 *
 * タブが非表示だとブラウザがスロットリングをかけ、**値が半分以下になる**
 * (実測: 表示時 104〜106 MIPS に対し、非表示時 34〜50 MIPS)。
 * 前後で `document.hidden` を見て、汚染された測定に印を付ける。
 */
function measure(sector, limit) {
  const emu = sector === null ? Emulator.bench() : new Emulator(sector);
  const hiddenBefore = document.hidden;
  const t0 = performance.now();
  const n = emu.run(limit);
  const ms = performance.now() - t0;
  return {
    n, ms,
    halted: emu.halted(),
    hidden: hiddenBefore || document.hidden,
    mips: n / (ms / 1000) / 1e6,
    nsPerInsn: (ms * 1e6) / n,
  };
}

function addRow(i, r, isWarmup) {
  const tr = document.createElement('tr');
  if (isWarmup) tr.className = 'warmup';
  const cells = [
    [r.hidden ? `${i} ⚠` : String(i), ''],
    [r.n.toLocaleString(), 'num'],
    [`${(r.ms / 1000).toFixed(2)} 秒`, 'num'],
    [r.mips.toFixed(1), 'num'],
    [`${r.nsPerInsn.toFixed(2)} ns`, 'num'],
  ];
  for (const [text, cls] of cells) {
    const td = document.createElement('td');
    td.textContent = text;
    if (cls) td.className = cls;
    tr.appendChild(td);
  }
  $tbody.appendChild(tr);
  $results.hidden = false;
}

async function runBench() {
  $run.disabled = true;
  // **デバッグ用の機械を止める。**
  //
  // 裏で刻み続けたまま計測すると、同じコアを取り合って数字が下がる。
  // 「タブが非表示だと半分になる」のと同じ種類の汚染で、しかも
  // 自分で作った汚染なので気づきにくい
  const wasRunning = runner ? !runner.paused : false;
  runner?.stop();
  $tbody.replaceChildren();
  $results.hidden = true;
  $summary.textContent = '';
  $summary.className = 'note';

  const runs = Math.max(1, Math.min(20, Number($runs.value) || 3));
  // HLTまで走らせる場合も上限は要る。無限ループを踏んだときに
  // ブラウザごと固まるのを防ぐ番人として使う
  const limit = $untilHalt.checked
    ? Math.max(Number($limit.value) || 0, 20_000_000_000)
    : Number($limit.value);

  const warmupMode = $('warmup').value;

  try {
    const sector = await selectedSector();
    const rows = [];

    if (warmupMode !== 'none') {
      $status.textContent = '空転中… (計測には含めません)';
      await paint();
      warmUp(warmupMode, sector, limit);
    }

    for (let i = 1; i <= runs; i++) {
      $status.textContent = `計測中… (${i}/${runs}) — このタブは数秒固まります`;
      await paint();

      const r = measure(sector, limit);

      // HLTまでのつもりが上限で打ち切られた = 命令数が実行時間に依存する測定。
      // MIPSの比較に使えないので止める
      if ($untilHalt.checked && !r.halted) {
        throw new Error('HLTに到達せず上限で打ち切った。「HLTまで」を外して命令数を固定すること');
      }

      rows.push(r);
      // 空転を入れていれば1回目も対等な測定なので、薄く表示しない
      addRow(i, r, i === 1 && runs > 1 && warmupMode === 'none');
      await paint();
    }

    // 命令数が揃っていなければMIPSを並べる意味がない
    const counts = new Set(rows.map(r => r.n));
    const best = Math.max(...rows.map(r => r.mips));
    $status.textContent = `完了。${rows[0].n.toLocaleString()} 命令 × ${runs}回`;

    let msg = `最良 ${best.toFixed(1)} MIPS。`;
    // 可視性の汚染は他のどの注意より優先して伝える。
    // 非表示タブはブラウザにスロットリングされ、値が半分以下になる
    if (rows.some(r => r.hidden)) {
      $summary.className = 'note warn';
      $summary.textContent =
        '⚠ 非表示のタブで測定した回がある (⚠印)。ブラウザのスロットリングで ' +
        '値が半分以下になるため、この結果は使えない。タブを前面にして測り直すこと。';
      $run.disabled = false;
      return;
    }
    if (counts.size > 1) {
      msg += '⚠ 回によって命令数が違う。MIPSを比較してはいけない。';
      $summary.className = 'note warn';
    } else if (runs > 1) {
      const ratio = best / rows[0].mips;
      if (ratio > 1.1) {
        msg += `1回目 (${rows[0].mips.toFixed(1)} MIPS) は最良の ${(100 / ratio).toFixed(0)}% しか出ていない。`
             + (warmupMode === 'none'
                 ? '「事前の空転」を入れると揃うか試すこと。'
                 : `空転 (${warmupMode}) を入れてもこの差が残っている。`);
      } else if (warmupMode !== 'none') {
        msg += `空転 (${warmupMode}) を入れたので1回目から揃っている。`;
      }
    }
    msg += ' ネイティブとの比較は最良値どうしで行うこと。';
    $summary.textContent = msg;
  } catch (e) {
    $status.textContent = `失敗: ${e.message}`;
    $summary.className = 'note warn';
  } finally {
    $run.disabled = false;
    if (wasRunning) runner?.start();
  }
}

await init();
$status.textContent = '準備完了。ボタンを押すと計測を始めます。';
$run.addEventListener('click', runBench);


// --- デバッガ ---
//
// **計測に使う機械とは別に立てる。** 計測は `emu.run()` を一息に呼ぶ経路で、
// そこへブレークポイントを混ぜると測っているものが変わってしまう。
// ここで見るのは「同じワークロードが何をしているか」であって、速度ではない。

/** デバッグ用の機械と、その実行ループ。押されるまで作らない */
let dbgEmu = null;
let runner = null;

const dbg = new Debugger({
  emu: () => dbgEmu,
  isPaused: () => runner?.paused ?? true,
  setPaused: (v) => (v ? runner?.stop() : runner?.start()),
});

$('debug').addEventListener('click', async () => {
  if (!dbgEmu) {
    const sector = await selectedSector();
    dbgEmu = sector === null ? Emulator.bench() : new Emulator(sector);
    runner = new SlicedRunner(dbgEmu, { onStop: (why) => dbg.onStop(why) });
    runner.start();
    // 動作確認用の窓口。手元で開いているときだけ出す (main.js と同じ扱い)
    if (['localhost', '127.0.0.1'].includes(location.hostname)) {
      window.__dbgEmu = dbgEmu;
      window.__dbgRunner = runner;
    }
  }
  dbg.show();
});
