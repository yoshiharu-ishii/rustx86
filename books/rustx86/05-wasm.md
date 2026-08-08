---
title: "ブラウザへ載せる — wasm-bindgenと端末"
---

ここまでは CLI で動きます。ブラウザに持っていくのは `wasm-bindgen` の薄い層だけです。

```
  ブラウザ (JS)                            WASM (Rust)
 ┌────────────────┐                     ┌────────────────────┐
 │  Terminal.js   │  key(code, down)    │   Emulator         │
 │   80x25 描画   │ ───────────────────▶│    └ Machine       │
 │   選択・コピー  │                      │       ├ Cpu       │
 │  スクロールバック│◀────────────────── │       ├ Devices   │
 └────────────────┘  text_vram_ptr()    │       │  ├ Pic     │
        ▲             (コピーなし)         │       │  ├ Pit    │
        │                                │       │  ├ Kbd8042│
 ┌────────────────┐                     │       │  └ Crtc   │
 │   machine.js   │  run_slice(n)       │       └ Memory     │
 │  フレームループ  │ ───────────────────▶└────────────────────┘
 └────────────────┘
```

## ビルド

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack

cd wasm && wasm-pack build --release --target web --out-dir ../web/pkg
python3 web/serve.py 8001
```

`--target web` を選んでいるのは、生成物をそのまま `<script type="module">` で
読めるからです。bundler 向けの出力にすると webpack などが要ります。

`file://` では開けません。ES モジュールも wasm も HTTP 越しでないと読めないためです。

そして**キャッシュを切るサーバーを自作しました**。

```python:web/serve.py
class NoCacheHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.send_header("Expires", "0")
        super().end_headers()
```

`python3 -m http.server` はキャッシュを効かせるので、**wasm を作り直しても
古いものが読まれます**。「直したのに変わらない」で何度も時間を溶かしました。
`?v=` をモジュールと `.wasm` の**両方**に付けるのも同じ理由です
(片方だけ新しいと「その関数は無い」と言われます)。

## 橋の部分

```rust:wasm/src/lib.rs
#[wasm_bindgen]
pub struct Emulator {
    m: Machine,
}

#[wasm_bindgen]
impl Emulator {
    /// ディスクイメージ (フロッピー) から起動する
    pub fn from_disk(image: &[u8]) -> Result<Emulator, JsError> {
        let mut m = Machine::new();
        m.boot_from_disk(image.to_vec())
            .map_err(|e| JsError::new(&e))?;
        Ok(Emulator { m })
    }

    /// 指定した命令数だけ進める。**1フレーム分の仕事**として呼ぶ。
    /// HLTで止まっていても抜けない — タイマ割り込みで起きるのを待つ必要があるため。
    /// アイドル中のOSは「HLTして割り込みを待つ」を繰り返している
    pub fn run_slice(&mut self, instructions: f64) {
        let n = instructions as u64;
        for _ in 0..n {
            self.m.step();
        }
    }

    /// テキストVRAM の先頭ポインタ。
    /// JS側はwasmのメモリを直接読む — コピーを作らないため
    pub fn text_vram_ptr(&self) -> *const u8 {
        self.m.text_vram().as_ptr()
    }

    /// キーの上げ下げを送る。`code` は `KeyboardEvent.code` (物理キーの識別子)。
    ///
    /// 文字ではなく**キーの位置**を渡すのが要点である。こうすると Ctrl も Esc も
    /// 矢印も特別扱いが要らず、修飾キーの組み立てはゲストのOSがやる
    pub fn key(&mut self, code: &str, down: bool) -> bool {
        self.m.devices.keyboard.key(code, down)
    }

    /// 画面が書き換わったか。**読むと下りる**ので描画の要否判定に使う
    pub fn take_vram_dirty(&mut self) -> bool {
        self.m.take_vram_dirty()
    }
}
```

**キーを文字ではなく位置で渡す**のがうまくいきました。`KeyboardEvent.code` を
そのままスキャンコードに変換すれば、Ctrl も Esc も矢印も特別扱いが要りません。
修飾キーの組み立ては**ゲストの OS がやる仕事**だからです。実機のキーボードが
送っているのも文字ではなく位置です。

## 落とし穴: wasmのメモリが伸びるとJSのビューが死ぬ

スナップショット機能を足したところ、**保存ボタンを押すと画面が真っ黒になる**
という症状が出ました。保存自体は成功しているのに、です。

犯人は `save_state()` が数 MB の `Vec<u8>` を確保することでした。
**WASM の線形メモリが伸びると、JS 側が持っていた `Uint8Array` のビューは
detach されます。**`text_vram_ptr()` で作ったビューが無効になり、
描画が空を読んでいました。

Rust 側は何も悪くありません。JS 側で写しを持つようにして直しました。

```js:web/terminal.js
/**
 * 今の画面の生バイト (文字+属性)。**自前の領域に写しを持つ。**
 *
 * wasmのメモリを直接見る参照を持ち続けてはいけない。wasm側で大きな確保が
 * あるとリニアメモリが伸び、**それまでの参照は無効になる**。
 * 実際、状態の保存 (数MBを確保する) をした瞬間に画面が真っ黒になった。
 * 写すのは4000バイトなので、抱えている危険に比べれば安い。
 */
```

これは自分のテストでは捕まえられませんでした。**「保存が成功したか」は見ていたが、
「保存した後も画面が生きているか」は見ていなかった**からです。
状態を変える操作のテストは、副作用の外側まで見ないといけません。

## パニックの中身をブラウザまで届ける

このエミュレータは「未実装は黙って 0 を返さず即 panic して正体を報告する」方針です。
ところがブラウザではこうとしか出ませんでした。

```
エラー: Uncaught RuntimeError: unreachable
```

**設計の核だった性質が、ブラウザではまるごと失われていました。**

```rust:wasm/src/lib.rs
/// パニックの内容をブラウザまで届ける。
///
/// **これが無いとJS側には `RuntimeError: unreachable` としか見えない。**
/// `INT 10h AH=0x13 未実装` や `unimplemented opcode 0x66 at 22c8:0759` という
/// **名前**こそが価値である。素のwasmはその文字列を捨ててしまうので、ここで拾い直す。
///
/// パニックの後のwasmインスタンスは触れないので、**フックの中で渡しきる**。
/// 「後から取りに行く」形にはできない。
#[wasm_bindgen]
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        console_error(&msg);
        report_panic(&msg);
    }));
}
```

JS 側では 2 つ気をつけました。

**後から飛んでくる `RuntimeError` で上書きさせない。** パニックは必ず中身の無い
包装として遅れて届くので、先に受け取った本当の理由を守ります。

**倒れた瞬間の画面を残す。** 描き直すと最後の絵が消えます。
「どこまで行けたか」が一番の情報なので、フレームループはパニックを捕まえたら
描かずに抜けます。

```js:web/machine.js
try {
  this.emu.run_slice(CHUNK);
} catch (e) {
  // wasmがパニックした。**ここで止めて、描き直さずに抜ける。**
  //
  // 描き直すと最後の絵が消えてしまう。「どこまで行けたか」が見えることが
  // このエミュレータの一番の情報なので、画面は倒れた瞬間のまま残す。
  this.running = false;
  this.crashed = true;
  this.onCrash?.(e);
  return;
}
```

結果、こう出るようになりました。

```
停止: 未実装の命令 0x66 で停止 (22c8:0759) — 画面は倒れた瞬間のまま
```

## 750倍速くなった話

ELKS の起動テストが **389 秒**かかっていました。原因は単純で、
「画面に `login:` が出るまで走らせる」テストが**1 命令ごとに 80×25 文字の
`String` を組み立てていた**のです。

```rust:core/tests/elks.rs
fn run_until(m: &mut Machine, needle: &str, budget: u64) -> bool {
    for _ in 0..budget {
        m.step();
        // 画面を毎命令組み立ててはいけない。1命令ごとに80x25文字のStringを
        // 作ることになり、起動が数百倍遅くなる (実際にやって390秒かかった)。
        // dirty フラグで、**書き換わったときだけ**見る
        if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
            return true;
        }
    }
    false
}
```

`take_vram_dirty()` を挟むだけで **0.52 秒**になりました。**750 倍**です。
ブラウザ側の描画も同じフラグで間引いています。

## 速度

静穏を確認した環境 (Apple Silicon M1 / release) での実測です。

| 測定 | MIPS | 1命令 |
|---|---|---|
| `Machine::step()` | **90.1** | 11.10 ns |
| `cpu::step()` のみ | **103.0** | 9.71 ns |
| WASM (Chrome) | **81.5** | 12.27 ns |

**WASM はネイティブの約 90%。** ブラウザで動かすこと自体は速度上の障害に
なりません。

もうひとつ、`bench` と `bench_raw` の差 12.4% が「**CPU が機械になった分の値段**」です。
割り込みの確認、装置のカウントダウン、HLT 判定、BIOS 入口の判定、
トラップフラグの確認 — OS の起動に必要だったものが、そのまま
1 命令あたり 1.39 ns として出ています。

なお**ばらつきは静かな環境では 0.3% しか出ません**。以前「1割ほどばらつく」と
書いていたのは、裏で動いていたものを測っていたからでした。
