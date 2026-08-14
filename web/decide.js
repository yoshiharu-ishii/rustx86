// 画面の**判断**だけを集めた場所。DOMもwasmも触らない純粋な関数で、
// 「何を落とされたか」「どのNICが挿さるか」「スクリプトを流すか」を決める。
//
// ## なぜ切り出すか
//
// UIのバグは見た目ではなく**判断**を間違えて起きる。実際に踏んだのは:
//
// - 再起動で自動起動スクリプトを流し直さず、FreeDOSが言語選択で止まった
// - vmlinux を拡張子で弾き、ディスクとして起動しようとして失敗した
// - リンクが死んでいるのにDHCPを打たせ、30秒の沈黙を見せた
//
// どれも画面を見なくても真偽を確かめられる話である。ここに集めておけば
// `node --test` で押さえられる (実ブラウザを持ち出さずに済む)。
// 配線 (どの要素をどう出すか) は main.js の仕事のまま。

/**
 * Linuxカーネルか。**先頭のELF印**(vmlinux)か、
 * **setupヘッダの "HdrS"**(bzImage、0x202固定)で見分ける。
 * どちらもブートセクタとは形が違うので、取り違えようがない
 */
export function isKernel(b) {
  const elf = b[0] === 0x7f && b[1] === 0x45 && b[2] === 0x4c && b[3] === 0x46;
  const hdrs =
    b.length > 0x206 &&
    b[0x202] === 0x48 && b[0x203] === 0x64 && b[0x204] === 0x72 && b[0x205] === 0x53;
  return elf || hdrs;
}

/**
 * 起動できるディスクか。**先頭512バイトの末尾にある 0x55AA** が、
 * 1981年から変わっていない「ここから起動してよい」の印である
 * (core側も同じ検査をする。こちらで見るのは日本語で理由を言うため)
 */
export function isBootable(b) {
  return b.length >= 512 && b[510] === 0x55 && b[511] === 0xaa;
}

/** 繋ぎ先URLにトークンを足す (空なら何もしない) */
export function withToken(url, token) {
  if (!token) return url;
  return `${url}${url.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}`;
}

/**
 * `?net=` の繋ぎ先 (無指定と `?net=off` は null)。
 * `?net=1` は手元のSLiRP backend、`?net=off` は「挿さずに起動する」
 * @param {string} search location.search
 * @param {string} defaultUrl
 */
export function netUrlFromQuery(search, defaultUrl) {
  const q = new URLSearchParams(search);
  const net = q.get('net');
  if (!net || net === 'off') return null;
  return withToken(net === '1' ? defaultUrl : net, q.get('nettoken'));
}

/** `?net=off` が指定されているか (既定で繋ぐのをやめる合図) */
export function netOff(search) {
  return new URLSearchParams(search).get('net') === 'off';
}

/**
 * この機械に挿さるNIC。**バスがOSの世代を決める** —
 * ISAを知らないOSにISAのカードを挿しても見えない (逆も同じ)
 * @param {boolean} isLinux 32bit Linux (PCIしか見ない) か
 */
export function nicFor(isLinux) {
  return isLinux
    ? { label: 'RTL8029 (PCI)', usable: true }
    : { label: 'NE2000 (ISA 0x300)', usable: true };
}

/**
 * 起動スクリプト。**線が生きているときだけ** netScript の続きも流す。
 * リンクが死んでいるのにDHCPを打たせると、30秒待って諦めるのを
 * 黙って見せることになる (カードはあるがケーブルが繋がっていない状態)
 * @param {object|null} m マシン定義
 * @param {boolean} linkUp SLiRP backendと繋がっているか
 */
export function scriptFor(m, linkUp) {
  if (!m?.script) return m?.script;
  return linkUp && m.netScript ? [...m.script, ...m.netScript] : m.script;
}

/**
 * キーボードから来た文字をゲストへ渡す形に直す。
 * **¥ は \ として届ける** — MacのJIS配列は \ が素直に打てないが、
 * 日本語DOSではそもそもパス区切り0x5Cの字形が「¥」だった。
 * ¥キーで `A:\>` のパスが打てるのは、歴史的にはむしろ正しい姿である
 */
export function guestChar(ch) {
  return ch === '¥' ? '\\' : ch;
}

/**
 * 要素の出し入れ。**`el.hidden = x` を使ってはいけない。**
 *
 * `hidden` は HTMLElement のプロパティで、**SVGElement には無い**。
 * SVGに代入するとJSの変数が生えるだけで属性は変わらず、しかも読み返すと
 * その変数が返るので**辻褄が合って気づけない** (一時停止の絵がずっと
 * 差し替わらなかった原因。読み返す検証をすり抜けた)。
 * 属性で操作すれば、HTMLでもSVGでも同じように効く。
 */
export function setHidden(el, hidden) {
  el.toggleAttribute('hidden', !!hidden);
}

/** 見た目の好みの並び。**押すたびにこの順で回る** */
export const THEMES = ['system', 'dark', 'light'];

/** 次の好み (最後まで行ったら先頭へ) */
export function nextTheme(pref) {
  const i = THEMES.indexOf(pref);
  return THEMES[(i < 0 ? 0 : i + 1) % THEMES.length];
}

/**
 * 好みを実際の明暗に解く。**`system` はここで解いて属性に落とす。**
 * CSS側で prefers-color-scheme を見ると同じ色の並びを2回書くことになり、
 * 片方だけ直す事故が起きる。解くのを1箇所に寄せれば、色の定義は
 * `:root` (暗) と `:root[data-theme="light"]` (明) の2つで済む
 * @param {string} pref 覚えている好み
 * @param {boolean} systemLight OSが明るい方を望んでいるか
 */
export function resolveTheme(pref, systemLight) {
  if (pref === 'dark' || pref === 'light') return pref;
  return systemLight ? 'light' : 'dark';
}

/**
 * 右クリックのメニューで**今できること**。
 *
 * コピーは**選んでいるときだけ** — どこのアプリでもそうであるように。
 * 以前は選んでいなければ画面全体を取っていたが、それを貼り戻すと
 * 起動ログが丸ごとコマンドとして流れ込む。画面ぜんぶが欲しいときは
 * 「ログを保存」が受け持つ。
 * イメージを開くのはドラッグと同じ条件にする — 走っている機械に
 * 別のディスクを差し込めてしまうと、画面と中身が食い違う
 * @param {boolean} hasGuest 機械が載っているか
 * @param {boolean} hasSelection 画面の一部を選んでいるか
 * @param {boolean} canOpen イメージを受け付ける状態か (acceptsDrop と同じ)
 */
export function menuAbility(hasGuest, hasSelection, canOpen) {
  return { copy: hasSelection, paste: hasGuest, open: canOpen };
}

/**
 * 貼り付けで**今いくつ渡すか**。
 *
 * キーの道は2段ある。8042の行列 (まだゲストへ配っていないスキャンコード) と、
 * BIOSの環 (16枠、ゲストが読むまで空かない)。**律速は環の方**で、
 * 8042は64命令ごとに1バイト出せるから実質詰まらない。
 *
 * ここを2度外している:
 *
 * - 8042の行列が空になるまで待つ → 出しては止まるカクカク
 * - 毎刻み1〜2文字に絞る → タイプライターのようにちょろちょろ
 *
 * 正しいのは**環の空きだけを見て、その分を一度に渡す**こと。配送中の分も
 * 席を取るので数に入れる。席が無ければ0を返し、ゲストが読むまで待つ
 * (溢れさせると実機と同じく捨てられる — 画面全文を貼って数十文字しか
 * 届かなかったのはこれ)。1枠だけ残すのは、環が満杯になると
 * 「空」と見分けが付かなくなる作りのため
 * @param {number} ringRoom BIOSの環の空き枠
 * @param {number} inflight まだ配り終えていない文字数 (8042の行列 ÷ 2)
 * @param {number} waiting 貼り付け待ちの残り文字数
 */
export function pasteChunk(ringRoom, inflight, waiting) {
  const room = ringRoom - inflight;
  if (room < 2) return 0;
  return Math.max(0, Math.min(room - 1, waiting));
}
