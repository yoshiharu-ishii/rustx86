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
  // 先頭は「スタート」— オープニングに戻る入口。マシンではないが、
  // メニューの並びとして同格に扱うとハイライトや選択の作法を使い回せる
  {
    group: '',
    id: 'start',
    label: 'スタート',
    kind: 'welcome',
  },
  {
    group: 'OSライブラリ',
    id: 'elks',
    label: 'ELKS 0.9.1',
    sub: 'フロッピー1枚のUNIX',
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
    group: 'OSライブラリ',
    id: 'freedos',
    label: 'FreeDOS 1.4',
    sub: 'DOS',
    image: './fd14boot.img',
    status: 'ok',
    note:
      'DOSプロンプト (A:\\>) まで自動で進む。ELKSと違って画面もキーもディスクも' +
      'BIOS経由なので、BIOS層の検証になっている。' +
      'ELIZA / ZMIY / ROW4T が動く。ZMIY は50行の盤面を描いて、見える25行の窓を' +
      'CRTCで蛇に追従させる (ハードウェアスクロール)。' +
      'HANGMAN だけは CGAグラフィックスを要求するので Tier 6 待ち — ' +
      '画面が白いのは「描いていない」のではなく「描く先が無い」ためで、要求は記録に残る。' +
      'AIR はPCスピーカーでバッハ (G線上のアリア) を演奏するデモ (キーで停止)。' +
      'DEBUG (lDebug) も入っているので、DEBUG AIR.COM → u 100 で中身を逆アセンブルできる。',
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
    // ネットワーク有効時 (?net=) だけ script の続きとして流す:
    // パケットドライバ常駐 → mTCP設定 → DHCP → 疎通の証明にping。
    // 各段の when は「前の段が画面に残した文言」— ドライバのMAC表示、
    // DHCPの設定書き込み完了、で次へ進む
    netScript: [
      { when: 'A:\\>', send: 'NE2000 0x60 3 0x300\n' },
      { when: 'My Ethernet address', send: 'SET MTCPCFG=A:\\MTCP.CFG\nDHCP\n' },
      { when: 'Settings written', send: 'PING 1.1.1.1\n' },
    ],
    source: 'https://download.freedos.org/1.4/FD14-FloppyEdition.zip',
    sourceLabel: 'FreeDOS 1.4 Floppy Edition',
    // 配布物の 144m/x86BOOT.img を fd14boot.img として置く
    file: 'fd14boot.img (配布zipの 144m/x86BOOT.img)',
  },
  {
    group: 'OSライブラリ',
    id: 'linux',
    label: 'Linux 6.18',
    sub: 'bzImage + initramfs',
    // コンソールはシリアル (ttyS0) で、VGAテキストとは描画の作法が丸ごと違う。
    // 端末は terminal.js ではなく ansi.js、回すのは machine.js ではなく
    // linux-machine.js (ワーカー)。**選び方と見た目はELKSと同じ**にする。
    kind: 'linux',
    status: 'ok',
    // ライブラリに並べる条件: どれか1つでも取れれば起動できる
    probe: ['./vmlinuz-lts', './vmlinux-lts.gz'],
    note:
      'BIOSは通さず、カーネルを直接ロードして32bitエントリへ飛ぶ。' +
      'コンソールはシリアル (ttyS0)。選ぶと電源ONからカーネルログの流れる' +
      '本物のフル起動 (〜20秒)。途中の状態は「スナップショット書出/復元」で残せる。' +
      'シェルが出たら ls / cat /proc/cpuinfo / snake / vi が叩ける。',
    // イメージ (vmlinuz-lts / initramfs-mini) は同梱しない (配布物のため)。
    // 無いときの案内は linux-machine.js が fetch 失敗時に出す
  },
  {
    group: 'メディア',
    id: 'open',
    label: 'イメージを開く…',
    kind: 'open',
    note:
      'フロッピー/ディスクイメージ (.img、先頭512バイトがブートセクタのもの) を' +
      '手元から選んで起動する。画面へのドラッグ&ドロップでも同じ。' +
      'スナップショット (.rx86snap) もここから読み戻せる。',
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
