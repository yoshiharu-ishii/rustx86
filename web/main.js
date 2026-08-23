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
import { MACHINES, ROOTFS, ISOS, byGroup } from './machines.js';
import { Debugger } from './debugger.js';
import { mountLinux } from './linux-machine.js';
import { packSnapshot, unpackSnapshot, isSnapshotFile, SNAP_EXT } from './snapfile.js';
import { Speaker } from './speaker.js';
import { Opl } from './opl.js';
import { NetLink } from './netlink.js';
// 画面の**判断**は decide.js に集めてある (node --test で押さえられる)。
// ここは配線に徹する — どの要素をどう出すか、いつ機械を回すか
import {
  isKernel, isIso, isBootable, withToken, netUrlFromQuery, netOff, nicFor, scriptFor, guestChar, setHidden,
  THEMES, nextTheme, resolveTheme, menuAbility, pasteChunk,
} from './decide.js';

const $ = id => document.getElementById(id);
const term = new Terminal($('screen'), { scrollback: 1000 });
// PCスピーカー。全機械で1個 — 実機にもスピーカーは1個しか付いていない。
// ブラウザの自動再生ポリシーがあるので、最初のキー/クリックで unlock する
const speaker = new Speaker();
const opl = new Opl();
// マスター音量 (OPL2 + PC スピーカー)。覚えておく — 機械を替えても音量は机の設定
{
  const el = $('volume');
  const apply = () => {
    const v = Number(el.value) / 100;
    opl.setVolume(v);
    speaker.setVolume(v);
  };
  el.value = localStorage.getItem('rx86.volume') ?? '70';
  el.addEventListener('input', () => {
    localStorage.setItem('rx86.volume', el.value);
    apply();
  });
  apply();
}
for (const ev of ['keydown', 'pointerdown']) {
  document.addEventListener(ev, () => { speaker.unlock(); opl.unlock(); }, { once: false, capture: true });
}

let machine = null;
/** Linuxを選んでいるときの取っ手 (選んでいなければ null)。
    **machine と並べて先に置く** — 画面の判断 (コピーの可否など) が
    読み込みの早い段階で参照するので、宣言が後ろにあると触れない */
let linux = null;
/** 最後に起動したイメージ。再起動に使う */
let lastImage = null;

/**
 * **何から起動したVMなのか。** 一覧のどれを選んだかとは別物で、
 * 落としたイメージや復元した状態から動いていることもある
 * @type {'library'|'image'|'snapshot'|null}
 */
let bootOrigin = null;
const ORIGIN_LABEL = { library: 'ライブラリ', image: 'イメージ', snapshot: '状態復元' };

// ---------- ネットワーク ----------
//
// **既定で繋いでおく。** 机の裏でLANケーブルが刺さっているのが普通の姿で、
// 使うたびに挿し直させる理由がない。相手が居なければ赤が点くだけで、機械は
// 「リンクの無いNIC」を積んで普通に起動する。挿さずに起動したいときは ?net=off
// (NIC無し起動のビット同一 = ADR-0017 の不変条件は、CIのheadlessが
//  NICを挿さないので今までどおり守られる)。
//
// 繋ぎ先は2通りの決まり方をする:
//   1. URLの ?net= — E2Eや自動化のための**上書き**。これがあれば即座に試し、
//      ダイアログは「今どこに繋がっているか」の表示に徹する
//   2. ダイアログでの入力 — 人間用。localStorage に覚える

const NET_DEFAULT_URL = 'ws://127.0.0.1:8087/net';
const NET_STORE = 'rustx86.net';

/** ?net= の繋ぎ先 (無指定と ?net=off は null) */
function netFromQuery() {
  return netUrlFromQuery(location.search, NET_DEFAULT_URL);
}

/** ブラウザが覚えている設定。{url, token} */
function netSaved() {
  try {
    return { url: NET_DEFAULT_URL, token: '', ...JSON.parse(localStorage.getItem(NET_STORE) || '{}') };
  } catch {
    return { url: NET_DEFAULT_URL, token: '' };
  }
}

/**
 * SLiRP backendへの結線。**これはケーブルであって、機械の部品ではない。**
 *
 * 実機でも、LANケーブルを生きたスイッチに挿せばリンクランプは点く —
 * その machine が起動しているかも、OSがドライバを持っているかも関係ない。
 * だから結線はページに1本だけ持ち、機械の入れ替えでも抜かない。
 * 機械に**NICを挿す**のは別の話で、そちらは電源ONの瞬間にしかできない。
 */
let link = null;

/** 起動スクリプト (判断は decide.js。ここは今のリンク状態を渡すだけ) */
function scriptOf(m) {
  return scriptFor(m, link?.state === 'up');
}

// ---------- スナップショット ----------
//
// 機械の状態は Rust 側がコンパクトなバイナリで書き出す (連長圧縮済み)。
// **JSONで束ねるのはここの仕事**で、いつ・何のイメージから取ったのかという
// 人間向けの情報を添える。中身をJSONの数値配列にすると1MBが数MBに膨れるので、
function setStatus(text, warn = false) {
  $('status').textContent = text;
  $('status').className = warn ? 'warn' : '';
}

/** ツールバーの表示を実際の状態に合わせる */
function syncControls() {
  // スタート画面では機械向けの操作列を丸ごと伏せる。
  // 押せない灰色のボタンの列は「まだ何も選んでいない」画面には要らない
  const onWelcome = !$('welcomePane').hidden;
  // スタート画面では機械まわりを丸ごと伏せる。**まだ机に何も置いていないの
  // だから、備品も操作列も状態カードも出す意味がない** — 選ぶことに集中させる
  for (const id of ['barRig', 'barOps', 'consoleHead', 'stage', 'devCard', 'infoCard']) {
    $(id).hidden = onWelcome;
  }
  // 下辺の絵も持ち場に合わせる: 案内 (本) か、動いている媒体 (フロッピー) か。
  // **SVGに .hidden = で代入しても効かない** — hidden は HTMLElement の
  // プロパティで、SVGElement には無い (代入するとJSの変数が生えるだけで
  // 属性は変わらないため、読み返すと辻褄が合って気づけない)。属性で操作する
  setHidden($('footGuide'), !onWelcome);
  setHidden($('footDisk'), onWelcome);
  // **「スタート」は起動したら消す。** 戻る先は画面ではなく「電源を切る」で、
  // 一度機械が立ち上がれば行き先としての意味を失う (左上のVMカードが
  // 「今どこに居るか」を引き受けたので、なおさら要らない)
  const start = $('machines').querySelector('[data-id="start"]');
  if (start) start.hidden = !onWelcome;
  // **一覧の点灯はライブラリから起動したときだけ。** 落としたカーネルでも
  // Linux機の組み立てには select() を通るので、そのままだと一覧の
  // 「Linux 6.18」が点いてしまう — 走っているのは持ち込みのカーネルであって、
  // 一覧のそれではない
  if (bootOrigin && bootOrigin !== 'library') markCurrent(null);
  syncVmCard();
  if (onWelcome) return;
  // 電源の灯り。**入っていれば緑** — 機械が居るかどうかがそのまま状態である
  const powered = !!machine || !!linux?.booted;
  $('power').toggleAttribute('data-on', powered);
  $('power').title = powered ? '電源を切る' : '電源を入れる';
  $('power').disabled = !powered && !lastImage && !linux;
  const on = !!machine;
  // 配列の選択は端末のもの (シリアル端末は文字を送るので配列に依らない)
  $('ctlLayout').hidden = !!linux;
  // ルートFSとRAMはLinuxの機械のときだけ。**16bit機には無い概念**なので出さない
  // ラベルと選択肢は対で包んである (index.html の .ctl) — 出し入れも対で
  for (const id of ['ctlRoot', 'ctlIso', 'ctlRam', 'ctlJit']) $(id).hidden = !linux;
  // デバッガ。Linuxはワーカーの中だが、覗き見RPC (linux-machine.js) 越しに覗ける
  $('debug').disabled = !on && !linux?.booted;
  // **どのNICを挿すかは、そのOSが知っているバスで決まる。**
  // 16bit (ELKS/DOS) はISAのNE2000、32bitのLinuxはPCIのRTL8029。
  // 中身は同じDP8390で、皮 (番地の見つかり方) だけが違う
  // PCI 機 (Linux ワーカー、ISO を載せた 32bit PC) は RTL8029、素の 16bit 機は ISA の NE2000
  const nic = nicFor(!!linux || !!machine?.pci);
  $('netSel').querySelector('option[value="on"]').textContent = nic.label;
  $('netSel').title = NET_LABEL[link?.state ?? 'off'] ?? NET_LABEL.off;
  // 左のデバイス欄も同じ判断で書き直す — ランプの状態変化しか見ていないと、
  // マシンを切り替えた瞬間だけ前のNICの名前が残る (実際に残った)
  setNetLamp(link?.state);
  syncSidebar();
  if (linux) {
    // 再起動は電源が入っているときの顔。OFFのときは「電源」が入口
    $('boot').disabled = linux.busy || !linux.booted;
    $('pause').disabled = !linux.booted;
    setPauseFace(!!linux.paused);
    $('snapExport').disabled = !linux.booted;
    $('snapImport').disabled = linux.busy;
    return;
  }
  $('pause').disabled = !on;
  setPauseFace(!!machine?.paused);
  $('boot').disabled = !lastImage;
  $('snapExport').disabled = !on;
  $('snapImport').disabled = false;
}

/**
 * 一時停止ボタンの顔。**絵とラベルの両方を替える** —
 * 止まっているときに ⏸ が出ていると、押した先が分からない。
 * (textContent で書き換えると中のSVGごと消えるので、要素を分けてある)
 */
function setPauseFace(paused) {
  setHidden($('pauseIcon'), paused);
  setHidden($('playIcon'), !paused);
  $('pauseLabel').textContent = paused ? '再開' : '一時停止';
}

/** 最後に起動したイメージの名前。スナップショットに添える */
let lastLabel = '';

/** 最後に挿した HDD 像 (再起動で同じものを挿し直す)。無ければ null */
let lastHdd = null;

function boot(image, label, hdd = null) {
  lastLabel = label;
  $('welcomePane').hidden = true;
  $('screen').hidden = false;
  machine?.stop();
  speaker.mute(); // 機械が替わるので、前の機械の音は道連れにしない
  stopPaste(); // 貼りかけの文字も道連れにしない (前の機械宛だったもの)
  // **ケーブルは抜かない。** 機械を替えてもスイッチとの結線は生きたままで、
  // 灯りも点きっぱなし — 実機で床のLANケーブルを抜かないのと同じである
  // Linuxを見ている最中にフロッピーを落とされたら、Linuxを畳んでVGA端末に戻す
  if (linux) {
    linux.destroy();
    linux = null;
    $('linuxScreen').hidden = true;
    $('screen').hidden = false;
  }
  try {
    machine = new Machine(image);
  } catch (e) {
    setStatus(`起動できない: ${e}`, true);
    return;
  }
  lastImage = image;
  // BIOS のハードディスク (C:)。**1命令も走る前に挿す** — DOS はプロンプトまでに
  // C: を数えるので、後から挿しても見えない (電源投入時にしかできないのは NIC と同じ)
  lastHdd = hdd;
  if (hdd) machine.hddAttach(hdd);
  term.reset();
  machine.onFrame = (cells, row, col, redraw) => {
    term.sample(cells, row, col);
    if (redraw) term.draw();
    advanceScript();
  };
  // mode 13h の顔。テキストとどちらを出すかは machine.js が video_mode で選ぶ
  machine.onGfxFrame = (fb, pal) => term.drawPixels(fb, pal);
  // デバッガが止めたら、理由を子ウインドウへ。**開いていなくても状態表示は出す**
  machine.onDebugStop = (why) => {
    dbg.onStop(why);
    setStatus(`デバッガが止めた: ${why}`);
    syncControls();
  };
  machine.onTone = hz => speaker.update(hz);
  // Adlib (OPL2): 実時間で経った分のサンプルを core から引き出して鳴らす
  machine.onOpl = render => opl.pump(n => render(opl.rate, n));
  // **NICを挿すのは電源を入れるこの瞬間だけ。** 起動時にしか装置を探さない
  // ゲスト (ELKSのカーネル) が居るので、後から挿しても見えない — 実機と同じ
  if (link) attachNet(machine);
  // 物理キーはそのまま、貼り付けはASCIIとして送る (¥→\ の理由は decide.js)
  term.onKey = (code, down) => machine.key(code, down);
  term.onChar = ch => machine.typeChar(guestChar(ch));
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

/** 状態を右上のピルと左のカードへ同時に書く。**同じ数字を2つ持たない** */
function showState(text, hist, histTitle = '') {
  $('pillState').textContent = text;
  $('pillHist').textContent = hist;
  $('pillHist').title = histTitle;
  // 走っていれば緑、止まっていれば灰
  const live = text !== '停止中' && text !== '電源オフ';
  $('pillDot').classList.toggle('ok', live);
}

/** 1秒に2回、速度と履歴の深さを出す。教材として「今どれくらい出ているか」を見せる */
setInterval(() => {
  if (linux) {
    // 起動の定規 (時間で統一、2026-08-13)。headless.mjs と同じ定義の秒数
    const boot = linux.bootSecs != null ? `起動 ${linux.bootSecs.toFixed(1)}s` : '';
    // **電源オフ → 起動中 → 走っている速さ** の順に遷移する。機械を選んだだけの
    // 状態を「起動中」と言うと、電源を入れたのかどうかが画面から分からない
    // (VGA機の側は最初から電源オフと言っていた)。
    // アイドル中の数字は「時計を流しただけ」なので MIPS とは呼ばない
    const run = linux.paused
      ? '停止中'
      : !linux.booted
        ? linux.busy
          ? '起動中'
          : '電源オフ'
        : linux.idle
          // シェルに着く前の休みは「アイドル」ではなく「起動中」— isolinux の
          // boot: 待ちも HLT で休むので、着いたように見えてしまう
          ? (linux.bootSecs == null ? '起動中' : 'アイドル')
          : linux.mips
            ? `${linux.mips.toFixed(0)} MIPS`
            : '起動中';
    showState(
      run,
      boot || '—',
      linux.bootSecs != null
        ? '電源ONからシェルまで (ISO はプロンプトが出た瞬間、他はバナー)'
        : 'まだシェルに着いていない',
    );
    return;
  }
  if (!machine) {
    showState('電源オフ', '履歴 0 行');
    return;
  }
  const run = machine.paused ? '停止中' : machine.idle ? 'アイドル' : `${machine.mips.toFixed(0)} MIPS`;
  const hist = term.offset
    ? `▲ ${term.offset} 行前`
    : `履歴 ${term.scrollback.length} 行`;
  showState(run, hist);
}, 500);

// --- キーボード配列 ---
//
// 既定はJIS。**スキャンコードはキーの位置なので配列とは無関係**だが、
// ゲスト (ELKS) はUS配列の対応表しか持たないため、JIS配列の実機では
// 見たままの文字が入らない。JISのときは位置ではなく文字を送って辻褄を合わせる。

// --- ルートFSとRAM (Linuxの機械) ---
//
// **つまみを画面に出す。** URL (`?initrd=` / `?ram=`) だけだと、後で自分の
// 設定の理由が分からなくなる — 「なぜこの機械でgccが使えるのか」「なぜRAMが
// 256MBなのか」が、選べるものの一覧として見えているのが一番効く。
// URLは初期値、選び直したら憶える。**効くのは次の電源ONから** (機械の構成なので)
const ROOT_KEY = 'rustx86.rootfs';
const ISO_KEY = 'rustx86.iso';
const RAM_KEY = 'rustx86.ram';
const rootSel = $('rootSel');
const isoSel = $('isoSel');
const ramSel = $('ramSel');
const jitSel = $('jitSel');
// CD-ROM はドライブ。**起動元はルートFSの有無で決まる**:
//   ルートFS「—」+ CD あり → CD から起動 (El Torito、BIOS 経由、画面は VGA)
//   ルートFS あり + CD あり → カーネル起動して CD を挿した状態 (ATAPI → /mnt/cdrom)
// 両方「—」は無い (何も無い機械に意味が無い) — ルートFSの先頭に戻す
for (const [sel, label] of [
  [rootSel, '— (CD から起動)'],
  [isoSel, '— (空)'],
]) {
  const o = document.createElement('option');
  o.value = '';
  o.textContent = label;
  sel.append(o);
}
for (const r of ISOS) {
  const o = document.createElement('option');
  o.value = r.name;
  o.textContent = r.label;
  o.title = r.note;
  isoSel.append(o);
}
for (const r of ROOTFS) {
  const o = document.createElement('option');
  o.value = r.name;
  o.textContent = r.label;
  o.title = r.note;
  rootSel.append(o);
}
// **web/ に置いた ISO は全部 CD-ROM の棚に並ぶ。** serve.py が /cdroms.json で *.iso を
// 数えて返す (静的配信では 404 → ISOS の説明付きの項だけ)。ISOS に無い名前は
// ファイル名のまま足す — 取ってきた ISO を置くだけで選べる
fetch('./cdroms.json', { cache: 'no-store' })
  .then(r => (r.ok ? r.json() : []))
  .then(list => {
    const wanted = localStorage.getItem(ISO_KEY) || '';
    for (const { name, size } of list) {
      if (ISOS.some(x => x.name === name)) continue;
      const o = document.createElement('option');
      o.value = name;
      o.textContent = `${name} (${(size / 1048576).toFixed(0)} MB)`;
      o.title = 'web/ に置いた ISO (説明は無し)。El Torito なら CD から起動できる';
      isoSel.append(o);
    }
    // 記憶していた名前が棚に現れたら選び直す (初期化時には無かった)
    if (wanted && isoSel.value !== wanted && [...isoSel.options].some(o => o.value === wanted)) {
      isoSel.value = wanted;
      syncRootfsHint();
    }
  })
  .catch(() => {});
const q0 = new URLSearchParams(location.search);
isoSel.value = q0.get('cd') || q0.get('iso') || localStorage.getItem(ISO_KEY) || '';
if (!isoSel.value) isoSel.value = ''; // 知らない名前
// ルートFS: URL > 記憶。記憶が「—」(CD から起動) でも CD が無ければミニへ
const rootMem = localStorage.getItem(ROOT_KEY);
rootSel.value = q0.get('initrd') ?? (rootMem === null ? ROOTFS[0].name : rootMem);
if (rootSel.value === undefined) rootSel.value = ROOTFS[0].name; // 知らない名前
if (!rootSel.value && !isoSel.value) rootSel.value = ROOTFS[0].name;
ramSel.value = q0.get('ram') || localStorage.getItem(RAM_KEY) || 'auto';
// JIT (F1d wasm)。q0の宣言より前に置くとTDZでmain.jsごと死ぬ (2026-08-17に実際に死んだ)
jitSel.value = q0.get('jit') || localStorage.getItem('rx86.jit') || 'off';
jitSel.addEventListener('change', () => {
  localStorage.setItem('rx86.jit', jitSel.value);
  // 実行中でも切り替わる (比較実験の外部フラグ)
  linux?.setJit?.(jitSel.value === 'on');
});
if (!ramSel.value) ramSel.value = 'auto';
for (const [sel, key] of [
  [rootSel, ROOT_KEY],
  [isoSel, ISO_KEY],
  [ramSel, RAM_KEY],
]) {
  sel.addEventListener('change', () => {
    // 両方「—」だけは許さない (CD を抜いたら、起動元はミニへ)
    if (!rootSel.value && !isoSel.value) rootSel.value = ROOTFS[0].name;
    localStorage.setItem(ROOT_KEY, rootSel.value);
    localStorage.setItem(ISO_KEY, isoSel.value);
    localStorage.setItem(key, sel.value);
    syncRootfsHint();
    focusScreen();
  });
}

/** 選択の理由を画面に出す。selectのtitleと、状態欄への一言 */
function syncRootfsHint() {
  const r = ROOTFS.find(x => x.name === rootSel.value);
  rootSel.title = r ? `${r.sub} — ${r.note}` : '「—」= CD-ROM から起動する (El Torito、BIOS 経由)';
  const i = ISOS.find(x => x.name === isoSel.value);
  isoSel.title = i
    ? `${i.sub} — ${i.note}`
    : 'CD-ROM ドライブ (ATAPI)。ルートFSが「—」なら CD から起動、あればカーネル起動して /mnt/cdrom に掛かる';
  ramSel.title =
    ramSel.value === 'auto'
      ? 'initramfsの展開後の大きさから決める (足りないと中身が黙って欠ける)'
      : `${ramSel.value}MB を明示する`;
  // 走っている機械の構成は変えられない — 次の電源ONから効くことを言う
  if (linux?.booted) setStatus('ルートFS/ISO/RAMの変更は次の電源ONから効きます');
}

const LAYOUT_KEY = 'rustx86.layout';
const layoutSel = $('layout');
term.layout = localStorage.getItem(LAYOUT_KEY) || 'jp';
layoutSel.value = term.layout;
layoutSel.addEventListener('change', () => {
  term.layout = layoutSel.value;
  localStorage.setItem(LAYOUT_KEY, term.layout);
  focusScreen();
});

// --- 操作 ---

// いま見えている画面にフォーカスを返す。マシンが居なければ何もしない
function focusScreen() {
  if (linux) $('linuxScreen').focus();
  else if (machine) $('screen').focus();
}

// ボタンを押した後はフォーカスを**画面に返す**。ボタンに残すと、直後の
// Enter/Spaceがゲスト行きのつもりでボタンをもう一度押してしまう
// (再起動の意図せぬ連打)。個々のハンドラではなくバブリングで一括して受ける。
// デバッガだけは例外 — 子ウインドウに移った注意を奪い返さない
for (const bar of document.querySelectorAll('.bar')) {
  bar.addEventListener('click', e => {
    const b = e.target.closest('button');
    if (b && b.id !== 'debug') focusScreen();
  });
}

// --- コンソールの見出しにある2つ: コピーとペースト ---
//
// 右クリックのメニューに組みを添える。**その環境の組みだけ出す** —
// Macに Ctrl+Shift+C と書いても、押す人は居ない
{
  const mac = /Mac|iPhone|iPad/.test(navigator.userAgentData?.platform || navigator.platform || '');
  $('mCopyKey').textContent = mac ? '⌘C' : 'Ctrl+Shift+C';
  $('mPasteKey').textContent = mac ? '⌘V' : 'Ctrl+Shift+V';
}

// ---------- 見た目の明暗 ----------
//
// **既定はシステムに従う。** 押すたびに システム→暗→明→システム と回る。
// 選んだらブラウザが覚える (機械ごとではなく、この人の好みなので)。
// 端末の中は常に黒地に緑のまま — 実機のモニタが部屋の明るさで色を
// 変えないのと同じで、変わるのは周りの造作だけである

const THEME_STORE = 'rustx86.theme';
const PREFERS_LIGHT = matchMedia('(prefers-color-scheme: light)');

/** 覚えている好み ('system' | 'dark' | 'light') */
function themePref() {
  const t = localStorage.getItem(THEME_STORE);
  return THEMES.includes(t) ? t : 'system';
}

/** 好みを画面に反映する (解き方は decide.js の resolveTheme) */
function applyTheme() {
  const pref = themePref();
  document.documentElement.dataset.theme = resolveTheme(pref, PREFERS_LIGHT.matches);
  setHidden($('themeAuto'), pref !== 'system');
  setHidden($('themeDark'), pref !== 'dark');
  setHidden($('themeLight'), pref !== 'light');
  $('theme').title = {
    system: '見た目: システムに従う',
    dark: '見た目: 暗く',
    light: '見た目: 明るく',
  }[pref];
}

applyTheme();
PREFERS_LIGHT.addEventListener('change', applyTheme); // system のときだけ効く
$('theme').addEventListener('click', () => {
  localStorage.setItem(THEME_STORE, nextTheme(themePref()));
  applyTheme();
});

// ---------- 右クリックのメニュー ----------
//
// 画面の上で右を押したときだけ出す。**今できることだけを並べる** —
// 選んでいなければコピーは灰色、走っていればイメージは開けない

const menu = $('menu');

function openMenu(x, y) {
  menu.hidden = false;
  // 画面の外へはみ出さない位置に置く (右下で押されたとき用)
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, innerWidth - r.width - 8)}px`;
  menu.style.top = `${Math.min(y, innerHeight - r.height - 8)}px`;
}

function closeMenu() {
  if (!menu.hidden) menu.hidden = true; // 閉じているものを閉じ直さない
}

// consoleBox はこの下で定義されるので、ここでは要素を直に引く
$('console').addEventListener('contextmenu', e => {
  e.preventDefault();
  const can = menuAbility(!!(machine || linux), hasSelection(), acceptsDrop());
  $('mCopy').disabled = !can.copy;
  $('mPaste').disabled = !can.paste;
  $('mOpen').disabled = !can.open;
  openMenu(e.clientX, e.clientY);
});
for (const ev of ['pointerdown', 'blur', 'wheel']) {
  window.addEventListener(ev, e => {
    if (ev === 'pointerdown' && menu.contains(e.target)) return;
    closeMenu();
  });
}
window.addEventListener('keydown', e => {
  // **捕捉段で止める。** ここで通すと、閉じるついでに Esc がゲストへ届く
  if (e.key === 'Escape' && !menu.hidden) { e.stopPropagation(); e.preventDefault(); closeMenu(); }
}, true);
$('mCopy').addEventListener('click', () => { closeMenu(); doCopy(); });
$('mPaste').addEventListener('click', () => { closeMenu(); requestPaste(); });
$('mOpen').addEventListener('click', () => { closeMenu(); $('imageFile').click(); });

// ---------- コピーと貼り付けの一本道 ----------
//
// 呼び口は4つある — 見出しのボタン・キーの組み・右クリックのメニュー・
// Macの ⌘V (ブラウザが paste 事象をくれる)。**判断はここ1箇所に置く。**
// 以前は経路ごとに実装が分かれていて、組みで押すと選んだ範囲しか取れず、
// 取り消しも効かず、状態も出ないという食い違いが起きていた

/** いま選ばれている文字列 (画面はVGAとシリアルの2つあるので、出ている方を見る) */
function selectedText() {
  return (linux ? linux.selectedText() : term.selectedText()) || '';
}

/** 何か選ばれているか。**可否を出し分けるだけならこちらを使う** (中身を作らない) */
function hasSelection() {
  return linux ? linux.hasSelection() : term.hasSelection();
}

/**
 * コピー。**選んだところだけを取る** — どこのアプリでもそうであるように。
 *
 * 以前は選んでいなければ画面全体を取っていた。それを貼り戻すと起動ログが
 * 丸ごとコマンドとして流れ込む (実際にELKSがそうなった)。
 * 画面ぜんぶが欲しいときは「ログを保存」が受け持つ
 */
async function doCopy() {
  const text = selectedText();
  if (!text) {
    setStatus('コピーするところをドラッグで選んでください');
    return;
  }
  if (await term.copyText(text)) setStatus(`選んだ ${text.length} 文字をコピーしました`);
  else setStatus('コピーできませんでした (ブラウザに拒否されました)', true);
}

/**
 * 貼り付けを頼まれたときの入口。
 *
 * **貼りかけは止められる。** 間違って大きなものを貼るとコマンドが
 * 延々と流れ込むので、流れている最中の2度目は「取り消し」になる。
 * これはボタンだけの作法だったが、組みでも右クリックでも同じにする
 * @param {string} [text] 中身が既に届いている経路 (Macの ⌘V) だけ渡す
 */
function requestPaste(text) {
  if (pasteQueue) {
    const left = pasteQueue.length;
    stopPaste();
    setStatus(`貼り付けを止めました (残り ${left} 文字は捨てました)`);
    return;
  }
  if (text === undefined) pasteFromClipboard();
  else deliverPaste(text);
}

/** クリップボードを読んで流す (中身が届かない経路のための前段) */
async function pasteFromClipboard() {
  let text = '';
  try {
    text = await navigator.clipboard.readText();
  } catch {
    setStatus('クリップボードを読めませんでした (ブラウザに拒否されました)', true);
    return;
  }
  deliverPaste(text);
}

/** ゲストへ流す出口。**経路によらずここを通る** */
function deliverPaste(text) {
  if (!text) return;
  // シリアル (Linux) は受け側の行列が深いので一息に流せる。
  // VGA機は**キーボードとして打つ**ので、少しずつ流す (下記)
  if (linux) linux.send(text);
  else startPaste(text.replaceAll('¥', '\\'));
  focusScreen();
}

// ---------- 貼り付け ----------
//
// **一度に流し込むと消える。** キーボードから来た文字は BIOS の待ち行列に
// 積まれるが、これは**16個しかない環**で、溢れた分は実機と同じく捨てられる。
// 画面全文 (1558文字) を貼ったら数十文字しか届かなかったのはこれである。
// 行列の空きを見ながら、空いた分だけ流す。

/** 貼り付け待ちの残り */
let pasteQueue = '';
let pasteTimer = null;

/**
 * ゲストが握っている修飾キーを離させる。
 *
 * **Ctrl+Shift+V は「押したまま」貼り付けが始まる。** Ctrl と Shift の
 * 打鍵はVの前に既にゲストへ届いていて、離すのは人の指が離れたときなので、
 * その間に流し込んだ文字はゲスト側で制御コードに化ける
 * (ELKSの login: に `§` や `‼♣‼↔` が出たのはこれ)。
 * 貼り付けは打鍵ではないので、始める前にこちらから離させる。
 * 実際の keyup は後から重ねて届くが、離すのを2度送っても害はない
 */
function releaseModifiers() {
  for (const code of ['ControlLeft', 'ControlRight', 'ShiftLeft', 'ShiftRight', 'AltLeft', 'AltRight']) {
    machine.key(code, false);
  }
}

function startPaste(text) {
  if (!pasteQueue && machine) releaseModifiers(); // 流し始めの一度だけでよい
  pasteQueue += text;
  if (pasteTimer) return;
  pasteTimer = setInterval(() => {
    if (!machine || machine.paused) return; // 止まっている間は待つ (再開すれば続く)
    if (!pasteQueue) {
      clearInterval(pasteTimer);
      pasteTimer = null;
      setStatus('貼り付けました');
      return;
    }
    // **空いた席は毎回いっぱいまで埋める。** 配分の理由は decide.js の
    // pasteChunk (ここを2度外しているので、判断はテストのある側に置く)
    const inflight = Math.ceil(machine.emu.key_backlog() / 2); // 1文字 = 押す/離すの2バイト
    const n = pasteChunk(biosKeyRoom(), inflight, pasteQueue.length);
    if (!n) return; // 席が無い。ゲストが読むまで待つ
    machine.paste(pasteQueue.slice(0, n));
    pasteQueue = pasteQueue.slice(n);
    if (pasteQueue) setStatus(`貼り付け中… 残り ${pasteQueue.length} 文字`);
  }, 4); // 環が空いた直後に継ぎ足せるよう、刻みは細かく
}

function stopPaste() {
  pasteQueue = '';
  if (pasteTimer) clearInterval(pasteTimer);
  pasteTimer = null;
}

/**
 * BIOSのキー待ち行列の空き。BDAに頭 (0x41A) と尻尾 (0x41C) があり、
 * 環は 0x1E〜0x3E の16個。**BIOSを使わないゲスト (ELKSは8042を直に読む) では
 * 頭も尻尾も動かない**ので、常に空きありと見える — そちらは受け側の行列が
 * 際限なく伸びるので、それで困らない
 */
function biosKeyRoom() {
  const b = machine.emu.read_mem(0x41a, 4);
  const head = b[0] | (b[1] << 8);
  const tail = b[2] | (b[3] << 8);
  const span = 0x3e - 0x1e; // 環の大きさ (バイト)
  const used = (((tail - head) % span) + span) % span / 2;
  return Math.max(0, 16 - 1 - used);
}

// **クリップボードの取っ手は機械に依らないので、ここで一度だけ張る。**
// boot() の中で張っていたときは、Linuxを直接起動した回では張られず、
// 組みでの貼り付けが黙って効かなかった。行き先 (doCopy / requestPaste) が
// 自分で機械を見るようになった以上、張り替える理由はもう無い。
// Macの ⌘V は中身が届くのでそれを渡し、Ctrl+Shift+V は事象が飛ばないので
// 読みにいかせる — どちらも取り消し・流量制御・Linux分岐は共通である
term.onPaste = text => requestPaste(text);
term.onPasteRequest = () => requestPaste();
term.onCopyRequest = () => doCopy();


// --- デバッガの子ウインドウ ---
//
// Emulator は再起動のたびに作り直されるので、**参照を握らせず毎回聞かせる**。
// 握らせると再起動後に古い機械を覗き続けることになる
// **いま動いている機械**を見せる。参照を握らせず毎回聞く —
// Emulator は再起動のたびに作り直されるため。
// Linuxのときはワーカー越しの代役 (各メソッドが Promise) を渡す。
// デバッガは全部 await で呼ぶので、同期の機械と代役の区別を知らない
const dbg = new Debugger({
  emu: () => (linux ? linux.dbgEmu : (machine?.emu ?? null)),
  isPaused: () => (linux ? linux.paused : (machine?.paused ?? true)),
  setPaused: (v) => {
    if (linux) {
      linux.setPaused(v);
      syncControls();
      return;
    }
    if (!machine) return;
    if (v) machine.stop();
    else machine.start();
    syncControls();
  },
  // 最初から流し直す。Linuxは**フル起動** — デバッガで見たいのは
  // 起動の流れそのものなので、スナップショット復帰では意味がない
  restart: async () => {
    if (linux) {
      await linux.boot();
      return;
    }
    if (!current) return;
    await bootFromUrl(current);
    startScript(scriptOf(current));
  },
});

// ---------- ネットワークの灯り と 設定ダイアログ ----------
//
// LANポートの絵が3色で状態を言う。**文字で説明しない** —
// 実機のハブのランプと同じで、色だけで足りる:
//   灰 まだ繋いでいない (既定。故障ではない)
//   緑 繋がった
//   赤 繋がらない / 切れた

/**
 * ケーブルを挿す。**機械が動いていなくても繋がるし、灯りも点く** —
 * リンクの成否はSLiRP backendとの間の話で、ゲストのドライバとは関係ない
 */
function netConnect(url) {
  link?.close();
  link = new NetLink(url);
  // Linuxのワーカーが受け取り側なら、新しい線にも同じ配線を張り直す
  // (張り忘れるとフレームがinboxに黙って積もる)
  if (linux) link.onFrame = f => linux?.netInject(f);
  setNetLamp('wait');
  link.onState = (s, reason) => {
    setNetLamp(s);
    // **状態は灯りと同じ短い名前で言う。** 繋ぎ先や失敗の詳しい理由は
    // ダイアログの仕事で、ここは1行の状態表示に徹する
    if (s === 'up') setStatus(NET_LABEL.up);
    else if (s === 'down') setStatus(`${NET_LABEL.down} — ${reason}`, true);
    netDialogSync();
  };
  netDialogSync();
}

/** ケーブルを抜く */
function netDisconnect() {
  link?.close();
  link = null;
  if (machine) machine.netlink = null;
  setNetLamp(null);
  netDialogSync();
}

/**
 * 機械にNICを挿す。**電源ONの瞬間にしかできない。**
 * ケーブルが死んでいても (赤) カードは挿す — スロットに刺さったNICと
 * 抜けたケーブルは別物で、ゲストからは「リンクの無いNIC」に見えるのが正しい
 */
function attachNet(m) {
  m.emu.net_attach(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
  m.netlink = link;
}

/** 灯りの3色に対応する名前。**装置の状態は装置の言葉で短く言う** —
    ifconfig や ip link と同じ語彙にしておくと、ゲストの中で見る状態と繋がる。
    ボタンの吹き出しにも状態表示にも同じ文字を使う (2箇所で言い方が違うと、
    同じ状態なのか判断させることになる) */
const NET_LABEL = {
  up: 'Network:Connect',
  down: 'Network:Disconnect',
  wait: 'Network:Connecting',
  off: 'Network:Disable',
};

function setNetLamp(state) {
  for (const el of [$('netSel'), $('devNicDot')]) {
    if (state) el.dataset.net = state;
    else delete el.dataset.net;
  }
  $('netSel').title = NET_LABEL[state] ?? NET_LABEL.off;
  // 選択そのものも状態に追従させる (「設定…」を選んだ後に戻す用でもある)
  $('netSel').value = link ? 'on' : 'off';
  const nicName = nicFor(!!linux || !!machine?.pci).label;
  $('devNic').textContent = link
    ? { up: nicName, wait: '接続中…', down: 'リンク無し' }[state] ?? nicName
    : '未接続';
}

/**
 * 左上の「今のVM」と、端末の上の「起動元」。
 * **同じ事実を粗さを変えて2箇所に出す** — 左は状態(何が動いているか)、
 * 端末の上は素性(何を食わせて動いているか)
 */
function syncVmCard() {
  const live = !!machine || !!linux?.booted;
  const paused = machine?.paused || linux?.paused;
  const via = bootOrigin ? `（${ORIGIN_LABEL[bootOrigin]}起動）` : '';
  $('vmDot').classList.toggle('ok', live && !paused);
  $('vmDot').classList.toggle('partial', !!paused);
  $('vmState').textContent = !live ? '停止中' : paused ? `一時停止中${via}` : `実行中${via}`;
  // **名前も素性に合わせる。** 落としたイメージで動いているのに一覧で
  // 選んだOS名が出ていると、別物を見ていることになる
  // Linuxは自分が読んだカーネル名を持っている。**前の機械のラベルを使わない**
  const imageLabel = linux ? linux.imageName || lastLabel : lastLabel;
  const name = bootOrigin === 'library' ? current?.label : imageLabel || current?.label;
  $('vmName').textContent = live || bootOrigin ? name ?? '—' : 'マシンを選んでください';

  const showOrigin = live && !!bootOrigin;
  $('originRow').hidden = !showOrigin;
  if (showOrigin) {
    $('originKind').textContent = `起動元：${ORIGIN_LABEL[bootOrigin]}`;
    $('originName').textContent = imageLabel || current?.file || '—';
  }
}

/** 左の状態カードを今の姿に合わせる。**画面に出ている数字と同じ出どころ**にする */
function syncSidebar() {
  const conJp = $('layout').value === 'jp' ? 'JIS 配列' : 'US 配列';
  $('devCon').textContent = linux ? 'シリアル (ttyS0)' : conJp;
  // Linuxのときは自分が読んだ名前を使う (前の機械のラベルを引きずらない)
  const imageLabel = linux ? linux.imageName : lastLabel;
  // ディスク型なら挿した実体 (vda) を、メモリ型なら initramfs を出す
  $('devDisk').textContent = linux
    ? linux.diskName
      ? `${linux.diskName} (vda)`
      : linux.initrdName
    : lastLabel || '—';
  $('infoImage').textContent = imageLabel || '—';
  // 機種とRAMは機械自身に聞く (デバッガと同じ出どころ)
  try {
    const j = JSON.parse(machine?.emu.cpu_json() ?? 'null');
    $('infoMachine').textContent = j?.machine ?? (linux ? 'PC (32bit)' : '—');
    $('infoRam').textContent = j ? `${j.ramMb} MB` : linux ? `${linux.ramMb} MB` : '—';
    $('infoArch').textContent = j?.pe ? 'i386 (プロテクトモード)' : 'i386 (リアルモード)';
  } catch {
    /* 起動直後などで読めなくても、表示が古いだけなので黙って見送る */
  }
}

/** ダイアログの中身を今の状態に合わせる */
function netDialogSync() {
  const msg = $('netMsg');
  const byQuery = !!netFromQuery();
  msg.className = 'msg';
  if (!link) {
    msg.textContent = '';
  } else if (link.state === 'up') {
    msg.textContent = `${link.url} に繋がっています`;
    msg.classList.add('ok');
    // ケーブルは生きているが、この機械はNIC無しで起動している状態を明示する。
    // **繋いだのに使えない**が一番の混乱どころなので、ここで先回りする
    if (machine && !machine.netlink) {
      msg.textContent += ' (この機械はNIC無しで起動しています。「再起動」で挿さります)';
    }
  } else if (link.state === 'down') {
    msg.textContent = link.reason;
    msg.classList.add('ng');
  } else {
    msg.textContent = '接続中…';
    msg.classList.add('wait');
  }
  // ?net= が居るときは、この画面から設定を変えても意味がない (URLが勝つ)
  for (const id of ['netUrl', 'netToken', 'netConnect']) $(id).disabled = byQuery;
  $('netDisconnect').hidden = !link;
  if (byQuery) {
    msg.textContent += (msg.textContent ? ' — ' : '') + 'URLの ?net= で指定されています';
  }
}

/** ネットワークの設定画面を開く (今の設定を入れてから) */
function openNetDialog() {
  const saved = netSaved();
  const q = netFromQuery();
  $('netUrl').value = q ?? saved.url;
  $('netToken').value = q ? '' : saved.token;
  netDialogSync();
  $('netDialog').showModal();
}

// NICの選択。**カードを挿すか抜くか**の2択で、設定は「設定…」から。
// 覚えている繋ぎ先があるので、ふだんは選ぶだけで繋がる
$('netSel').addEventListener('change', e => {
  const v = e.target.value;
  if (v === 'config') {
    openNetDialog();
    e.target.value = link ? 'on' : 'off'; // 「設定…」は選択肢ではなく入口
    return;
  }
  if (v === 'on') {
    const saved = netSaved();
    netConnect(withToken(saved.url, saved.token));
    if (machine && !machine.netlink) {
      setStatus(`${NET_LABEL.up} — ゲストから使うには「再起動」`);
    }
  } else {
    netDisconnect();
    setStatus(NET_LABEL.off);
  }
});

$('netForm').addEventListener('submit', e => {
  // <form method="dialog"> は submitter の value が dialog.returnValue になる
  const how = e.submitter?.value;
  if (how === 'connect') {
    const url = $('netUrl').value.trim();
    const token = $('netToken').value.trim();
    localStorage.setItem(NET_STORE, JSON.stringify({ url, token }));
    // **ケーブルはその場で挿さる** (灯りも点く)。ただしNICが機械に
    // 挿さるのは電源ONの瞬間だけなので、走行中なら再起動を促す
    netConnect(withToken(url, token));
    // 走行中の機械はNIC無しで起動している。**この一言だけは足す** —
    // 「繋いだのに使えない」で詰まるのが一番もったいない
    if (machine && !machine.netlink) {
      setStatus(`${NET_LABEL.up} — ゲストから使うには「再起動」`);
    }
  } else if (how === 'disconnect') {
    netDisconnect();
    setStatus(NET_LABEL.off);
  }
});

// 電源。**入っていれば切り、切れていれば入れる** — 実機のスイッチと同じ1個。
// 電源を切った機械は消えるが、入れていたイメージは机の上に残るので、
// もう一度押せば同じものが立ち上がる
$('power').addEventListener('click', async () => {
  speaker.mute();
  if (linux) {
    // **OFFにしても機械は選ばれたまま。** ルートFS/RAMを選び直して
    // もう一度入れられる — 実機で電源を切ってもマシンは机に残るのと同じ。
    // 起動中 (busy) に切るのも許す (組み間違いに気づくのは大抵ログの途中)
    if (linux.booted || linux.busy) linux.powerOff();
    else await linux.boot();
    syncControls();
    return;
  }
  if (machine) {
    machine.stop();
    machine.netlink = null; // ケーブルは机に残る。機械から抜けるだけ
    machine = null;
    term.reset();
    term.draw();
    // 素性 (bootOrigin) は残す — 同じものからもう一度立ち上げるため
    setStatus('電源を切りました。もう一度押すと同じイメージで立ち上がります');
  } else if (lastImage) {
    boot(lastImage, lastLabel, lastHdd);
    startScript(scriptOf(current));
  }
  syncControls();
});

$('debug').addEventListener('click', () => {
  dbg.show();
  dbg.reset();
});

$('pause').addEventListener('click', () => {
  // 止めた機械は音も止める — 鳴りっぱなしのオシレータだけが残ると不気味
  speaker.mute();
  if (linux) {
    linux.setPaused(!linux.paused);
    syncControls();
    return;
  }
  if (!machine) return;
  if (machine.paused) machine.start();
  else machine.stop();
  syncControls();
});

$('boot').addEventListener('click', () => {
  if (linux) {
    // **再起動 = フル起動。** 実機の再起動がBIOSから走るのと同じで、
    // カーネルログの流れる本物のブートをやり直す。
    // スナップショットからの高速復帰は「マシンを選び直したとき」の顔
    linux.boot();
    return;
  }
  // **再起動でも自動起動スクリプトを流し直す。** 電源を入れ直したのだから
  // F5もドライバ常駐もやり直しになる — 流さないと FreeDOS が言語選択で
  // 止まったまま「ネットに繋がらない」ように見える。
  // ラベルも保つ (ここで 'ディスク' に潰すと、左のディスク欄が化ける)
  if (lastImage) {
    boot(lastImage, lastLabel || 'ディスク', lastHdd);
    startScript(scriptOf(current));
  }
});

$('snapExport').addEventListener('click', async () => {
  // スナップショット書出 (Tier 3g)。保存の実体はこのファイルだけ —
  // localStorage/ワーカー内のクイックセーブは廃止した (入り口は1つ)
  try {
    let state, label;
    if (linux) {
      state = await linux.captureState();
      if (!state) return;
      label = 'linux';
    } else if (machine) {
      state = machine.saveState();
      label = (lastLabel || 'machine').replace(/\.\w+$/, '');
    } else {
      return;
    }
    const bytes = await packSnapshot(state, label, linux ? 'L' : 'V');
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([bytes], { type: 'application/octet-stream' }));
    a.download = `${label}${SNAP_EXT}`;
    a.click();
    URL.revokeObjectURL(a.href);
    setStatus(`スナップショットを書き出した (${(bytes.length / 1024).toFixed(0)} KB)`);
  } catch (e) {
    setStatus(`書き出せない: ${e.message}`, true);
  }
});

$('snapImport').addEventListener('click', () => $('snapFile').click());
$('snapFile').addEventListener('change', () => {
  const f = $('snapFile').files?.[0];
  $('snapFile').value = '';
  if (f) insertMedia(f); // 中身のmagicで見分ける — ドロップと同じ口
});

$('save').addEventListener('click', () => {
  if (linux) {
    // 端末が見た全文 (履歴+画面)。VGA機のログ保存と同じ意味論
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([linux.logText], { type: 'text/plain' }));
    a.download = 'linux.log';
    a.click();
    URL.revokeObjectURL(a.href);
    return;
  }
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

/**
 * イメージを受け取れるのは**スタート画面だけ**。
 *
 * 走っている機械の画面に落とせてしまうと、電源の入ったPCにフロッピーを
 * 差し込んだら勝手に再起動した、という乱暴な挙動になる。一度立ち上げたら
 * まず電源を切る — 実機と同じ順序を守らせる。
 * (別のイメージに移りたいときは、左のOSライブラリか「イメージを開く…」から)
 */
function acceptsDrop() {
  return !$('welcomePane').hidden;
}

// **子要素をまたぐたびに dragleave が飛ぶ**ので、素直に付け外しすると
// 枠が明滅する。入った回数を数えて、0になったときだけ消す
let dragDepth = 0;
// **どこに落ちてもブラウザには開かせない。** 既定の動作はファイルを開くこと
// なので、素通しにするとページごと差し替わり、走っていた機械が消える
for (const ev of ['dragover', 'drop']) {
  document.addEventListener(ev, e => {
    e.preventDefault();
    if (e.type === 'dragover' && e.dataTransfer) e.dataTransfer.dropEffect = 'none';
  });
}

consoleBox.addEventListener('dragenter', e => {
  e.preventDefault();
  if (acceptsDrop() && ++dragDepth === 1) consoleBox.classList.add('drop');
});
consoleBox.addEventListener('dragover', e => {
  e.preventDefault();
  e.stopPropagation(); // 上の「受け取らない」既定を、この枠の中だけ上書きする
  // 「コピーして取り込む」の意思表示。これが無いとカーソルが禁止マークになる
  if (e.dataTransfer) e.dataTransfer.dropEffect = acceptsDrop() ? 'copy' : 'none';
});
consoleBox.addEventListener('dragleave', () => {
  if (--dragDepth <= 0) {
    dragDepth = 0;
    consoleBox.classList.remove('drop');
  }
});
consoleBox.addEventListener('drop', async e => {
  e.preventDefault();
  dragDepth = 0;
  consoleBox.classList.remove('drop');
  if (!acceptsDrop()) {
    // 黙って捨てない。**なぜ入らないのかと、どうすれば入るのかを言う**。
    // 電源を切っても机の上には機械が残っているので、入口は左のメディア口になる
    setStatus('動いている機械にはイメージを入れられません。左の「イメージを開く…」から差し替えてください', true);
    return;
  }
  const f = e.dataTransfer?.files?.[0];
  if (f) insertMedia(f);
});

// 「イメージを開く…」(メニュー/カード) も同じ口。実機で言えばドライブは1つ —
// ドロップもファイル選択も**同じメディア投入**として扱う
$('imageFile').addEventListener('change', () => {
  const f = $('imageFile').files?.[0];
  $('imageFile').value = ''; // 同じファイルをもう一度選べるように
  if (f) insertMedia(f);
});

async function insertMedia(f) {
  // スナップショットならそこへ戻る。ディスクなら起動する。
  // 判定は拡張子でなく中身のmagic (Tier 3g。旧JSON+localStorage形式は廃止 —
  // 保存の実体はファイルに一本化)
  const head = new Uint8Array(await f.slice(0, 32).arrayBuffer());
  if (isSnapshotFile(head)) {
    try {
      const o = await unpackSnapshot(new Uint8Array(await f.arrayBuffer()));
      const stamp = `${o.label}、${o.created.toLocaleString()}`;
      if (o.kind === 'L') {
        // Linux機の状態: 走っていればその場で復元、居なければ
        // マウントして**この状態から起動** (まっさらなページでも一発で戻れる)
        if (linux?.booted) {
          linux.loadStateBytes(o.state);
        } else {
          if (!linux) await select(MACHINES.find(x => x.kind === 'linux'), { autoBoot: false });
          bootOrigin = 'snapshot';
          await linux.boot({ snapshot: o.state });
        }
        setStatus(`${f.name} の状態に戻した (${stamp})`);
        return;
      }
      if (!machine) {
        setStatus('先に同じディスクイメージを起動してください (VGA機のスナップショット)', true);
        return;
      }
      bootOrigin = 'snapshot';
      machine.loadState(o.state);
      term.reset();
      setStatus(`${f.name} の状態に戻した (${stamp})`);
      $('screen').focus();
    } catch (err) {
      setStatus(`復元できない: ${err.message}`, true);
    }
    return;
  }
  setStatus(`${f.name} を読み込み中…`);
  const bytes = new Uint8Array(await f.arrayBuffer());
  // **中身で行き先を決める。** 落ちてくるものは3種類ある:
  //   スナップショット (上で処理済み) / Linuxカーネル / ディスクイメージ
  // 拡張子では決めない — vmlinux-lts のように拡張子を持たないものがある
  if (isKernel(bytes)) {
    if (!linux) await select(MACHINES.find(x => x.kind === 'linux'), { autoBoot: false });
    bootOrigin = 'image';
    lastLabel = f.name;
    await linux.boot({ kernel: bytes, kernelName: f.name });
    return;
  }
  // ISO (CD001) は Linux の機械で BIOS 経由 (El Torito) — 画面は VGA の升目で写す
  if (isIso(bytes)) {
    if (!linux) await select(MACHINES.find(x => x.kind === 'linux'), { autoBoot: false });
    bootOrigin = 'image';
    lastLabel = f.name;
    await linux.boot({ iso: bytes, isoName: f.name });
    return;
  }
  // ディスクとして通す前に**印を確かめる**。拡張子で絞るのをやめた以上、
  // 何を落とされてもおかしくない — 分からないものは分からないと言う
  if (!isBootable(bytes)) {
    setStatus(
      `${f.name} は起動できる形に見えません ` +
        '(ブートセクタの印 0x55AA も ISO 9660 の印 CD001 も無く、Linuxカーネルでもスナップショットでもない)',
      true,
    );
    return;
  }
  bootOrigin = 'image';
  boot(bytes, f.name);
}


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

/** イメージが実在するか (HEADで聞く)。**無いOSはライブラリに並べない** —
 *  選べないものを見せて「取ってきて置け」と言うより、置いたら現れる方が素直。
 *  取得の案内はスタートの1行とREADMEが持つ */
async function imageAvailable(m) {
  const urls = m.probe ?? (m.image ? [m.image] : null);
  if (!urls) return true; // イメージを要しない項目 (スタート・メディア)
  for (const u of urls) {
    try {
      const r = await fetch(u, { method: 'HEAD' });
      if (r.ok) return true;
    } catch {
      /* 続けて次の候補 */
    }
  }
  return false;
}

async function renderMachines() {
  const nav = $('machines');
  const avail = new Map(
    await Promise.all(MACHINES.map(async (m) => [m.id, await imageAvailable(m)])),
  );
  nav.textContent = '';
  // カードの中身は .body に入れる (見出し帯を持つ他のカードと作法を揃える)
  const body = document.createElement('div');
  body.className = 'body';
  nav.append(body);
  for (const [group, list] of byGroup()) {
    const rows = list.filter((m) => avail.get(m.id));
    if (rows.length === 0) continue; // 空のグループは見出しごと出さない
    if (group) {
      const h = document.createElement('h3');
      h.textContent = group;
      body.append(h);
    }
    for (const m of rows) {
      // **別ページに住むマシンはリンクにする** (Linux)。見た目はボタンと揃えるが、
      // 中身は本物の <a> — 新しいタブで開く・URLをコピーする、が普通にできる
      const b = document.createElement(m.href ? 'a' : 'button');
      b.title = m.note ?? '';
      // 緑ランプ + 名前だけの1行。「動く」は色で分かるので言葉にしない。
      // ランプはマシンだけ (「スタート」はマシンではない)
      const dot = m.status ? `<span class="dot ${m.status}"></span>` : '';
      b.innerHTML = `${dot}<span class="name">${m.label}</span>`;
      if (m.href) {
        b.href = m.href;
      } else {
        b.dataset.id = m.id;
        b.disabled = m.status === 'todo';
        b.addEventListener('click', () => select(m));
      }
      body.append(b);
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

async function select(m, { autoBoot = true } = {}) {
  // 「イメージを開く…」はまだ何も切り替えない — ファイルが選ばれた瞬間に
  // insertMedia が起動する (キャンセルなら何も起きない)
  if (m.kind === 'open') {
    $('imageFile').click();
    return;
  }
  // **切り替えたら前の機械は捨てる。**
  //
  // OSもベンチも同じCPUを回している。片方を残したまま次を始めると、
  // 裏で走り続けて画面にも出ず、計測を汚し、デバッガは古い機械を覗く。
  // 「選び直したらまっさらから」を守る
  machine?.stop();
  linux?.destroy();
  linux = null;
  $('welcomePane').hidden = true;
  $('linuxScreen').hidden = true;
  $('screen').hidden = false;

  current = m;
  markCurrent(m.id);
  showNote(m);

  // 「スタート」— オープニングに戻る。機械は全部畳んだ状態
  if (m.kind === 'welcome') {
    machine = null;
    lastImage = null;
    term.reset();
    $('screen').hidden = true;
    $('welcomePane').hidden = false;
    showState('電源オフ', '履歴 0 行'); // 前のマシンの「アイドル」等を持ち越さない
    bootOrigin = null;
    lastLabel = '';
    showNote(null);
    setStatus('OSライブラリから選ぶか、イメージをドロップ /「イメージを開く…」で起動してください');
    dbg.reset();
    syncControls();
    return;
  }

  if (m.kind === 'linux') {
    machine = null;
    lastImage = null;
    term.reset();
    $('screen').hidden = true;
    $('linuxScreen').hidden = false;
    linux = mountLinux($('linuxScreen'), {
      onStatus: setStatus,
      // クリップボードはVGA機と同じ道へ (取り消しも状態表示も共通になる)
      onPaste: text => requestPaste(text),
      onPasteRequest: () => requestPaste(),
      onCopyRequest: () => doCopy(),
      onState: syncControls,
      onDbgStop: (why) => dbg.onStop(why),
      onTone: hz => speaker.update(hz),
      // NICを挿すのは電源の瞬間 (VGA機の attachNet と同じ判断)。
      // 線が来ていればRTL8029が挿さって出る — MACも16bit機と同じ
      mac: () => (link ? [0x52, 0x54, 0x00, 0x12, 0x34, 0x56] : undefined),
      // どのルートFSを何MBで載せるかも電源の瞬間に決まる (上のNICと同じ)
      rootfs: () => ({ name: rootSel.value, cd: isoSel.value, ramMb: ramSel.value === 'auto' ? 0 : +ramSel.value }),
      jit: () => jitSel.value === 'on',
      // 画面: フレームバッファか (機械の定義で決まる — 「Linux (フレームバッファ)」)
      fb: () => m.fb === true,
      // ゲストが送ったフレームは線へ (無ければ捨てる — 抜けたケーブル)
      onNetTx: f => link?.send(f),
    });
    // 届いたフレームはワーカーへ直行 (inboxで寝かせない — ワーカー側に
    // 自分のスライス境界の受信箱がある)
    if (link) link.onFrame = f => linux?.netInject(f);
    dbg.reset();
    syncControls();
    // **Linuxは電源OFFで待つ** — ルートFS/RAMを選んでから電源を入れる
    // (実機の「組んでから電源」の順)。16bit機は従来どおり選んだら即起動 —
    // あちらには選ぶ構成が無い。スナップショット復元は呼び手が boot({snapshot}) する
    if (autoBoot) setStatus('電源OFF — ルートFSとRAMを選んで「電源」で起動');
    syncControls();
    return;
  }

  bootOrigin = 'library';
  await bootFromUrl(m);
  startScript(scriptOf(m));
  dbg.reset();
}

async function bootFromUrl(m = current) {
  if (!m?.image) return;
  setStatus(`${m.label} を取得中…`);
  try {
    const r = await fetch(m.image);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const image = new Uint8Array(await r.arrayBuffer());
    // ハードディスク像 (C:) がある項はそれも取ってから電源を入れる。.gz は輸送路の
    // 圧縮で、ここ (ホスト) で1回だけ解く (Linux のディスクと同じ作法)
    let hdd = null;
    if (m.hdd) {
      setStatus(`${m.label} のハードディスク像を取得中…`);
      const h = await fetch(m.hdd);
      if (!h.ok) throw new Error(`${m.hdd}: HTTP ${h.status}`);
      let bytes = await h.arrayBuffer();
      if (m.hdd.endsWith('.gz')) {
        const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
        bytes = await new Response(stream).arrayBuffer();
      }
      hdd = new Uint8Array(bytes);
    }
    term.reset();
    boot(image, m.image.replace('./', ''), hdd);
  } catch (e) {
    setStatus(
      `${m.image.replace('./', '')} が見つからない (${e.message})。イメージをここにドロップしてください`,
      true,
    );
  }
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
  await renderMachines();
  markCurrent('start');
  for (const b of document.querySelectorAll('#welcomePane [data-open]')) {
    b.addEventListener('click', () => $('imageFile').click());
  }
  setStatus('OSライブラリから選ぶか、イメージをドロップ /「イメージを開く…」で起動してください');
  syncControls();
  // **既定で繋いでおく。** 机の裏でLANケーブルが刺さっているのが普通の姿で、
  // 使うたびに挿し直させる理由がない。相手 (SLiRP backend) が居なければ
  // 赤が点くだけで、機械は「リンクの無いNIC」を積んで普通に起動する。
  // 切っておきたいときは ?net=off
  const q = netFromQuery();
  if (new URLSearchParams(location.search).get('net') === 'off') {
    setNetLamp(null);
  } else if (q) {
    netConnect(q);
  } else {
    const saved = netSaved();
    netConnect(withToken(saved.url, saved.token));
  }
} catch (e) {
  setStatus(`WASMの読み込みに失敗: ${e}`, true);
}
