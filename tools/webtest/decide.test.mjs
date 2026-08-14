// 画面の判断 (web/decide.js) のテスト。
//
//   node --test tools/webtest/
//
// **実ブラウザを持ち出さない。** ここで押さえるのは見た目ではなく判断で、
// 今日踏んだUIのバグはどれも判断の間違いだった。素の node で回るので
// CIでも数百ミリ秒で済む。
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  isKernel, isBootable, withToken, netUrlFromQuery, netOff, nicFor, scriptFor, guestChar, setHidden,
  THEMES, nextTheme, resolveTheme, menuAbility, pasteChunk,
} from '../../web/decide.js';

/** 先頭512バイトの末尾に 0x55AA を置いた、最小の起動できるディスク */
function disk(extra = 0) {
  const b = new Uint8Array(512 + extra);
  b[510] = 0x55;
  b[511] = 0xaa;
  return b;
}

test('落とされたものを中身で見分ける', () => {
  // vmlinux: 拡張子を持たないので、印で見分けるしかない
  const elf = new Uint8Array(0x300);
  elf.set([0x7f, 0x45, 0x4c, 0x46]);
  assert.ok(isKernel(elf), 'ELFをカーネルと見なす');

  // bzImage: setupヘッダの "HdrS" は 0x202 固定
  const bz = new Uint8Array(0x300);
  bz.set([0x48, 0x64, 0x72, 0x53], 0x202);
  assert.ok(isKernel(bz), 'HdrSをカーネルと見なす');

  assert.ok(!isKernel(disk()), 'ディスクをカーネルと間違えない');
  assert.ok(isBootable(disk()), '0x55AAがあれば起動できる');
  assert.ok(!isBootable(new Uint8Array([1, 2, 3])), '短すぎるものは起動できない');
  assert.ok(!isBootable(new Uint8Array(512)), '印が無ければ起動できない');
  // **ELFの中に偶然0x55AAが居ても、カーネル判定が先に効く**ことを担保する
  const elfWithSig = new Uint8Array(0x300);
  elfWithSig.set([0x7f, 0x45, 0x4c, 0x46]);
  elfWithSig[510] = 0x55;
  elfWithSig[511] = 0xaa;
  assert.ok(isKernel(elfWithSig), 'ELF印を優先する');
});

test('繋ぎ先の組み立て', () => {
  assert.equal(withToken('ws://h/net', ''), 'ws://h/net', 'トークン無しは素通し');
  assert.equal(withToken('ws://h/net', 'dev'), 'ws://h/net?token=dev');
  assert.equal(withToken('ws://h/net?a=1', 'dev'), 'ws://h/net?a=1&token=dev', '既にクエリがあれば &');
  assert.equal(withToken('ws://h/net', 'a b'), 'ws://h/net?token=a%20b', 'トークンは符号化する');
});

test('?net= の読み方', () => {
  const D = 'ws://127.0.0.1:8087/net';
  assert.equal(netUrlFromQuery('', D), null, '無指定は繋ぎ先を決めない (既定に任せる)');
  assert.equal(netUrlFromQuery('?net=1', D), D, '?net=1 は手元のSLiRP backend');
  assert.equal(netUrlFromQuery('?net=1&nettoken=dev', D), `${D}?token=dev`);
  assert.equal(netUrlFromQuery('?net=wss://x/net', D), 'wss://x/net', '任意の繋ぎ先');
  // **off は繋ぎ先ではない。** URLとして扱うと ws://off のような嘘になる
  assert.equal(netUrlFromQuery('?net=off', D), null);
  assert.ok(netOff('?net=off'), 'off は「挿さずに起動」の合図');
  assert.ok(!netOff('?net=1'));
  assert.ok(!netOff(''), '無指定は off ではない (既定は繋ぐ)');
});

test('挿さるNICはOSの世代で決まる', () => {
  const isa = nicFor(false);
  assert.ok(isa.usable, '16bitのゲストにはISAのNE2000が挿さる');
  assert.match(isa.label, /NE2000/);

  const pci = nicFor(true);
  assert.ok(pci.usable, 'LinuxにはPCIのRTL8029が挿さる');
  assert.match(pci.label, /RTL8029/);
});

test('自動起動は線が生きているときだけネットの続きを流す', () => {
  const m = { script: [{ when: 'a', send: 'x' }], netScript: [{ when: 'b', send: 'y' }] };
  assert.equal(scriptFor(m, false).length, 1, 'リンクが死んでいればDHCPを打たせない');
  assert.equal(scriptFor(m, true).length, 2, '生きていれば続きを流す');
  // 元の配列を書き換えない (2度目の起動で段が増え続けると自動起動が壊れる)
  assert.equal(m.script.length, 1);
  assert.equal(scriptFor({ script: [{ when: 'a' }] }, true).length, 1, 'netScript が無い機械');
  assert.equal(scriptFor(null, true), undefined, 'マシン未選択でも落ちない');
});

test('¥ はバックスラッシュとして届く', () => {
  assert.equal(guestChar('¥'), '\\', 'MacのJIS配列で A:\\> のパスが打てる');
  assert.equal(guestChar('a'), 'a');
  assert.equal(guestChar('\\'), '\\');
});

test('出し入れは属性で行う (SVGの .hidden は効かない)', () => {
  // DOMを持ち出さずに、属性を触ったかどうかだけを見る作りもの。
  // **SVGは .hidden プロパティを持たない**ので、代入で済ませると
  // 見た目が変わらないまま読み返しだけ辻褄が合う (実際に踏んだ)
  const el = {
    attrs: new Set(),
    toggleAttribute(name, on) {
      if (on) this.attrs.add(name);
      else this.attrs.delete(name);
      return on;
    },
  };
  setHidden(el, true);
  assert.ok(el.attrs.has('hidden'), '隠すときは hidden 属性が付く');
  setHidden(el, false);
  assert.ok(!el.attrs.has('hidden'), '出すときは hidden 属性が外れる');
  assert.equal(el.hidden, undefined, 'プロパティ側は触らない (SVGでは意味を持たない)');
});

test('見た目の好みは3つを回り、system だけがOSに従う', () => {
  assert.deepEqual(THEMES, ['system', 'dark', 'light']);
  assert.equal(nextTheme('system'), 'dark');
  assert.equal(nextTheme('light'), 'system', '最後まで行ったら先頭へ');
  assert.equal(nextTheme('でたらめ'), 'system', '覚えが壊れていても止まらない');

  assert.equal(resolveTheme('dark', true), 'dark', '選んだ側がOSより強い');
  assert.equal(resolveTheme('light', false), 'light');
  assert.equal(resolveTheme('system', true), 'light', 'OSが明るいなら明るく');
  assert.equal(resolveTheme('system', false), 'dark');
});

test('右クリックのメニューは今できることだけ出す', () => {
  // 起動前: 貼るゲストが居ない。イメージは受け付ける
  assert.deepEqual(menuAbility(false, false, true), { copy: false, paste: false, open: true });
  // **走っていてもコピーは選んでいなければ押せない** (普通のアプリと同じ作法)。
  // ディスクも差し替えない (ドロップと同じ)
  assert.deepEqual(menuAbility(true, false, false), { copy: false, paste: true, open: false });
  // 選んでいればコピーできる。走っていなくても構わない (画面に残った字を取れる)
  assert.equal(menuAbility(true, true, false).copy, true);
  assert.equal(menuAbility(false, true, true).copy, true);
});

test('貼り付けは環の空きの分だけ一度に渡す', () => {
  // 空の環 (16枠) には、1枠残して15文字まで渡す。
  // **1文字ずつに絞らない** — 絞るとタイプライターのように見える
  assert.equal(pasteChunk(16, 0, 100), 15);
  // 配送中の分も席を取る (8042に4バイト = 2文字が居る)
  assert.equal(pasteChunk(16, 2, 100), 13);
  // 残りが少なければ残りだけ
  assert.equal(pasteChunk(16, 0, 3), 3);
  // **席が無ければ渡さない。** 溢れさせると実機と同じく捨てられる
  assert.equal(pasteChunk(1, 0, 100), 0);
  assert.equal(pasteChunk(0, 0, 100), 0);
  assert.equal(pasteChunk(8, 8, 100), 0, '配送中の分で埋まっていれば待つ');
  assert.equal(pasteChunk(4, 9, 100), 0, '見積もりが過剰でも負にならない');
  // 貼るものが無ければ0
  assert.equal(pasteChunk(16, 0, 0), 0);
});
