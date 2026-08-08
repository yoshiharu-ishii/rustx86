// 起動できるマシンの一覧。
//
// **ここはデータだけを持つ。** 起動の仕方は [`main.js`](./main.js)、
// 機械の回し方は [`machine.js`](./machine.js) の仕事である。
//
// ## なぜ Rust 側に `MachineProfile` を作らないのか
//
// ELKS と FreeDOS は **16bit・640K・フロッピー起動が完全に同じ**で、違うのは
// イメージだけである。今の時点で構成の違いを表す型を Rust に作っても、
// 取りうる値が1つしか無い。**投機的な抽象になる**ので作らない。
//
// 本当に構成が分かれるのは Tier 3 (32bit) からで、そこで初めて
// CPUの世代・メモリ量・装置構成が枝分かれする。型はそのときに作る。
// それまでは「どのイメージか」「どこまで動くか」という、**実際に変わるものだけ**を
// ここに置く。
//
// ## 未実装のものも並べる
//
// これは教材なので、**どこまで行けて、なぜ止まるのかが画面に出ている**方がよい。
// 灰色の項目は「まだ無い」ではなく「ここが次」を意味する。

/** @typedef {'ok'|'partial'|'todo'} Status */

export const MACHINES = [
  {
    group: '16bit リアルモード',
    id: 'elks',
    label: 'ELKS 0.9.1',
    sub: '16bit UNIX',
    image: './fd1440.img',
    status: 'ok',
    note:
      'ログイン名は root。tetris / invaders / ttypong / sl / matrix が入っている。' +
      'BIOSはほとんど使わず、8042とテキストVRAMを直接叩くOSである。',
    source: 'https://github.com/ghaerr/elks/releases',
    sourceLabel: 'ELKS のリリース',
    file: 'fd1440.img',
  },
  {
    group: '16bit リアルモード',
    id: 'freedos',
    label: 'FreeDOS 1.4',
    sub: '8086ビルド',
    image: './fd14boot.img',
    status: 'ok',
    note:
      'DOSプロンプト (A:\\>) まで自動で進む。ELKSと違って画面もキーもディスクも' +
      'BIOS経由なので、BIOS層の検証になっている。' +
      'ELIZA / ZMIY / ROW4T が動く。ZMIY は50行の盤面を描いて、見える25行の窓を' +
      'CRTCで蛇に追従させる (ハードウェアスクロール)。' +
      'HANGMAN だけは CGAグラフィックスを要求するので Tier 6 待ち — ' +
      '画面が白いのは「描いていない」のではなく「描く先が無い」ためで、要求は記録に残る。',
    // **選んだらプロンプトまで自動で進む。**
    //
    // このフロッピーは本来インストーラを起動する。素のプロンプトに降りるには
    // 起動時に F5 で CONFIG.SYS と AUTOEXEC.BAT を飛ばし、聞かれるシェルの場所を
    // 答える — DOSの定石だが、**知らないと辿り着けない**。
    // 「押す瞬間を当てて長いパスを打て」は動くとは言えないので、機械にやらせる。
    script: [
      { when: 'FreeDOS kernel', send: { scancodes: [0x3f, 0xbf] } }, // F5
      { when: 'full shell command line', send: '\\FREEDOS\\BIN\\COMMAND.COM\n' },
    ],
    source: 'https://download.freedos.org/1.4/FD14-FloppyEdition.zip',
    sourceLabel: 'FreeDOS 1.4 Floppy Edition',
    // 配布物の 144m/x86BOOT.img を fd14boot.img として置く
    file: 'fd14boot.img (配布zipの 144m/x86BOOT.img)',
  },
  {
    group: '32bit プロテクトモード',
    id: 'linux',
    label: 'Linux',
    sub: 'bzImage 直接ロード',
    status: 'todo',
    note: 'Tier 4。BIOSは通さず bzImage + initrd を直接ロードして32bitエントリへ飛ぶ。',
  },
  {
    group: 'その他',
    id: 'bench',
    label: '実行速度ベンチ',
    sub: '命令数固定のワークロード',
    // **OSと同じ扱いにする。** ベンチもCPUを回す処理なので、
    // 別ページへ飛ばさず右のペインに出す。単独ページ (bench.html) は
    // 「計測だけの静かな部屋」として残してあり、中身は同じ部品である
    kind: 'bench',
    status: 'ok',
    note:
      'ネイティブとブラウザで何倍違うかを測る。命令数が固定のワークロード ' +
      '(asm/bench.asm) を流すので、環境が変わってもMIPSを比較できる。' +
      '計測だけの静かな部屋が要るなら bench.html を直接開く。',
  },
];

/** 表示順を保ったままグループごとにまとめる */
export function byGroup(machines = MACHINES) {
  const out = new Map();
  for (const m of machines) {
    if (!out.has(m.group)) out.set(m.group, []);
    out.get(m.group).push(m);
  }
  return out;
}

/** @param {Status} s */
export function statusLabel(s) {
  return { ok: '動く', partial: '途中まで', todo: 'これから' }[s] ?? s;
}
