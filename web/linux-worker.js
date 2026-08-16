// Linux を回すワーカー。
//
// **メインスレッドで回すと、起動の1〜2分ページが固まる**。ボタンも押せず、
// 端末も更新されない。ワーカーに追い出せば、CPUがぶん回っていても画面は
// 60fpsで更新でき、キー入力も届く。wasm はここで初期化して抱える。
//
// メインとの約束 (postMessage):
//   受信: {type:'boot', kernel, initrd, cmdline, ramMb, mac?, disk?}  カーネルから起動する
//                                     (macがあればRTL8029、diskがあればvirtio-blkを挿す)
//         {type:'boot', snapshot}                        起動済み控えから復元する
//         {type:'save'}                                  今の状態を丸ごと控えて返す
//         {type:'load', bytes}                           控えた状態へ戻す
//         {type:'input', bytes}                          シリアルへ流す
//         {type:'net-rx', frames}                        届いたEthernetフレーム
//         {type:'pause'} / {type:'resume'}
//         {type:'dbg', id, method, args}                 デバッガの覗き見RPC (下記)
//   送信: {type:'ready'}                     wasm初期化完了
//         {type:'serial', bytes}             コンソール出力 (差分)
//         {type:'net-tx', frames}            ゲストが送ったEthernetフレーム
//         {type:'status', booted, mips, trap} 状態 (定期)
//         {type:'trap', reason}              未実装で停止
//         {type:'dbg-result', id, result}    RPCの返事
//         {type:'dbg-stop', why}             見張り (ブレークポイント等) が機械を止めた

import init, { Emulator } from './pkg/rustx86_wasm.js';
import { setupJit, pumpJit, resetJit } from './jit-runtime.js';

let emu = null;
let running = false;
/** ゲストの時計 (仮想ミリ秒) と、その基準になった実時刻。
    **仮想時間は実時間を追い越してはいけない** — 追い越すと、ゲストの
    「1秒に1回」(pingやTCPの再送) が実時間では毎秒何百回にもなる。
    実際に ping 1.1.1.1 が本物のインターネットへ洪水になった */
let virtualMs = 0;
let clockT0 = 0;
/** 外から届いたフレーム。**スライス境界でまとめて注入する** —
    ネットワークの非決定さ (いつ届くか) をここで止める (netlink.jsと同じ理屈) */
let netInbox = [];
let instrs = 0;
let lastMeasure = 0;
let lastTone = 0;

const wasmExports = await init();
let jitOn = false;
/** 初回enable時にimport束とテーブルを結線する (再bootでは張り直す) */
function jitEnable() {
  setupJit(emu, wasmExports);
  emu.jit_enable();
  jitOn = true;
}
postMessage({ type: 'ready' });

self.onmessage = (e) => {
  const msg = e.data;
  switch (msg.type) {
    case 'boot': {
      if (msg.snapshot) {
        // 起動済みスナップショットから復元 (数秒)。
        // 送信済みのシリアル出力は履歴なので控えに入っていない —
        // 空画面のままだと死んで見えるので、改行を1つ流して
        // シェルにプロンプトを出させる
        emu = Emulator.from_snapshot(new Uint8Array(msg.snapshot));
        emu.serial_in(new TextEncoder().encode('\n'));
      } else {
        emu = Emulator.from_bzimage(
          new Uint8Array(msg.kernel),
          msg.initrd ? new Uint8Array(msg.initrd) : undefined,
          msg.cmdline ?? 'console=ttyS0',
          msg.ramMb ?? 128,
        );
        // NICを挿すのは電源を入れるこの瞬間だけ (VGA機と同じ)。
        // Linuxは起動時にしかPCIを数えないので、後から挿しても見えない
        if (msg.mac) emu.net_attach(new Uint8Array(msg.mac));
        // ディスクも同じ瞬間。initramfs-miniのinitがvdaを見つけて移り住む
        if (msg.disk) emu.blk_attach(new Uint8Array(msg.disk));
        // RTCを実時刻に合わせる (TLSの証明書検証は正しい時計が前提)。
        // スナップショット復元はカーネルがもう時計を読んだ後なので合わせない
        emu.set_rtc_unix(Date.now() / 1000);
      }
      // JIT (F1d wasm)。電源投入時の初期値 — 実行中の切替は 'jit' メッセージ
      resetJit();
      jitOn = false;
      if (msg.jit) jitEnable();
      netInbox = [];
      running = true;
      instrs = 0;
      lastMeasure = performance.now();
      virtualMs = 0;
      clockT0 = lastMeasure;
      loop();
      break;
    }
    case 'input':
      if (emu) emu.serial_in(new Uint8Array(msg.bytes));
      break;
    case 'net-rx':
      for (const f of msg.frames) netInbox.push(new Uint8Array(f));
      // **寝ている間に届いたら、待たずに起こす。**
      // 時計の轡で最大50ms寝るので、そのままだと届いたフレームが最大50ms
      // 待たされる。**遅延の定数項の正体がこれだった** — 実測で ping の
      // RTT が 20.2ms → 13.7ms (-32%、刻み30万命令。tools/webtest/netlat.mjs)。
      // 轡は壊れない: 早送りで進めた分は既に仮想時間に計上済みで、
      // 早く起きても借りは増えない (寝るのを短く切り上げるだけ)
      wakeNow();
      break;
    case 'save': {
      if (!emu) break;
      // スライスの切れ目で呼ばれるので、機械は命令境界の綺麗な姿
      const bytes = emu.save_state();
      postMessage({ type: 'state', bytes: bytes.buffer }, [bytes.buffer]);
      break;
    }
    case 'load': {
      if (!emu) break;
      emu.load_state(new Uint8Array(msg.bytes));
      // 起動スナップショットと違って**改行は突かない** — 画面には保存時までの
      // 表示がそのまま残っているので、突くと復元のたびに空プロンプトが積もる
      postMessage({ type: 'loaded' });
      break;
    }
    case 'jit':
      // 実行中のon/off (比較実験の外部フラグ)。offは据え付けごと捨てる —
      // on/offどちらで走っても決定性 (命令数・出力) が不変なのが門番
      if (msg.on && !jitOn) jitEnable();
      if (!msg.on && jitOn) {
        emu.jit_disable();
        jitOn = false;
      }
      postMessage({ type: 'jit', on: jitOn });
      break;
    case 'pause':
      running = false;
      break;
    case 'resume':
      if (emu && !running) {
        running = true;
        lastMeasure = performance.now();
        loop();
      }
      break;
    case 'dbg': {
      // デバッガの覗き見RPC。**メソッド名は許可リスト** — postMessage の中身は
      // 信用しない (任意メソッド呼び出しの入口にしない)。
      // 走っている間もスライスの切れ目で捌かれるので、live表示もできる
      postMessage({ type: 'dbg-result', id: msg.id, result: dbgDispatch(msg.method, msg.args ?? []) });
      break;
    }
  }
};

/** デバッガRPCで呼んでよいメソッド。Emulator のデバッグAPIと1対1 */
const DBG_METHODS = new Set([
  'cpu_json', 'watches_json', 'trace_json', 'read_mem',
  'set_break', 'watch_mem', 'watch_io', 'clear_debug',
  'step_one', 'take_stop', 'is_stopped', 'set_counting', 'record_trace',
]);

function dbgDispatch(method, args) {
  if (!emu || !DBG_METHODS.has(method)) return null;
  try {
    const r = emu[method](...args);
    return r === undefined ? true : r;
  } catch (err) {
    // 覗き見の失敗で機械もワーカーも殺さない。null は「読めなかった」の顔
    return null;
  }
}

// 1スライスの命令数。
//
// **大きすぎると入力の反応が鈍る。** run_slice はwasmの中でブロックするので、
// 走っている間に来たキーは次のスライスまで届かない。しかもアイドル (HLT) 中は
// 命令を実行しないぶんスライスが速く終わり、動的調整が上限まで膨らむ —
// 上限を50Mにしていたら、キーを打ってからエコーが返るまで数秒かかった。
// 目標8ms・上限5Mに抑えて、応答性を優先する
let sliceSize = 1_000_000;

// 1ゲストミリ秒に相当する仮想命令数。
//
// このエミュレータの時間は「1命令 ≒ 一定時間」の勘定で、PITの入力
// 1.193182 MHz が 64命令 (INSTRUCTIONS_PER_TICK) に1クロック刻まれる。
// つまりゲストの1秒 = 1,193,182 × 64 ≒ 76.4M 仮想命令。
//
// コア側にアイドル (HLT) の早送りが入ったので、run_slice の予算 (仮想時間) は
// 暇なら一瞬で消化される。**そのまま次を回すとゲストの時計だけが実時間の
// 何百倍も速く進む** — DOSの時計が暴走し、snakeが目で追えなくなる。
// 実時間との釣り合いはここ (ランナー) の仕事:
// **飛ばした仮想時間 (take_idle_skipped) だけ、実時間で待ってから次を回す。**
// 忙しい実行は1命令も飛ばさないので待ちゼロ = 今までどおり全力で回る。
// 「halted で終わったかどうか」で判別しないのは、スライスの切れ目が
// たまたま割り込みハンドラの中だと全力モードに化けて、snake が2〜3倍速に
// なったため (実際になった)。飛ばした量そのものを数えるのが正確である
const INSTR_PER_GUEST_MS = (1_193_182 * 64) / 1000;

// 次スライスの予約 (クランプ回避)。
//
// `setTimeout(loop, 0)` はHTML仕様の「ネストが5段を超えたタイマは最低4ms」に
// 当たる。スライス目標が8msなので **8ms働いて4ms強制休憩 = デューティ67%** —
// ブラウザだけheadlessより×1.5遅かった「タブ税」の正体がこれだった。
// MessageChannelのport.postMessageはクランプの無いマクロタスクで、
// キー入力などworkerへのメッセージも間に割り込める (同じFIFOに並ぶ)。
// アイドル時の「実時間で待つ」setTimeoutは意図した待ちなのでそのまま
const wake = new MessageChannel();
wake.port1.onmessage = () => loop();
function scheduleNext() {
  wake.port2.postMessage(0);
}

// **寝ているタイマの札。** 時計の轡で実時間を待っている間にフレームが届いたら、
// 待たずに起こすために覚えておく (下の nap / wakeNow)
let napTimer = null;
/** 実時間で ms だけ寝てから続きを回す */
function nap(ms) {
  napTimer = setTimeout(() => {
    napTimer = null;
    loop();
  }, ms);
}
/** 寝ている最中なら、今すぐ起こす。寝ていなければ何もしない */
function wakeNow() {
  if (napTimer === null) return;
  clearTimeout(napTimer);
  napTimer = null;
  loop();
}

function loop() {
  if (!running || !emu) return;
  const t0 = performance.now();
  let ran = 0;

  // 届いたフレームをスライス境界で注入する (受信リングが詰まっていたら
  // 落ちるが、それは実機でも同じ)
  if (netInbox.length) {
    for (const f of netInbox) emu.net_inject_frame(f);
    netInbox = [];
  }

  try {
    // **返るのは「実際に進んだ量」** — 送信フレームが出ると早く戻るので、
    // 頼んだ量で勘定するとゲストの時計が速く回る (pitfalls 7 の型)
    ran = emu.run_slice(sliceSize);
    // スライスで焼き上がったブロックを据え付ける (instantiateはJSの仕事)。
    // 据え付くまでの間はインタプリタが走っている — 退路は常にある
    if (jitOn) pumpJit(emu);
  } catch (err) {
    running = false;
    postMessage({ type: 'trap', reason: 'wasm panic: ' + err });
    return;
  }
  instrs += ran;

  // ゲストが送ったフレームを外へ (1メッセージにまとめ、中身は移送する)
  {
    const frames = [];
    for (;;) {
      const f = emu.net_take_frame();
      if (!f.length) break;
      frames.push(f.buffer);
    }
    if (frames.length) postMessage({ type: 'net-tx', frames }, frames);
  }

  // 出力を流す (差分)
  const out = emu.serial_out();
  if (out.length) {
    // Uint8Array を transferable で渡す (コピーを避ける)
    postMessage({ type: 'serial', bytes: out.buffer }, [out.buffer]);
  }

  // PCスピーカー。値はスライスごとにポーリングし、**変わったときだけ**報告する
  // (WebAudioはワーカーから触れないのでメインが鳴らす)
  const hz = emu.speaker_tone();
  if (hz !== lastTone) {
    lastTone = hz;
    postMessage({ type: 'tone', hz });
  }

  const trap = emu.trap_reason();
  if (trap) {
    running = false;
    postMessage({ type: 'trap', reason: trap });
    return;
  }

  // デバッガの見張り (ブレークポイント等) が機械を止めたら、ループを降りて
  // メインへ理由を知らせる。見張っていなければ真偽値1つの判定
  if (emu.is_stopped()) {
    running = false;
    postMessage({ type: 'dbg-stop', why: emu.take_stop() });
    return;
  }

  const now = performance.now();
  const dt = now - t0;
  // このスライスで早送りが飛ばした仮想時間 (ミリ秒)
  const skippedMs = emu.take_idle_skipped() / INSTR_PER_GUEST_MS;
  const idle = skippedMs > dt;

  // --- ゲストの時計を実時間に繋ぎ止める ---
  //
  // 仮想時間 = 実行した命令ぶん + 早送りが飛ばしたぶん。起動中 (全力で忙しい)
  // は実測が想定 (~76 MIPS) より遅いので先行せず、**起動の速さは落ちない**。
  // 先行するのはHLTの早送りが利くアイドル時で、以前は「50msだけ寝て残りの
  // 借りは忘れる」だったため、ゲストの1秒が実時間の数十msになっていた。
  // 溜まった先行ぶんは下の待ちで実時間に返す。
  //
  // 逆向き (実時間が先行) は放置でよいが、**上限を置く** — 一時停止や重い
  // ホストで実時間が何分も先行した後、その分をまとめて回すと結局洪水になる
  // **早送りぶんを足してはいけない。** `run_slice(n)` は「TSCが n 進むまで」
  // 回る契約で、HLTの早送りもTSCを進めるので、飛ばした時間は既に sliceSize の
  // 中に入っている。両方足すと仮想時間が実際の倍のペースで溜まり、その借りを
  // 実時間で返すので **ゲストの1秒が実時間2秒になる** (sleep 5 が10秒かかった)。
  // skippedMs はアイドル判定 (上の idle) にだけ使う
  virtualMs += ran / INSTR_PER_GUEST_MS;
  const realMs = now - clockT0;
  if (virtualMs < realMs - 100) virtualMs = realMs - 100;
  const aheadMs = virtualMs - realMs;

  // 定期的に状態を報告 (MIPS)。アイドル中の数字は「時計を流しただけ」なので
  // idle を添えて、見せ方は画面側に任せる
  if (now - lastMeasure >= 500) {
    const mips = instrs / (now - lastMeasure) / 1000;
    postMessage({ type: 'status', mips, idle });
    instrs = 0;
    lastMeasure = now;
  }

  if (idle) {
    // アイドル: 時計の先行ぶんを実時間で返す。50msずつ寝るのは応答性のため
    // (キーはワーカーのメッセージで届き、寝ている間も割り込める)。
    // 返しきるまでは次のループでもここへ来るので、借りは消えない。
    // 次のスライスは短く戻す — 5Mのまま寝ると65ms待ちになり打鍵が鈍る
    sliceSize = Math.round(INSTR_PER_GUEST_MS * 16);
    nap(Math.max(0, Math.min(50, aheadMs)));
    return;
  }

  // 忙しくても時計が先行したら実時間に合わせる (wasmが想定より速いとき)
  if (aheadMs > 8) {
    nap(Math.min(50, aheadMs));
    return;
  }

  // 忙しい: スライス時間を測って ~8ms に寄せ、全力で回す
  if (dt > 0) {
    const target = 8;
    sliceSize = Math.max(100_000, Math.min(5_000_000, Math.round((sliceSize * target) / dt)));
  }

  // 次のスライス。マクロタスク境界を挟むのでメッセージを捌く隙は保たれる
  scheduleNext();
}
