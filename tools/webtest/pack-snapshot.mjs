// ネイティブ (boot 例の SNAPSHOT_SAVE) で作った生の控えを、**ブラウザが読める封筒**
// (.rx86snap) に包み直す。
//
//   node tools/webtest/pack-snapshot.mjs <生の控え> [ラベル] [V|L]
//
// 出力は同じ場所に .rx86snap で置く。ブラウザは「イメージを開く…」かドロップで受け取り、
// **中身の magic** で見分けて復元する (拡張子には頼っていない)。
//
// 使いどころ: DSL のように起動が長い機械を、速いネイティブ (PGO ビルド) で
// 目的の状態まで進めてから控え、ブラウザはそこから始める。
// CD の像は控えに入っていないので、ブラウザ側で**同じ ISO を CD-ROM に選んでおく**
// (復元時に挿し直される — `cd_wanted()`)
import { readFileSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, extname } from 'node:path';
import { packSnapshot, SNAP_EXT } from '../../web/snapfile.js';

const [src, label, kind] = process.argv.slice(2);
if (!src) {
  console.error('usage: node tools/webtest/pack-snapshot.mjs <生の控え> [ラベル] [V|L]');
  process.exit(1);
}
const state = new Uint8Array(readFileSync(src));
// core の save_state は先頭に "RX86SNAP" を持つ (封筒の magic とは別の層)
const head = Buffer.from(state.slice(0, 8)).toString('latin1');
if (head !== 'RX86SNAP') {
  console.error(`これは core の控えではない (先頭 = ${JSON.stringify(head)})`);
  process.exit(1);
}
const name = label ?? basename(src, extname(src));
const out = await packSnapshot(state, name, kind ?? 'L');
const dst = join(dirname(src), name + SNAP_EXT);
writeFileSync(dst, out);
console.log(
  `${dst} (${(out.length / 1024 / 1024).toFixed(1)} MB、生 ${(state.length / 1024 / 1024).toFixed(1)} MB を gzip)`,
);
