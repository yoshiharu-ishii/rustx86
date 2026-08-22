# グラフィックス時代の鉱脈 (第6ラウンド調査、2026-08-22)

Tier 6 で mode 13h・efifb・PS/2・X (fbdev 1024×768×32bpp) がブラウザまで
通った。**画面が出た瞬間から速度の定規が変わる** — gcc窓で磨いたエンジンが
X では遅く JIT の上積みも消え、画素の配管は台帳に1行も無い。ここはその両方の台帳。
入力は3本: (1) グラフィックス配管のコード分析と node 実測、(2) X を
ネイティブで回した CPU 側センサス (新定規「X窓」)、(3) 外部技法の文献調査。
裁定は [ADR-0028](../adr/0028-part3-cpu-and-gfx.md)。

## 1. 新定規「X窓」 — gcc窓より遅く JIT が効かない、原因は命令ミックス

ハーネス: jcmd と同じ作り (シリアルから命令を流し DONEMARK まで)、
`DISK=images/disk-x.img INITRD=initramfs-mini LFB=1024x768 RAM_MB=256`、
`xinit` で Xorg + xsetroot + xeyes + xclock + xterm (`seq 1 15000` の
スクロール描画) → X終了。ヘッドレスで X が実際に描いている (LFB dump に
xeyes・カーソルが写る)。

| | 窓命令数 | 壁時計 | MIPS | 指紋 |
|---|---|---|---|---|
| X窓 JIT off (静かな機体、交互3周) | 1284M | 18.5-18.6s | **68.9-69.6** | 296fabe8e823bd6c |
| X窓 JIT on | 1284M | 18.6-18.7s | **68.7-69.1** (on≈off) | 同一 (ビット同一) |
| 参考 gcc窓 (同日同条件) | 520M | — | off 73-75 / on 69-74 | 冷間の履歴値は off 80-83 / on 92-96 |

(訂正 2026-08-22 夕: 調査時の「off 56.5 / on 64 = gcc窓より-30%」は ESET のフルスキャン中の
数字で過大だった。静かな機体では -5〜-8%。**定規は静かな機体で** — pitfalls 6 の再演)

命令数は決定的に同一なので、差は**命令ミックスそのもの**。センサス (opstats):

- dcache ヒット97.1%、**従来経路落ち 37M = 2.9%** (gcc/ブートは1%未満)。
  落ち1本 ~200 ホスト命令として**窓の時間の ~10%**
- JIT on: **カバレッジ 74.8%** (gcc窓72%と同程度)、平均 8.2 命令/入場、
  **焼き 116,565 vs 据付 85,579** — 焼き直しの嵐が X でも再演
- collect を止めた uop: grp3 36% / imul 12% / string REP 17%

従来経路落ちの正体 (37M の内訳):

| オペコード | 回数 | 割合 | 正体 |
|---|---|---|---|
| 0F 系合計 | 15M | 29.5% | 2バイト命令 — dcache は jcc/setcc/imul/movzx しか持たない |
| 0FBE / 0FBF | 4M | 8.2% | **movsx** (pixman・xterm の文字/画素処理) |
| 0F7E / 0FD6 / 0F29 / 0F70 | 3M | 8.2% | **SSE2 movd / movq / movaps / pshufd** (pixman・memcpy) |
| 0FB1 / 0FA3 / 0FAC / 0F45 / 0FBC | 2M | 5.3% | cmpxchg / bt / shrd / cmovne / bsf |
| 3B / 39 / C7 / F7 (一部) | 4M | ~10% | **0x66 付き** cmp / mov [m],imm16 / test (16bit ALU) |
| A8 / A9 | 3M | 6.5% | test al,imm8 / test eax,imm32 |
| 69 / 6B | 1.5M | 4.9% | imul r,rm,imm |
| FA/FB/FC/9C/99 | 2M | 5% | cli/sti/cld/pushf/cdq (カーネル) |
| D9/DB | 0.5M | 1% | x87 |

`bounce` (fbdev デモ) は nanosleep 主体で窓388Mのうち実命令64M — CPU の
定規には軽すぎる。**X窓が正しい定規**。

### 外部調査が裏づけた事実 (CPU 側の的を変える)

- **Xorg fbdev は ShadowFB 既定ON** — pixman の SSE2 合成はシステムメモリの
  シャドウに対して走り、LFB には damage 矩形の memcpy だけが来る
  ([fbdev(4)](https://manpages.debian.org/testing/xserver-xorg-video-fbdev/fbdev.4.en.html))。
  つまり LFB 宛ストアは `rep movs` = バルク路に既に乗っている。
  **SSE2 が落ちているのはシャドウ側の合成とlibcのmemcpy**であり、画素ストアの
  経路そのものは穴ではない
- LFB は `self.mem` の素のRAM (`core/src/boot/bzimage.rs:163`、`mem/mod.rs:45`)。
  `fast_write*` と `jit_try_write32` の高速路に乗り、`page_has_code` が立たないので
  SMC 検出は分岐1つ。**「フック無し」の約束は守れている**
- 穴は1つ: `write_m128` (`core/src/cpu/sse.rs:70`) が `write32`×4 で変換を4回踏む。
  ページ内16Bなら変換1回にできる (命令数・フォールト挙動不変)

## 2. 画素の配管 — 3MB を1フレームに4回コピーし、1回は無駄

両 Linux 機とも **32bpp [pad,R,G,B]** (`bzimage.rs:192` で bpp=32 固定。
24bpp 分岐 `web/ansi.js:649` と「24bpp・赤先頭」のコメントは死んだ記述)。

```
ゲストCPU ──store──▶ self.mem (素のRAM、通知なし)
  ▼ Worker (linux-worker.js:297)  33ms毎 + 'lfb-ack' 背圧
  ① lfbBuf.set(wasmビュー)                 3,145,728 B コピー
  ② postMessage(transferable)              ゼロコピー (ackで往復・再利用)
  ▼ main (linux-machine.js:373 → ansi.js:616)
  ③ バイト単位ループで [pad,R,G,B]→[R,G,B,255]   読3MB+書3MB、786,432画素×4代入
  ④ gfx.ctx.putImageData(img)              3MB — オフスクリーン canvas、誰も使わない
  ⑤ this.ctx.putImageData(img)             3MB — 表示 canvas
  ⑥ 'lfb-ack' で ArrayBuffer を Worker へ返す
```

node 実測 (1024×768、web/ と同じループ):

| 工程 | ms/frame |
|---|---|
| ③ バイト単位の詰め替え (現状) | **3.73** |
| 同じ処理を Uint32 で `(s>>>8)\|0xff000000` | 1.09 |
| ① `set()` 丸コピー | 0.08 |
| 行単位の変化検出 (最悪=全行相違) | 0.32 |

メインスレッドは 30fps で **~110ms/s (11%)** を詰め替えに使い、再送判定が
`now - lastLfbAt >= 33` なので描画が遅いぶん実効 fps が落ちる (≈25fps)。
割り当ては初回のみ (毎フレームの new は無い)。mode 13h は 64KB+パレット768B
の Vec コピーで 0.3〜0.5ms 級 — 優先度低。

### 外部の相場

- postMessage の構造化複製は **~80kB/ms** (3MB = 37ms)。transferable か
  SharedArrayBuffer 以外に道は無い ([Mozilla 2015](https://hacks.mozilla.org/2015/07/how-fast-are-web-workers/)) —
  本機は transferable 済み
- v86 は LFB ダーティをホストMMUでなく**書き込み経路のアドレス判定+ページ
  ビットマップ**で取り、`putImageData` の dirty rect で行範囲だけ描く
  ([memory.rs](https://github.com/copy/v86/blob/master/src/rust/cpu/memory.rs) /
  [screen.js](https://github.com/copy/v86/blob/master/src/browser/screen.js))。
  本機は「画素ストアにフックを付けない」約束なので採らない (💤、下表 G6)
- DOSBox の行キャッシュ memcmp は**静止画でも全体の 8.3% (AVX2で6.2%)**
  ([dosbox-x #1880](https://github.com/joncampbell123/dosbox-x/pull/1880)) —
  「比較は安い」は 64KB の VGA でしか成り立たない。3MB では比較自体が税
- mprotect+SIGSEGV は Linux x86 で **1.92µs/fault**
  ([xzpeter](https://xzpeter.org/userfaultfd-wp-latency-measurements/))、
  **macOS arm64 の公開実測は無い** — 触るなら1行のマイクロベンチが先
- WebGL テクスチャ転送は 64MB で 6-15ms (M4 Max) → 3MB なら ~0.5ms 級だが
  putImageData との同条件比較は無い。自前計測が要る

## 3. wasm JIT — 64K 直接マップが X のコード量に負けている

別セッションの soak 実測 (X + dillo + マウスを 30G 命令): `baked=2,679,007`
`recycled=2,646,138` に対し据え付け ≈31K、`retired=7,193`。
**据え付け1に対して焼き86回**。`wasm/src/jit.rs:1195` の
`si = (pa ^ pa>>12) & (JSLOTS-1)` の衝突が主因で、Xorg+icewm+dillo+libc の
コード量に 64K 枠が足りない。ネイティブで JSLOTS 256K→2M にして衝突再焼き
359k→129k・-13 MIPS を取り返した前例と同型。帳簿は `wasm/src/lib.rs` の
export (`jit_installed/baked/recycled/retired`) で読める。

定規: wasm の A/B は配置ノイズで嘘をつく ([perf.md 測定の規律](perf.md)) ので、
「同じ命令数での秒数」(soak) か帳簿の焼き回数そのもので裁く。

## 候補台帳 (perf.md のカタログに採番、ここは根拠つきの全量)

### CPU (X窓で裁く。指紋は値まで不変が契約)

| # | 案 | 期待 (X窓) | 決定性 | 根拠 |
|---|---|---|---|---|
| C16 | **0F 語彙の拡張**: movsx (0FBE/BF)・cmov・bt/shrd/bsf・cmpxchg + 1バイトの A8/A9・69/6B・99 | 落ち37Mの~55% → 窓 ~5% | 不変 (exec は twobyte.rs の原本へ委譲、StrOne と同じ型) | 上表 |
| C17 | **16bit ALU の o16 通し** (66 付き cmp/test/C7) | 落ちの ~10% | 不変 (C15-PR3 と同じ処方) | 上表 |
| C18 | **SSE2 移動系を dcache へ** (movd/movq/movaps/movdqa/pshufd) — `decode.rs:130,140` の 66/F2/F3 門を「0F なら通す」に | 落ちの ~8-10%、1本が特に高い | 不変 | pixman/memcpy |
| C19 | `write_m128` の変換1回化 (ページ内16B) | 小 | 不変 (フォールト順序も同じ) | sse.rs:70 |
| J1 | **焼き直しの嵐の鎮火** (2-way スロット / Assembler 再利用 / PROFIT スイープ) | 窓 ~9% (jit-roadmap3 §3、X でも同型) | 不変 | 焼き116k/据付86k |
| J2 | 常駐化① ストアTLBインライン | 画素書きが主役の X で gcc より効く見込み | 不変 | jit-roadmap3 §2 |
| J3 | JIT collect 語彙 (grp3/imul/REP) | カバレッジ 74.8% → 上 | 不変 | collect 停止理由 |

### 画素の配管 (ホスト側のみ。ゲストの遷移に触れない = A/B 不要、秒/フレームで裁く)

| # | 案 | 期待 | 根拠 |
|---|---|---|---|
| G1 | **④ 未使用 canvas への putImageData を削る** | 3MB/frame、1行 | ansi.js:657 |
| G2 | **詰め替えを Uint32 で** | 3.7→1.1ms/frame (-70%) | 上表 |
| G3 | **詰め替えを Worker 側で** (①と同時に変換、main は putImageData だけ) | main の描画負荷ほぼ0 | linux-worker.js:303 |
| G4 | **行単位の変化検出で送信スキップ** (Worker が前フレームと Uint32 比較、最悪0.32ms) | アイドル時の 3MB 転送+描画が消える | 読むだけ=フック無し |
| G5 | 変化行帯だけ送り `putImageData` の dirty rect で描く | 文字入力・小窓で 3MB→数十KB | v86 の消費側と同形 |
| G6 | LFB 書き込みフック+ページビットマップ (v86 式) | — | 💤 「画素ストアにフック無し」の約束に反する。G4/G5 で届かなくなったら再訪 |
| G7 | OffscreenCanvas を Worker へ (`transferControlToOffscreen`) | postMessage/ack 往復と main の putImageData が消える | linux-worker.js:291 の「足りなくなってから」メモ |
| G8 | SharedArrayBuffer でゼロコピー (COOP/COEP + `--shared-memory` ビルド) | ①消滅 | 配布環境の制約が大。G7 の後 |
| G9 | `desynchronized:true` / rAF 駆動の pull (裏タブで送らない) | 合成レイテンシ・裏タブの無駄 | 1行級 |
| G10 | mode 13h: `palette()` の Vec コピーをゼロコピー窓+Uint32 LUT に | 0.1-0.2ms/frame | wasm/src/lib.rs:457 |
| G11 | WebGL/WebGPU テクスチャ | G1-G3 で足りる見込み。putImageData との同条件比較が先 | 外部 A6 |
| G12 | ホストMMU (mprotect) によるダーティ検出 | ネイティブ限定・macOS の fault 原価が未計測 | 外部 B1 |

### wasm/ブラウザ

| # | 案 | 期待 | 根拠 |
|---|---|---|---|
| W1 | **wasm JIT スロット表の拡張 (64K→256K) or 2-way** | 焼き260万→桁減 | 上 §3 |
| W2 | モジュール引退 (1/8 則) のしきい値を帳簿で決め直す | retired 7,193/30G の再焼き減 | 同 |
| W3 | 動的生成モジュールが Liftoff 止まりか計測 (`new WebAssembly.Module` の所要と tier) | ブラウザ JIT が効かない構造の説明候補 | [V8 Liftoff](https://v8.dev/blog/liftoff): TurboFan 比 50-70% 遅い・OSR 無し |

## 出典 (第6ラウンド)

- [fbdev(4) ShadowFB](https://manpages.debian.org/testing/xserver-xorg-video-fbdev/fbdev.4.en.html) /
  [fbturbo README](https://github.com/ssvb/xf86-video-fbturbo/blob/master/README)
- [v86 memory.rs (LFB dirty)](https://github.com/copy/v86/blob/master/src/rust/cpu/memory.rs) /
  [v86 screen.js](https://github.com/copy/v86/blob/master/src/browser/screen.js)
- [dosbox-x #1880 (行キャッシュ AVX2)](https://github.com/joncampbell123/dosbox-x/pull/1880) /
  [QEMU memory_region_snapshot_and_clear_dirty](https://www.qemu.org/docs/master/devel/memory.html) /
  [Basilisk II VOSF](https://basilisk.cebix.net/TECH)
- [How fast are web workers (postMessage 80kB/ms)](https://hacks.mozilla.org/2015/07/how-fast-are-web-workers/) /
  [OffscreenCanvas](https://web.dev/articles/offscreen-canvas) /
  [WebGL/WebGPU upload bench](https://github.com/mvaligursky/webgl-webgpu-texture-upload)
- [mprotect/SIGSEGV 1.92µs (Linux x86)](https://xzpeter.org/userfaultfd-wp-latency-measurements/) /
  [blink README (RO+SIGSEGV の SMC)](https://github.com/jart/blink/blob/master/README.md)
- [rv8 (直接マップ翻訳キャッシュ+ret比較)](https://carrv.github.io/2017/papers/clark-rv8-carrv2017.pdf) /
  [Dynarmic RSB](https://github.com/PabloMK7/dynarmic/blob/master/docs/Design.md) /
  [QEMU TLB 動的リサイズ](https://arxiv.org/abs/1905.06825)
- [V8 Liftoff](https://v8.dev/blog/liftoff) / [dynamic tiering](https://v8.dev/blog/wasm-dynamic-tiering) /
  [memory64 は 10-100% 遅い](https://spidermonkey.dev/blog/2025/01/15/is-memory64-actually-worth-using.html)
- 警告: [arXiv 2501.03427「QEMU の 35×」](https://arxiv.org/abs/2501.03427) は無分岐の合成ベンチ —
  N-Queens では QEMU の方が 9 倍速い。台帳に入れない

関連: [jit-roadmap3.md](jit-roadmap3.md) / [core-roadmap.md](core-roadmap.md) /
[system-roadmap.md](system-roadmap.md) / [perf.md](perf.md)
