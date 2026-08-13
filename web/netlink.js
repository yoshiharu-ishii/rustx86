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

/**
 * 状態は3つ。**「一度も繋がらなかった」と「繋がっていたが切れた」を
 * 区別しない** — 使う側から見ればどちらも「線が来ていない」で、
 * やることは同じ (設定を見直すか、デーモンを立てる) である。
 * @typedef {'wait'|'up'|'down'} NetState
 */

export class NetLink {
  #ws = null;
  #inbox = [];
  /** @type {NetState} */
  state = 'wait';
  onState = null;
  /** 人に見せる最後の理由 (繋がらなかったときだけ中身がある) */
  reason = '';

  /** @param {string} url 例 ws://127.0.0.1:8087/net?token=dev */
  constructor(url) {
    this.url = url;
    try {
      this.#ws = new WebSocket(url);
    } catch (e) {
      // URLの形が違うと new の時点で投げる (ws:/wss: 以外など)。
      // **例外にせず状態にして返す** — 呼ぶ側は色を変えるだけで済む
      this.#fail(`URLが不正: ${e.message}`);
      return;
    }
    this.#ws.binaryType = 'arraybuffer';
    this.#ws.onopen = () => this.#setState('up');
    this.#ws.onmessage = e => this.#inbox.push(new Uint8Array(e.data));
    // WebSocketは失敗の理由を教えてくれない (仕様。盗み見対策)。
    // 分かるのは「開く前に閉じた=繋がらなかった」か「開いた後に閉じた=切れた」かだけ
    this.#ws.onclose = () =>
      this.#fail(this.state === 'up' ? '切れた' : '繋がらない (wsslirpdは動いているか)');
    this.#ws.onerror = () => {
      if (this.state === 'wait') this.#fail('繋がらない (wsslirpdは動いているか)');
    };
  }

  #fail(reason) {
    this.reason = reason;
    this.#setState('down');
  }

  #setState(s) {
    if (this.state === s) return;
    this.state = s;
    if (s === 'up') this.reason = '';
    this.onState?.(s, this.reason);
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
    // 自分で閉じるときは onclose の「切れた」を鳴らさない
    if (this.#ws) {
      this.#ws.onclose = null;
      this.#ws.onerror = null;
      this.#ws.close();
    }
    this.#ws = null;
  }
}
