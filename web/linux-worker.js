// Linux を回すワーカー。
//
// **メインスレッドで回すと、起動の1〜2分ページが固まる**。ボタンも押せず、
// 端末も更新されない。ワーカーに追い出せば、CPUがぶん回っていても画面は
// 60fpsで更新でき、キー入力も届く。wasm はここで初期化して抱える。
//
// メインとの約束 (postMessage):
//   受信: {type:'boot', kernel, initrd, cmdline, ramMb}  起動する
//         {type:'input', bytes}                          シリアルへ流す
//         {type:'pause'} / {type:'resume'}
//   送信: {type:'ready'}                     wasm初期化完了
//         {type:'serial', bytes}             コンソール出力 (差分)
//         {type:'status', booted, mips, trap} 状態 (定期)
//         {type:'trap', reason}              未実装で停止

import init, { Emulator } from './pkg/rustx86_wasm.js';

let emu = null;
let running = false;
let instrs = 0;
let lastMeasure = 0;

await init();
postMessage({ type: 'ready' });

self.onmessage = (e) => {
  const msg = e.data;
  switch (msg.type) {
    case 'boot': {
      emu = Emulator.from_bzimage(
        new Uint8Array(msg.kernel),
        msg.initrd ? new Uint8Array(msg.initrd) : undefined,
        msg.cmdline ?? 'console=ttyS0',
        msg.ramMb ?? 128,
      );
      running = true;
      instrs = 0;
      lastMeasure = performance.now();
      loop();
      break;
    }
    case 'input':
      if (emu) emu.serial_in(new Uint8Array(msg.bytes));
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
  }
};

// 1スライスの命令数。**大きすぎると出力の反応が鈍り、小さすぎると
// ループのオーバーヘッドが効く**。ワーカーなのでメインの60fpsに縛られず、
// 出力を細かく流すため 20ms 相当を狙って動的に調整する
let sliceSize = 2_000_000;

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

  const trap = emu.trap_reason();
  if (trap) {
    running = false;
    postMessage({ type: 'trap', reason: trap });
    return;
  }

  // スライス時間を測って ~20ms に寄せる
  const dt = performance.now() - t0;
  if (dt > 0) {
    const target = 20;
    sliceSize = Math.max(200_000, Math.min(50_000_000, Math.round((sliceSize * target) / dt)));
  }

  // 定期的に状態を報告 (MIPS)
  const now = performance.now();
  if (now - lastMeasure >= 500) {
    const mips = instrs / (now - lastMeasure) / 1000;
    postMessage({ type: 'status', mips });
    instrs = 0;
    lastMeasure = now;
  }

  // 次のスライス。setTimeout(0) でメッセージを捌く隙を作る
  setTimeout(loop, 0);
}
