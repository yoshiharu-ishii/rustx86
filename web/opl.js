// OPL2 (Adlib) の描き手。core の合成器から「実時間で経った分」のサンプルを引き出し、
// AudioWorklet (opl-worklet.js) へ流す。PC スピーカー (speaker.js) とは別系統。
//
// - AudioContext はユーザー操作まで鳴らせないので、最初のジェスチャで開く
// - 先読みは ~100ms: フレーム (16ms) ごとに「再生位置 + 100ms」まで埋める。
//   これ以上貯めると曲の出だしが遅れ、少ないと途切れる
const LEAD_SEC = 0.1;

export class Opl {
  #ctx = null;
  #node = null;
  #written = 0; // 送り済みのサンプル数
  #ready = false;
  #rate = 0;

  async unlock() {
    if (this.#ctx) {
      if (this.#ctx.state === 'suspended') this.#ctx.resume();
      return;
    }
    this.#ctx = new AudioContext();
    this.#rate = this.#ctx.sampleRate;
    try {
      await this.#ctx.audioWorklet.addModule('./opl-worklet.js');
      this.#node = new AudioWorkletNode(this.#ctx, 'opl-player', { outputChannelCount: [1] });
      this.#node.connect(this.#ctx.destination);
      this.#ready = true;
    } catch (e) {
      console.warn('OPL: AudioWorklet を用意できない', e);
    }
    if (this.#ctx.state === 'suspended') this.#ctx.resume();
  }

  /** 合成器のサンプルレート (core に伝える)。まだ開いていなければ 0 */
  get rate() {
    return this.#rate;
  }

  /**
   * フレームごとに呼ぶ。render(n) は core から n サンプルの Int16Array を返す関数
   * @param {(n:number)=>Int16Array} render
   */
  pump(render) {
    if (!this.#ready || this.#ctx.state !== 'running') return;
    const target = (this.#ctx.currentTime + LEAD_SEC) * this.#rate;
    if (this.#written === 0) this.#written = this.#ctx.currentTime * this.#rate;
    const need = Math.floor(target - this.#written);
    if (need <= 0) return;
    const n = Math.min(need, this.#rate); // 1 秒以上は一度に取らない (タブ復帰時)
    const s = render(n);
    if (!s || !s.length) return;
    this.#node.port.postMessage(s, [s.buffer]);
    this.#written += s.length;
  }

  /** 機械を止める/替えるとき: 送り位置を捨てる (次は今から)。音はリングが尽きれば止まる */
  mute() {
    this.#written = 0;
  }
}
