# ビルドの最小構成 — RustとWASMで何が起きているか

このリポジトリを動かすのに必要なのは Rust だけで、ブラウザ版を作るときに
道具が2つ増える。ここではその**最小の手順**と、**なぜその選択なのか**を書く。

「動かし方」だけなら [README の実行](../README.md#実行) で足りる。
こちらは**何が起きているか**を知りたいとき用である。

## 1. Rust を入れる

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

`rustup` は Rust のツールチェーン管理で、`rustc` (コンパイラ) と `cargo`
(ビルドとパッケージ管理) が一緒に入る。安定版でよい。

このリポジトリは **edition 2021** で書いている。

```toml:core/Cargo.toml
[package]
name = "rustx86-core"
version = "0.1.0"
edition = "2021"

[dependencies]
```

`[dependencies]` が空なのは意図的である。`core` は**外部クレートにまったく
依存していない**。教材なので、読んだときに「この行が何をしているか」が
クレートの向こう側へ消えないようにしている。

つまり `cargo test` は Rust を入れただけで通る。

重い依存は**別のクレートに隔離する**。co-sim は Unicorn Engine を、
`disasm` は逆アセンブラ (iced-x86) を抱えるが、どちらも `core` の外に居る。
デバッガの逆アセンブル表示は「画面に出すための都合」= 表示層の関心なので、
エミュレータ本体を汚さない。core とは独立した第二の実装になるので、
両者が食い違えば検知すべき信号になる (co-sim と同じ構図)。

## 2. ワークスペースの形

```toml:Cargo.toml
[workspace]
members = ["core", "cosim", "wasm"]
default-members = ["core"]
resolver = "2"

[profile.release]
opt-level = 3
lto = true
```

**`default-members = ["core"]` が効いている。** `cargo test` と打つと `core` だけが
対象になり、`cosim` (Unicorn Engine のビルドに数分かかる) は走らない。
co-sim を回したいときだけ明示する。

```bash
cargo test                    # core だけ (速い)
cargo test -p rustx86-cosim   # Unicorn との突き合わせ (初回は数分)
```

`lto = true` は**リンク時最適化**で、クレートをまたいだインライン展開が効く。
エミュレータは `Machine::step()` から `cpu::step()` へ、さらに `alu8()` へと
細かい関数呼び出しが積み重なるので、ここが素直に効く。

`profile.release` を設定しているので、速度を測るときは必ず `--release` を付ける。
デバッグビルドでは1桁遅くなり、比較の意味がなくなる。

## 3. ブラウザ版に要る2つ

```bash
rustup target add wasm32-unknown-unknown   # wasm を吐けるようにする
cargo install wasm-pack                    # JSとの繋ぎを自動生成する道具
```

### wasm32-unknown-unknown とは

ターゲット名は `<アーキテクチャ>-<ベンダ>-<OS>` の形で、
`wasm32-unknown-unknown` は「**32bit wasm・ベンダ不明・OS無し**」を意味する。

OS が無いというのが大事で、**OS が提供するものは使えない**。
このリポジトリで実際にぶつかったのがこれである。

```rust:core/src/dev/cmos.rs
//! - **決定的でなければスナップショットが再現しない。** 同じ状態から再開したら
//!   同じ時刻でなければ困る。ホストの時計を読むと再開のたびに違う値になる
//! - `core` は時計を持てない。`std::time::Instant` は wasm32 では動かない
```

`std::time::Instant` はプラットフォームの時計に触るので、wasm32 では動かない。
ファイルもスレッドも同様である。だから**時刻はゲストの命令数から導いている**。

制約が設計を決めた例で、結果的にスナップショットの再現性という利点になった。

### wasm-pack が何をしているか

`cargo build --target wasm32-unknown-unknown` だけでも `.wasm` は作れる。
だがそれは**関数と数値しか受け渡せない**。文字列も構造体も配列も渡せない。

wasm-bindgen (と、それを呼ぶ wasm-pack) は、その間を埋める**JSの糊コード**を
自動生成する。実際に出てくるものはこうなる。

```bash
cd wasm && wasm-pack build --release --target web --out-dir ../web/pkg
```

```
web/pkg/
├── rustx86_wasm_bg.wasm     105 KB  ← 本体 (機械語)
├── rustx86_wasm.js           16 KB  ← 糊。文字列や構造体の受け渡しを担う
├── rustx86_wasm.d.ts        7.5 KB  ← 型定義 (エディタ補完用)
└── package.json                     ← npm形式のメタ情報
```

Rust 側で `#[wasm_bindgen]` を付けたものが、そのまま JS の名前として出る。

```rust:wasm/src/lib.rs
#[wasm_bindgen]
pub fn cp437_table() -> String {
    rustx86_core::cp437::table_string()
}
```

```ts
// 生成された rustx86_wasm.d.ts より
export function cp437_table(): string;
```

`String` を返す関数がそのまま使えているのは、糊が
「wasm のメモリからバイト列を読んで JS の文字列に組み立てる」処理を
書いてくれているからである。**wasm 単体ではこれができない。**

### なぜ `--target web` なのか

`wasm-pack` の `--target` は出力の形を決める。主なものは3つ。

| 値 | 出てくるもの | 使いどころ |
|---|---|---|
| `web` | ESモジュール。`<script type="module">` でそのまま読める | **これを使っている** |
| `bundler` | webpack などのバンドラ前提 | ビルド工程を持つアプリ |
| `nodejs` | `require()` で読む形 | Node.js |

このリポジトリのブラウザ側はビルド工程を持たない。`web/*.js` を素の
ESモジュールとして書き、そのままブラウザに読ませている。
**バンドラを1つ増やさずに済むので `web` を選んだ。**

```js:web/machine.js
import init, { Emulator, cp437_table, install_panic_hook } from './pkg/rustx86_wasm.js?v=7';
```

## 4. HTTPで配る必要がある

```bash
python3 web/serve.py 8001
# http://localhost:8001/ を開く
```

**`file://` では開けない。** ESモジュールも wasm も、`file://` では
セキュリティ上の理由で読み込めない。ローカルでもHTTPサーバーが要る。

そして `python3 -m http.server` ではなく `web/serve.py` を使っている。

```python:web/serve.py
class NoCacheHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.send_header("Expires", "0")
        super().end_headers()
```

理由は単純で、**標準のサーバーはキャッシュを効かせるから**である。
wasm を作り直しても古いものが読まれる。

これで何度も時間を溶かした。症状が厄介で、

- HTMLが古いままで、**3種類の実装を試したのに結果が1バイトも変わらなかった**
- 糊 (`rustx86_wasm.js`) だけ新しくて `.wasm` が古く、
  「`emu.key` は関数ではありません」と言われた
- 逆に `.wasm` だけ新しくて、「`wasm.emulator_cursor_row` は無い」と言われた

当時は `?v=番号` を糊と `.wasm` の両方に付けて手で上げることで破っていたが、
これは**後に廃止した**。番号を上げ忘れる・片方だけ上げるという新しい事故を
生むだけで、キャッシュ問題そのものは `serve.py` の `no-store` が既に
解決していたからである。同じ問題を2回別の方法で直して、手作業の方だけが
残っていた。**検査で見張るより、事故れない構造にする。**

**出力が変わらないときは、まず「そのコードが本当に走っているか」を疑う。**
このリポジトリで得た教訓の中で、たぶん一番よく再利用できるものである。

## 5. wasm 特有の落とし穴

### メモリが伸びると JS のビューが死ぬ

wasm の線形メモリは伸びる。伸びると、**JS 側が持っていた `Uint8Array` の
ビューは detach され、中身が読めなくなる**。

このリポジトリでは、画面 (テキストVRAM) を毎フレームコピーせずに
wasm のメモリを直接見ていたので、状態のスナップショット (数MBを確保する) を
取った瞬間に**画面が真っ黒になった**。

```js:web/terminal.js
     * wasmのメモリを直接見る参照を持ち続けてはいけない。wasm側で大きな確保が
     * あるとリニアメモリが伸び、**それまでの参照は無効になる**。
     * 実際、状態の保存 (数MBを確保する) をした瞬間に画面が真っ黒になった。
     * 写すのは4000バイトなので、抱えている危険に比べれば安い。
```

**ポインタを渡してゼロコピーで読むのは速いが、参照を持ち続けてはいけない。**
毎回取り直すか、写しを持つ。

### panic の中身が消える

Rust の `panic!` は wasm では `unreachable` 命令になる。JS から見ると
こうとしか分からない。

```
エラー: Uncaught RuntimeError: unreachable
```

**メッセージは捨てられる。** このリポジトリは「未実装は黙って0を返さず
即panicして正体を報告する」方針なので、これでは設計の核が死ぬ。

フックを入れて拾い直している。

```rust:wasm/src/lib.rs
#[wasm_bindgen]
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        console_error(&msg);
        report_panic(&msg);
    }));
}
```

**パニックの後の wasm インスタンスには触れない**ので、フックの中で渡しきる。
「後から取りに行く」形にはできない。

### メモリの上限は4GB

wasm32 の線形メモリは 4GB が上限で、エミュレートする RAM もディスクイメージも
**同じ4GBを分け合う**。しかもリロードで消える。

大きなディスクを扱いたくなったら、**Web Worker + OPFS** へ移す必要がある。
OPFS の `createSyncAccessHandle()` は `read()`/`write()` が同期APIで、
**Web Worker の中でしか使えない** — エミュレータ向けの設計になっている。
このリポジトリでは [箱 B5](../README.md#b5-web-worker--opfs-への移行) として置いてある。

## 6. 最小の Hello

このリポジトリから離れて、**最小構成**を作るとこうなる。

```bash
cargo new --lib hello-wasm && cd hello-wasm
```

```toml
# hello-wasm/Cargo.toml (このリポジトリの外に作る最小例)
[package]
name = "hello-wasm"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
wasm-bindgen = "0.2"
```

**`crate-type` が要点。** `cdylib` はC互換の動的ライブラリという意味で、
wasm を吐くにはこれが要る。`rlib` を併記しているのは、他の Rust クレートから
普通に使えるようにするためで、このリポジトリの `wasm` クレートも同じ形にしている。

```toml:wasm/Cargo.toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
wasm-bindgen = "0.2"
rustx86-core = { path = "../core" }
```

```rust
// hello-wasm/src/lib.rs
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}
```

```bash
wasm-pack build --release --target web
```

```html
<script type="module">
  import init, { greet } from './pkg/hello_wasm.js';
  await init();
  console.log(greet('x86'));
</script>
```

`&str` を渡して `String` を受け取れているのは、全部 wasm-bindgen が
糊を書いてくれているからである。生成される型定義にもそう出る。

```ts
// pkg/hello_wasm.d.ts
export function greet(name: string): string;
```

**この最小例は実際に作って動かして確かめた。** 出来上がりは `.wasm` が 15 KB、
糊が 6.5 KB。ブラウザで開くと `Hello, x86!` と表示される。

## 参考: 実測

このリポジトリでの実測値 (Apple Silicon M1 / release / 静穏を確認した環境)。

| | |
|---|---|
| `.wasm` の大きさ | 105 KB |
| 糊 (`.js`) の大きさ | 16 KB |
| ネイティブ | 90.1 MIPS |
| WASM (Chrome) | 81.5 MIPS (**ネイティブの約90%**) |

**ブラウザで動かすこと自体は速度上の障害にならない。**
詳しくは [README のベンチマーク](../README.md#ベンチマーク) を参照。
