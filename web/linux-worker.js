// Linux を回すワーカー。
//
// **メインスレッドで回すと、起動の1〜2分ページが固まる**。ボタンも押せず、
// 端末も更新されない。ワーカーに追い出せば、CPUがぶん回っていても画面は
// 60fpsで更新でき、キー入力も届く。wasm はここで初期化して抱える。
//
// メインとの約束 (postMessage):
//   受信: {type:'boot', kernel, initrd, cmdline, ramMb}  カーネルから起動する
//         {type:'boot', snapshot}                        起動済み控えから復元する
//         {type:'save'}                                  今の状態を丸ごと控えて返す
//         {type:'load', bytes}                           控えた状態へ戻す
//         {type:'input', bytes}                          シリアルへ流す
//         {type:'pause'} / {type:'resume'}
//         {type:'dbg', id, method, args}                 デバッガの覗き見RPC (下記)
//   送信: {type:'ready'}                     wasm初期化完了
//         {type:'serial', bytes}             コンソール出力 (差分)
//         {type:'status', booted, mips, trap} 状態 (定期)
//         {type:'trap', reason}              未実装で停止
//         {type:'dbg-result', id, result}    RPCの返事
//         {type:'dbg-stop', why}             見張り (ブレークポイント等) が機械を止めた

import init, { Emulator } from './pkg/rustx86_wasm.js';

let emu = null;
let running = false;
let instrs = 0;
let lastMeasure = 0;
let lastTone = 0;

await init();
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
      }
      running = true;
      instrs = 0;
      lastMeasure = performance.now();
      loop();
      break;
    }
    case 'input':
      if (emu) emu.serial_in(new Uint8Array(msg.bytes));
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

function loop() {
  if (!running || !emu) return;
  const t0 = performance.now();

  try {
    emu.run_slice(sliceSize);
  } catch (err) {
    running = false;
    postMessage({ type: 'trap', reason: 'wasm panic: ' + err });
    return;
  }
  instrs += sliceSize;

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

  // 定期的に状態を報告 (MIPS)。アイドル中の数字は「時計を流しただけ」なので
  // idle を添えて、見せ方は画面側に任せる
  if (now - lastMeasure >= 500) {
    const mips = instrs / (now - lastMeasure) / 1000;
    postMessage({ type: 'status', mips, idle });
    instrs = 0;
    lastMeasure = now;
  }

  if (idle) {
    // アイドル: 飛ばした時間から実際に使った時間を引いた分だけ実時間で待つ。
    // CPUはこの間まったく回らない (キーはワーカーのメッセージで届く)。
    // 次のスライスは短く戻す — 5Mのまま寝ると65ms待ちになり打鍵が鈍る
    sliceSize = Math.round(INSTR_PER_GUEST_MS * 16);
    setTimeout(loop, Math.min(50, skippedMs - dt));
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
