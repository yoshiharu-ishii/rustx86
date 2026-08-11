// F1a の関門プローブ — 「実行時にwasmを生成して走らせる」固定費を測る。
//
//   node tools/webtest/jit-probe.mjs
//
// テンプレートJIT (ADR-0008) の設計は、この3つの固定費で決まる:
//   1. 生成:        ブロック→wasmバイト列の組み立て (自前コードの速さ)
//   2. instantiate: V8にモジュールを食わせてコンパイルさせる (V8の都合)
//   3. 呼び出し:    ホストwasm↔生成wasmの往復 (境界の税)
// これを測らずに設計すると「1ブロック=1モジュール」のような
// instantiate地獄を作りかねない。数字で「まとめ焼き」の粒度を決める。
//
// モジュールはバイト手組み (依存なし)。生成する関数は共有メモリ上の
// 「レジスタファイル」を読み書きする8命令ぶんの列 — uop 8個のブロックの雛形。

function uleb(n) {
  const out = [];
  do {
    let b = n & 0x7f;
    n >>>= 7;
    if (n) b |= 0x80;
    out.push(b);
  } while (n);
  return out;
}
function sleb(n) {
  const out = [];
  for (;;) {
    let b = n & 0x7f;
    n >>= 7;
    const done = (n === 0 && !(b & 0x40)) || (n === -1 && (b & 0x40));
    if (!done) b |= 0x80;
    out.push(b);
    if (done) return out;
  }
}
const enc = new TextEncoder();
function section(id, body) {
  return [id, ...uleb(body.length), ...body];
}
function vec(items) {
  return [...uleb(items.length), ...items.flat()];
}
function name(s) {
  const b = [...enc.encode(s)];
  return [...uleb(b.length), ...b];
}

// 1関数ぶんの本体: メモリ0の reg[i] += reg[j] 形の i32 RMW を8個並べる。
// オフセットを散らして「ブロックごとに違うコード」を再現 (V8のキャッシュ封じ)
function funcBody(seed) {
  const ins = [];
  for (let k = 0; k < 8; k++) {
    const dst = ((seed + k) % 8) * 4;
    const src = ((seed + k + 3) % 8) * 4;
    ins.push(
      0x41, ...sleb(dst),            // i32.const dst
      0x41, ...sleb(dst), 0x28, 0x02, 0x00, // i32.const dst; i32.load
      0x41, ...sleb(src), 0x28, 0x02, 0x00, // i32.const src; i32.load
      0x6a,                          // i32.add
      0x36, 0x02, 0x00,              // i32.store
    );
  }
  ins.push(0x0b); // end
  const body = [0x00 /* locals無し */, ...ins];
  return [...uleb(body.length), ...body];
}

// n個の関数を持つモジュールを組む (メモリはimport = ホストと共有)
function buildModule(nFuncs, seed0) {
  const type = section(1, vec([[0x60, 0x00, 0x00]])); // () -> ()
  const imp = section(2, vec([[...name('e'), ...name('m'), 0x02, 0x00, 0x01]])); // memory min1
  const func = section(3, vec(Array.from({ length: nFuncs }, () => [0x00])));
  const expo = section(
    7,
    vec(Array.from({ length: nFuncs }, (_, i) => [...name('f' + i), 0x00, ...uleb(i)])),
  );
  const code = section(10, vec(Array.from({ length: nFuncs }, (_, i) => funcBody(seed0 + i))));
  return new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...type, ...imp, ...func, ...expo, ...code]);
}

const mem = new WebAssembly.Memory({ initial: 1 });
const imports = { e: { m: mem } };
const now = () => performance.now();

// ---- 1. 生成コスト (バイト列の組み立てだけ) ----
{
  const t0 = now();
  const N = 10000;
  for (let i = 0; i < N; i++) buildModule(1, i);
  const t1 = now();
  console.log(`生成:        ${(((t1 - t0) / N) * 1e3).toFixed(1)}µs/ブロック (バイト組み立てのみ)`);
}

// ---- 2. instantiate: 1ブロック=1モジュール方式の固定費 ----
{
  const N = 2000;
  const mods = Array.from({ length: N }, (_, i) => buildModule(1, i));
  const t0 = now();
  const insts = mods.map((m) => new WebAssembly.Instance(new WebAssembly.Module(m), imports));
  const t1 = now();
  console.log(`個別焼き:    ${(((t1 - t0) / N) * 1e3).toFixed(1)}µs/ブロック (Module+Instance ×${N})`);
  // 呼べることの確認
  insts[0].exports.f0();
}

// ---- 3. まとめ焼き: Nブロックを1モジュールに束ねる方式 ----
for (const N of [100, 1000, 10000]) {
  const t0 = now();
  const m = buildModule(N, 0);
  const t1 = now();
  const inst = new WebAssembly.Instance(new WebAssembly.Module(m), imports);
  const t2 = now();
  console.log(
    `まとめ焼き:  ${N}ブロック → 生成${(t1 - t0).toFixed(1)}ms + 焼き${(t2 - t1).toFixed(1)}ms` +
      ` = ${(((t2 - t0) / N) * 1e3).toFixed(1)}µs/ブロック`,
  );
}

// ---- 4. 呼び出しの税: JS→生成wasm の往復 ----
{
  const inst = new WebAssembly.Instance(new WebAssembly.Module(buildModule(1, 0)), imports);
  const f = inst.exports.f0;
  f(); // ウォームアップ
  const N = 1e7;
  const t0 = now();
  for (let i = 0; i < N; i++) f();
  const t1 = now();
  const perCall = ((t1 - t0) / N) * 1e6; // ns
  console.log(`呼び出し:    ${perCall.toFixed(0)}ns/回 (uop8個入りブロック = ${(perCall / 8).toFixed(0)}ns/uop相当)`);
}

// ---- 5. 比較の物差し: 今のインタプリタの1命令 ----
console.log('物差し:      現行インタプリタは wasm ~28MIPS(Ivy)〜62MIPS(M1) = 16〜36ns/命令');
