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
    label: 'ELKS',
    sub: 'フロッピー1枚のUNIX',
    image: './fd2880.img',
    status: 'ok',
    note:
      'ログイン名は root。tetris / invaders / ttypong / sl / matrix が入っている。' +
      'BIOSはほとんど使わず、8042とテキストVRAMを直接叩くOSである。' +
      'ネットワーク有効時 (?net=) は起動時にktcpが自動で上がり、' +
      'urlget http://example.com/ で本物のインターネットからHTMLが引ける ' +
      '(pingコマンドはELKSには無い。telnetd/ftpdも動いている)。',
    source: 'https://github.com/ghaerr/elks/releases',
    sourceLabel: 'ELKS のリリース',
    file: 'fd2880.img',
  },
  {
    group: 'OSライブラリ',
    id: 'freedos',
    label: 'FreeDOS',
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
    // パケットドライバ常駐 → mTCP設定 → DHCP でアドレス取得まで。
    // PING は自分で打つ楽しみに残す (\ が要らないのでどのキーボードでも打てる)
    netScript: [
      { when: 'A:\\>', send: 'NE2000 0x60 3 0x300\n' },
      { when: 'My Ethernet address', send: 'SET MTCPCFG=A:\\MTCP.CFG\nDHCP\n' },
    ],
    source: 'https://download.freedos.org/1.4/FD14-FloppyEdition.zip',
    sourceLabel: 'FreeDOS 1.4 Floppy Edition',
    // 配布物の 144m/x86BOOT.img を fd14boot.img として置く
    file: 'fd14boot.img (配布zipの 144m/x86BOOT.img)',
  },
  {
    group: 'OSライブラリ',
    id: 'linux',
    label: 'Linux (コンソール)',
    sub: '画面はシリアル端末 (ttyS0) — カーネルの文字出力をブラウザの端末が描く',
    // コンソールはシリアル (ttyS0) で、VGAテキストとは描画の作法が丸ごと違う。
    // 端末は terminal.js ではなく ansi.js、回すのは machine.js ではなく
    // linux-machine.js (ワーカー)。**選び方と見た目はELKSと同じ**にする。
    kind: 'linux',
    status: 'ok',
    // ライブラリに並べる条件: どれか1つでも取れれば起動できる
    probe: ['./vmlinuz-lts', './vmlinux-lts.gz'],
    note:
      'BIOSは通さず、カーネルを直接ロードして32bitエントリへ飛ぶ。' +
      '**画面はシリアル端末 (ttyS0)**: カーネルは文字を送るだけで、それを描くのは' +
      'ブラウザ側の端末 (ansi.js)。ゲストから見て画面装置は存在しない。' +
      '選ぶと電源ONからカーネルログの流れる本物のフル起動 (〜20秒)。' +
      '途中の状態は「スナップショット書出/復元」で残せる。' +
      'シェルが出たら ls / cat /proc/cpuinfo / snake / vi が叩ける。' +
      '下の「Linux (フレームバッファ)」とはカーネルもルートFSも同じで、違うのは画面だけ。',
    // イメージ (vmlinuz-lts / initramfs-mini) は同梱しない (配布物のため)。
    // 無いときの案内は linux-machine.js が fetch 失敗時に出す
  },
  {
    group: 'OSライブラリ',
    id: 'linux-fb',
    label: 'Linux (フレームバッファ)',
    sub: '画面は 640×480 の画素 — カーネル自身 (efifb/fbcon) が描く',
    kind: 'linux',
    // **同じカーネル・同じルートFS、違うのは画面だけ。** 起動時に
    // zero page でリニアFBを申告し (efifb)、console= の最後を tty0 にする。
    // initはそれを見てシェルを /dev/tty1 (fbcon) に出す。キーはPS/2経由
    fb: true,
    status: 'ok',
    probe: ['./vmlinuz-lts', './vmlinux-lts.gz'],
    note:
      '**画面は画素**: 起動時にLinuxへ 640×480×24bpp のリニアフレームバッファを申告し、' +
      'カーネルの efifb が掴んで fbcon がカーネル自身のフォントで描く。ブラウザはその' +
      '画素をそのまま映すだけで、文字を解釈しない。シェルも画面側 (tty1) に出て、' +
      'キーはPS/2キーボードとしてカーネルへ入る (シリアル側にはバナーが1行流れるだけ)。' +
      '上の「Linux (コンソール)」とはカーネルもルートFSも同じで、**違うのは画面装置の有無だけ**。' +
      'fbcon が描く分だけ起動の命令数が変わる (970M → 1160M) ので、別の機械として並べている。' +
      'この先の fbdev アプリ (links2 -g / Nano-X / SDL) はこちらで動かす。',
  },
  {
    group: 'メディア',
    id: 'open',
    label: 'イメージを開く…',
    kind: 'open',
    note:
      '手元のファイルから起動する。**拡張子では絞らない** — 何であるかは' +
      '中身の印で決める: ブートセクタ (末尾の 0x55AA) ならディスク、' +
      'ELF か "HdrS" ならLinuxカーネル (initramfs はページの隣から借りる)、' +
      'スナップショット (.rx86snap) ならその状態へ戻る。' +
      'スタート画面へのドラッグ&ドロップでも同じ。',
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

/**
 * Linux機に載せられるルートFS (initramfs)。実物は `web/` の隣に置く。
 *
 * **なぜ画面に出すのか** — つまみをURL (`?initrd=`) にしか置かないと、
 * 「なぜこの機械はgccが使えるのか」「なぜRAMが256MBなのか」が画面から消え、
 * 後で自分の設定の理由が分からなくなる。**選べるものが並んでいれば、
 * 選ばなかった方も含めて理由が見える。**
 *
 * `ram` は空なら自動 (initrdの展開後の大きさから決める。linux-machine.js)。
 */
export const ROOTFS = [
  {
    name: 'initramfs-mini',
    label: 'ミニ (RAM)',
    sub: 'busybox + snake/vi',
    initrd: 'initramfs-mini',
    note: '4MB。まっすぐシェルに出る既定のルートFS。128MBで動く',
  },
  {
    name: 'disk-gcc',
    label: 'gcc入り (ディスク)',
    sub: 'squashfs + overlay',
    initrd: 'initramfs-mini',
    // .gz は**輸送路の圧縮** — ホスト側で1回だけ解いてからvdaに挿す。
    // squashfs自体は無圧縮 (ゲストのCPUに読むたび解凍させると、
    // cold readのsysが0.9s→15.6sに化ける。実測は make-gcc-disk.sh の注釈)
    disk: 'disk-gcc.img.gz',
    note:
      'virtio-blkの/dev/vdaにgcc一式 (34MBのsquashfs)。ミニのinitが見つけて' +
      '移り住む。読んだ分しかRAMに載らないので**128MBで済む** — こちらが本命。' +
      'tools/images/sh/make-gcc-disk.sh で作る',
  },
  {
    name: 'disk-x',
    label: 'X入り (ディスク)',
    sub: 'Xorg fbdev + icewm、dillo/w3m/links、1024×768',
    initrd: 'initramfs-mini',
    disk: 'disk-x.img.gz',
    // X は広い方が使える。xorg.conf はモード無指定なので fb の解像度にそのまま乗る
    lfb: { width: 1024, height: 768 },
    note:
      'virtio-blkの/dev/vdaに Xorg (fbdevドライバ) + evdev + **icewm** + xterm、' +
      'ブラウザ3種 (**dillo** / w3m / links、CA束つき — NICを挿せば https も)、' +
      'feh / xfe / xclock・xeyes・xcalc / xboard。gz 51MB。' +
      '「Linux (フレームバッファ)」機で起動して `startx` と打つと、' +
      'efifb の **1024×768** に X が上がる (GPUは無いので全部ソフトウェア描画)。' +
      'マウスは画面の上に居る間だけゲストへ届く (捕獲も脱出キーも無し、Esc は vi のもの)。tools/images/sh/make-x-disk.sh で作る',
  },
  {
    name: 'initramfs-gcc',
    label: 'gcc入り (RAM)',
    sub: 'initramfsに全部載せ',
    initrd: 'initramfs-gcc',
    note:
      '34MB (展開84MB)。ディスク無しでも動く保険の形。展開した中身が' +
      'そのままtmpfsに載るので、RAMは自動で256MBになる。' +
      'tools/images/sh/make-gcc-initramfs.sh で作る',
  },
];

/** @param {Status} s */
export function statusLabel(s) {
  return { ok: '動く', partial: '途中まで', todo: 'これから' }[s] ?? s;
}
