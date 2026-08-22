// OPL2 (Adlib) の音を鳴らす AudioWorklet。
//
// メインが core から引き出した i16 の塊 (port 経由) をリングに積み、
// 128 フレームごとに吐く。足りなければ無音 (鳴り続けるより途切れる方がまし)。
// 大きな塊を貯めないよう、リングは 1 秒分で頭打ち
class OplPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = [];
    this.queued = 0;
    this.port.onmessage = (e) => {
      const s = e.data; // Int16Array
      if (this.queued > sampleRate) return; // 1 秒以上溜まっていたら捨てる
      this.queue.push({ s, at: 0 });
      this.queued += s.length;
    };
  }
  process(_inputs, outputs) {
    const out = outputs[0][0];
    let i = 0;
    while (i < out.length && this.queue.length) {
      const head = this.queue[0];
      while (i < out.length && head.at < head.s.length) {
        out[i++] = head.s[head.at++] / 32768;
      }
      if (head.at >= head.s.length) this.queue.shift();
    }
    this.queued -= i;
    for (; i < out.length; i++) out[i] = 0;
    return true;
  }
}
registerProcessor('opl-player', OplPlayer);
