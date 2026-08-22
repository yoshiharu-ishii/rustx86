# 0028: 最適化 Part 3 — 画面が出たら定規が変わる (X窓・画素の配管・wasm JIT の枠)

日付: 2026-08-22 / 状態: 提案 (調査完了、着手順はユーザー裁定待ち) / 関係: [gfx-roadmap](../reference/gfx-roadmap.md), [jit-roadmap3](../reference/jit-roadmap3.md), [0027](0027-core-round4.md), [0021](0021-broad-sweep-round.md)

## 背景

Part 1 (13→100 MIPS、ADR-0007/0014/0015) と Part 2 (C11 cold外し・F1d JIT・
C15 core第4ラウンド、ADR-0021〜0027) はブートと gcc窓を定規に磨いた。
Tier 6 で mode 13h・efifb・PS/2・X がブラウザまで通り (PR #204〜#211)、
**定規が変わった**: 第6ラウンド調査 ([gfx-roadmap](../reference/gfx-roadmap.md)) で
3つの事実が出た。

1. **X窓は gcc窓より遅く、JIT がほぼ効かない** (1284M命令・指紋ビット同一)。静かな機体での
   実測 (2026-08-22 夕、訂正): off **~69** / on **~69-71** MIPS — 同日同条件の gcc窓 off 73-75 に対し
   -5〜-8%、on 側は gcc窓で効く JIT の上積みが X では消える。(調査時の「off 56.5 / on 64、-30%」は
   ESET スキャン中の数字で過大だった — 定規は静かな機体で)。
   命令数は同じなので原因は命令ミックス — 従来経路落ちが 2.9% (gcc の3倍、
   時間の~10%) で、正体は movsx・SSE2 の移動系・16bit ALU・cmov/bt 等の
   **dcache 語彙の穴**。JIT カバレッジ 74.8%、焼き116k vs 据付86k の嵐も再演
2. **画素の配管は 3MB を1フレームに4回コピーし、うち1回は誰も使わない canvas への
   書き込み**。バイト単位の詰め替えがメインスレッドの 11% を食い、実効 fps を
   落としている。ゲスト側 (LFB は素のRAM・高速路・SMC 網の外) は設計どおりで穴なし
3. **wasm JIT は 64K 直接マップが X のコード量に負け**、据え付け31Kに対し焼き260万回
   (別セッションの soak 実測)。ネイティブで JSLOTS 256K→2M が効いた前例と同型

外部調査の裏づけ: Xorg fbdev は ShadowFB 既定ONで、LFB に来るのは damage 矩形の
memcpy だけ (pixman の SSE2 合成はシャドウ側)。つまり**画素ストアの経路は穴ではなく、
SSE2 が落ちているのはシャドウ合成と libc の memcpy** — 的は「語彙」であって
「画素経路」ではない。

## 決定 (提案)

### 1. 定規を1本足す — X窓 (第3の定規)

jboot (ブート) / jcmd (gcc窓) に **X窓** (Xorg + xeyes + xclock + xterm スクロール、
1284M命令・FNV 296fabe8e823bd6c) を足す。ハーネスは jcmd と同じ作り
(`DISK`/`INITRD`/`LFB`/`RAM_MB`/`CMDLINE`) で `jit-a64/src/bin/gfxcmd.rs` に置く
(調査時の写しは scratchpad、PR で本体へ)。**disk-x.img は焼き直すとコースが変わる**ので、
指紋は「同一イメージ内の on/off 相対」を門番とし、イメージ更新時は基線を張り替える
(system-roadmap の「帳簿の分離」と同じ扱い)。

### 2. CPU 側は「語彙の穴」を X窓で裁く (指紋は値まで不変が契約)

順序は期待値×安さ。全部 exec が原本 (twobyte.rs / sse.rs / string.rs) へ委譲する
型なので、意味論の原本は1つのまま:

| 順 | 玉 | 的 |
|---|---|---|
| C16 | 0F 語彙 (movsx/cmov/bt/shrd/bsf/cmpxchg) + A8/A9・69/6B・99 | 落ちの~55% |
| C17 | 16bit ALU の o16 通し (66 付き cmp/test/C7) | 落ちの~10% |
| C18 | SSE2 移動系 (movd/movq/movaps/movdqa/pshufd) — 66/F2/F3 の門を 0F に開ける | 落ちの~8-10%、1本が高い |
| J1 | 焼き直しの嵐 (2-way / Assembler 再利用 / PROFIT スイープ) | jit-roadmap3 §3、X でも同型 |
| J2 | 常駐化① ストアTLBインライン | 画素書きが主役の X で効く見込み |

C16〜C18 は C15-PR3 (o16 を通すだけで -2〜4%) の延長で、**構造を足さず腕を足す**だけ。
判定は X窓 off/on + gcc窓 + ブートの交互A/B、cosim/lockstep/3OS 門番。

### 3. 画素の配管はホスト側だけ触る — 「フック無し」の約束は守る

G1〜G3 (未使用 putImageData の削除・Uint32 詰め替え・Worker 側で変換) を1本の PR に。
秒/フレームで裁く (ゲストの遷移に触れないので A/B は不要、指紋の門番は通す)。
その後 G4/G5 (Worker 側の行比較で送信スキップ・変化行帯だけ送る) — **読むだけ**
なので約束の内側。v86 式の書き込みフック (G6) は💤: 画素ストアが JIT の高速路から
落ちる税を払ってまで取る段ではない。復帰条件 = G4/G5 で届かない絵 (全画面動画級) が
要件になったとき。OffscreenCanvas (G7) / SAB (G8) はその次。

### 4. wasm JIT の枠 (W1/W2)

スロット表 64K→256K (または 2-way) を soak の帳簿 (`jit_baked/recycled`) で裁く。
wasm の A/B は配置ノイズで嘘をつく (perf.md 測定の規律) ので、判定は**焼き回数と
同一命令数での秒数**。引退 (1/8 則) のしきい値も同じ帳簿で決め直す。

## 見送り・寝かせ (理由つき、[keep-options-open] の流儀で削除はしない)

- **ホストMMU (mprotect+SIGSEGV) でのダーティ検出 (G12)**: macOS arm64 の fault 原価の
  公開実測が無い。触るなら1行のマイクロベンチが先。ネイティブ限定で wasm に載らない
- **DOSBox 式の行 memcmp を main で**: 3MB では比較自体が税 (静止画で 6-8%)。
  比較は Worker 側・Uint32 で (G4)、それでも足りなければ G6 へ
- **ゲスト PTE の D ビット走査**: 前例なし、D ビットを触るとゲスト可視 — 決定性と衝突
- **SSE2 を JIT 語彙に**: まず dcache (C18)。pixman の合成は JIT カバー率より
  「従来経路落ち」の単価の問題
- **WebGL/WebGPU (G11)**: putImageData との同条件比較が無い。G1-G3 で足りる見込み

## 検証

- CPU: 指紋 3コース (jboot / jcmd / X窓) の on/off ビット同一、cosim・lockstep・
  3OS・regress・vga-check。交互A/B はネイティブのみ ([native-ruler-only])
- 画素: headless.mjs の LFB ケース (regress 4本目) + ブラウザ実走、フレーム時間を
  `performance.now()` で Worker/main 両方に計器化して前後比較
- wasm JIT: soak (X→dillo→マウス 30G命令) の帳簿と秒

## 教訓

- **定規はワークロードが作る** — gcc窓で「語彙フェーズは終わり」と書いた
  (jit-roadmap3) が、X では語彙の穴が時間の10%に戻ってきた。落ち率は
  ワークロードごとにセンサスを取り直す
- **コピーの回数は設計図に描いて数える** — 3MB×4 回のうち1回は、誰も参照しない
  canvas への書き込みだった。「動いた」の後に配管図を1度は描く
- **他所の定石 (書き込みフック) は自分の約束と照らす** — v86 の方式は正しいが、
  本機は画素ストアを JIT の高速路に残す約束を先にしている
