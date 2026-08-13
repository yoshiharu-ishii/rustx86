// wsslirp (ユーザーモードNAT) へのWebSocket結線。
//
// 境界プロトコルは「1バイナリメッセージ = 1 Ethernetフレーム」だけ。
// ゲストの仮想NE2000が吐いたフレームをそのまま流し、向こうから来た
// メッセージをそのまま受信リングへ入れる。TCP終端もDHCPもDNSも
// 全部wsslirpd側の仕事で、こちらはフレームの運び屋に徹する。
//
// 機械は決定的な世界に住んでいるので、ネットワークの非決定さ (いつ届くか)
// はこのファイルで止める: 届いたフレームは inbox に溜め、機械のスライス
// 境界 (pump) でまとめて注入する。

export class NetLink {
  #ws = null;
  #inbox = [];
  /** 'connecting' | 'up' | 'down' */
  state = 'connecting';
  onState = null;

  /** @param {string} url 例 ws://127.0.0.1:8087/net?token=dev */
  constructor(url) {
    this.#ws = new WebSocket(url);
    this.#ws.binaryType = 'arraybuffer';
    this.#ws.onopen = () => this.#setState('up');
    this.#ws.onclose = () => this.#setState('down');
    this.#ws.onerror = () => this.#setState('down');
    this.#ws.onmessage = e => this.#inbox.push(new Uint8Array(e.data));
  }

  #setState(s) {
    this.state = s;
    this.onState?.(s);
  }

  /** スライス境界で呼ぶ: 出ていくフレームを送り、届いたフレームを注入する */
  pump(emu) {
    for (;;) {
      const f = emu.net_take_frame();
      if (!f.length) break;
      if (this.state === 'up') this.#ws.send(f);
      // 繋がっていなければ捨てる — ケーブルの抜けたNICと同じ顔
    }
    for (const f of this.#inbox) emu.net_inject_frame(f);
    this.#inbox.length = 0;
  }

  close() {
    this.#ws?.close();
  }
}
