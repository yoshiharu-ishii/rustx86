// 実行速度ベンチ (WASM)。
//
// 計測は performance.now() で行う。Rust側の std::time::Instant は
// wasm32-unknown-unknown では動かない (プラットフォームの時計に触れない) ため。
//
// ここはエミュレータ本体とは独立した計測ツールとして置いている。
// 任意のブートセクタを、HLTまで or 命令数固定で流せる。

import init, { Emulator } from './pkg/rustx86_wasm.js';

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

/** DOMの更新をブラウザに反映させてから重い処理へ入る */
const paint = () => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));

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

/** 1回分の計測。この間メインスレッドは止まる */
function measure(sector, limit) {
  const emu = sector === null ? Emulator.bench() : new Emulator(sector);
  const t0 = performance.now();
  const n = emu.run(limit);
  const ms = performance.now() - t0;
  return {
    n, ms,
    halted: emu.halted(),
    mips: n / (ms / 1000) / 1e6,
    nsPerInsn: (ms * 1e6) / n,
  };
}

function addRow(i, r, isWarmup) {
  const tr = document.createElement('tr');
  if (isWarmup) tr.className = 'warmup';
  const cells = [
    [String(i), ''],
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

  try {
    const sector = await selectedSector();
    const rows = [];

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
      addRow(i, r, i === 1 && runs > 1);
      await paint();
    }

    // 命令数が揃っていなければMIPSを並べる意味がない
    const counts = new Set(rows.map(r => r.n));
    const best = Math.max(...rows.map(r => r.mips));
    $status.textContent = `完了。${rows[0].n.toLocaleString()} 命令 × ${runs}回`;

    let msg = `最良 ${best.toFixed(1)} MIPS。`;
    if (counts.size > 1) {
      msg += '⚠ 回によって命令数が違う。MIPSを比較してはいけない。';
      $summary.className = 'note warn';
    } else if (runs > 1) {
      const ratio = best / rows[0].mips;
      if (ratio > 1.1) {
        msg += `1回目 (${rows[0].mips.toFixed(1)} MIPS) は最良の ${(100 / ratio).toFixed(0)}% しか出ていない。`
             + 'JITのティアアップとwasmメモリの初回確保が乗るためで、これを基準にしてはいけない。';
      }
    }
    msg += ' ネイティブとの比較は最良値どうしで行うこと。';
    $summary.textContent = msg;
  } catch (e) {
    $status.textContent = `失敗: ${e.message}`;
    $summary.className = 'note warn';
  } finally {
    $run.disabled = false;
  }
}

await init();
$status.textContent = '準備完了。ボタンを押すと計測を始めます。';
$run.addEventListener('click', runBench);
