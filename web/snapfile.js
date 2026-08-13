// スナップショットのファイル形式 (Tier 3g)。
//
// 以前はJSON (gzip+base64) で書き出していて、base64で+33%・JSONの包み紙で
// さらに膨れていた。ファイルはブラウザの外の世界なので文字列である必要が無い —
// **バイナリの封筒 + gzip(RX86SNAP)** で書く。
//
//   +0   "RX86SNPF"      (8B  ファイルのmagic。中身のRX86SNAPとは別の層)
//   +8   版 = 1          (u8)
//   +9   ラベル長        (u16 LE)
//   +11  ラベル          (UTF-8。起動イメージ名)
//   +11+L 作成時刻       (f64 LE、Date.now()のms)
//   +19+L gzip(state)    (stateはcoreのsave_state = RX86SNAP v8、RLE済み)
//
// 中身の意味 (レジスタ・装置・メモリの解釈) はcore側 (snapshot.rs) が
// MAGICと版で守る。この層はあくまで「ファイルの包み方」だけを持つ。

const MAGIC = new TextEncoder().encode('RX86SNPF');
export const SNAP_EXT = '.rx86snap';

async function gzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

async function gunzip(bytes) {
  const s = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Uint8Array(await new Response(s).arrayBuffer());
}

/** state (save_stateの生バイト列) をファイル用のバイト列に包む */
export async function packSnapshot(state, label) {
  const packed = await gzip(state);
  const name = new TextEncoder().encode(label ?? 'unknown');
  const head = new Uint8Array(8 + 1 + 2 + name.length + 8);
  head.set(MAGIC, 0);
  const dv = new DataView(head.buffer);
  dv.setUint8(8, 1);
  dv.setUint16(9, name.length, true);
  head.set(name, 11);
  dv.setFloat64(11 + name.length, Date.now(), true);
  const out = new Uint8Array(head.length + packed.length);
  out.set(head, 0);
  out.set(packed, head.length);
  return out;
}

/** ファイルの先頭がこの形式か (拡張子に頼らない — 落とされた物は中身で見る) */
export function isSnapshotFile(bytes) {
  if (bytes.length < 19) return false;
  return MAGIC.every((b, i) => bytes[i] === b);
}

/** ファイルのバイト列から { label, created, state } を取り出す */
export async function unpackSnapshot(bytes) {
  if (!isSnapshotFile(bytes)) {
    throw new Error('rustx86 のスナップショットではない');
  }
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const ver = dv.getUint8(8);
  if (ver !== 1) {
    throw new Error(`知らない版のスナップショット: v${ver}`);
  }
  const len = dv.getUint16(9, true);
  const label = new TextDecoder().decode(bytes.subarray(11, 11 + len));
  const created = new Date(dv.getFloat64(11 + len, true));
  const state = await gunzip(bytes.subarray(11 + len + 8));
  return { label, created, state };
}
