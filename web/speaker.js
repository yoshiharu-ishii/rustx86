// PCスピーカーの音をWebAudioで鳴らす。
//
// coreは「今の周波数 (Hz、無音は0)」を返すだけで、音の出し方を知らない —
// 画面 (テキストVRAM) と同じ分業である。こちらはスライスごとにその値を
// 受け取り、変わったときだけオシレータに反映する。
//
// ## ブラウザの作法に2つ付き合う
//
// - **AudioContextはユーザー操作まで鳴らせない** (自動再生ポリシー)。
//   最初のジェスチャで resume() する。それまでの音は黙って落ちる —
//   起動時のBIOSビープより「クリックしたら鳴る」ことの方が大事
// - **矩形波をそのまま最大音量で出すと耳に痛い。** 実機のスピーカーも
//   紙コーン1個の控えめな音なので、ゲインは小さく固定する

const VOLUME = 0.04;

export class Speaker {
  #ctx = null;
  #osc = null;
  #gain = null;
  #hz = 0;

  // AudioContextを作る。ユーザー操作を待つ間はsuspendedのままでよい
  #ensure() {
    if (this.#ctx) return;
    this.#ctx = new AudioContext();
    this.#gain = this.#ctx.createGain();
    this.#gain.gain.value = 0;
    this.#gain.connect(this.#ctx.destination);
    this.#osc = this.#ctx.createOscillator();
    this.#osc.type = 'square';
    this.#osc.connect(this.#gain);
    this.#osc.start();
  }

  /// 最初のユーザー操作 (キー・クリック) から呼ぶ。以降、音が出せる
  unlock() {
    this.#ensure();
    if (this.#ctx.state === 'suspended') this.#ctx.resume();
  }

  /// スライスごとに現在の周波数を流し込む (無音は0)。変化が無ければ何もしない
  update(hz) {
    if (hz === this.#hz) return;
    this.#hz = hz;
    this.#ensure();
    const t = this.#ctx.currentTime;
    if (hz > 0) {
      this.#osc.frequency.setValueAtTime(hz, t);
      // 立ち上がりを数msなだらかにしてクリックノイズを避ける
      this.#gain.gain.cancelScheduledValues(t);
      this.#gain.gain.setTargetAtTime(VOLUME, t, 0.005);
    } else {
      this.#gain.gain.cancelScheduledValues(t);
      this.#gain.gain.setTargetAtTime(0, t, 0.005);
    }
  }

  /// 機械を止める/切り替えるときに黙らせる
  mute() {
    this.update(0);
  }
}
