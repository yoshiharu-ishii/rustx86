# ADR-0021: 掃除残しと新家系 — 広範囲調査を4バッチで裁く

- 状態: **決定** (2026-08-16。裁定済み、実装はバッチごとのPRで)
- 日付: 2026-08-16
- 関連: [ADR-0020](0020-external-review-round3.md) (判別則(4)の精密化 — 本件の判定基準)、
  [ADR-0014](0014-external-review-hotpath.md) (バッチ実装→合算判定の方法)、
  [perf.md](../reference/perf.md) (採番 B7 / C11〜C14 / D4〜D5 / E7)

## 背景

note_write返済 (PR #168) の後、「他にできる最適化はあるか」を広範囲に調査した。
方法は3並行: (1) core非ホットループの掃除残し探索、(2) dcache実行経路の
ミクロ構造とレイアウト精査、(3) 他エミュレータ (QEMU/v86/blink/Box64/
DOSBox/Bochs/wasm3) の技法カタログの外部調査。結果を判別則(4)
(**制御依存はデータ依存ではない。効くのはループ搬送依存の短縮か
キャッシュフットプリント削減**) で裁いた。

## 事実の発見 (裁定の前提)

1. **Entry表は4MiB** — Entryはレイアウト計算で32B (Rmの判別子パディング4B+
   末尾3Bが捨て札)、128Kスロットで4MiB。dcache/mod.rsの「768KB」コメントは
   5.3倍ずれた死んだ記述 (バッチCで修正)。搬送鎖の先頭ロードが引く表なので、
   ビットセット化 (+2%→-1.4%、PR #168) と同じ鉱脈のより大きい塊
2. **batch1の刈り残し** — `advance_ip32` は ip_mask() を剥がして-8%に寄与したが、
   同じ論拠が使える `set_ip` (制御uop = 全命令15〜20%が通る) は剥がされていない
3. **M1設計定数** (実測ソース: dougallj/applecpu、7-cpu.com):
   キャッシュライン**128B**・L1d 128KB/load-to-use 3cy・L2 ~18cy・
   分岐ミス~13cy・taken branch 1本/cy。以後の設計判断の定数とする

## 裁定 — 4バッチ (実装順: C → A → B → D1)

### バッチC: cold外しの総ざらい (perf.md **C11**) — 先頭

#[cold]化は5勝2敗1分の勝ち越し実績があり、リスク最小でA/Bの見通しも良くなる:
- 最頻arm (mov 24%/alu 16%/test/movzx/push/pop/call/ret) に slow_read/write と
  逐語同一のインライン低速路が12箇所残存 → 既存#[cold]関数呼びに
- fill/fallback経路 (**4MiBのvec!確保コード含む**) がホットループ本体に同居 →
  `#[cold] #[inline(never)]` へ外出し。ヒット時の `Option<(u8,Uop)>` 中間器も直結に
- 稀uop arm (Grp3/Grp5/SetCC/Imul/moffs/StrOne、各~1%以下) の#[cold]移送
- `translate_for` をTLBヒット路 (inline) と walk/queue_ad (#[cold]) に分離
  — S2 (flush_ad分離 -3〜4%) と同じ処方の未適用箇所

### バッチA: ループ搬送鎖の直短縮 (perf.md **C12/C13**) — batch1 (-8%) の続編

鎖 `ip → lin → pa → slot → Entryロード → len → 次のip` の別々の節を
バッチ実装→合算判定→勝てば二分 (ADR-0014の方法):
- `set_ip32` 新設 — ip_mask() (cr0ロード+CSロード+分岐) を制御uopから剥がす
- exec が書いた ip を外側ループがメモリから読み直している store→load 往復を
  レジスタ返しに
- 次スロット計算 (new_lin→pa→slot の5演算) をページ内オフセットcarryの2演算に
- jcc の condition() が flag() を2〜3回叩き cc_op を都度再ロード → cc_op で
  1回だけ分岐する形に。cc_sign() の可変シフトは set_cc 時に u32 で実体化
- 同乗の小玉: Grp3の実効アドレス二度作り、Grp5{rm:Reg,kind:0/1}のF_MEM過剰
  (60B控えの無駄払い)、moffs系のtranslate-first未配線

### バッチB: Entry痩身 32→24B (perf.md **D4**)

`Rm::Reg` を MemRef の番兵 (例: seg=0xFF) に畳むと Rm 12→8B → Uop 20→16B →
Entry 24B・表3MiB。副次で Uop が16Bに収まり **ABI値渡しがメモリ経由から
レジスタ2本に変わる**。正直な注意: #111でEntry 4B痩身単独はワッシュだった —
8B+ABI変化という跳びの大きさで挑み、負けたら台帳へ。

### バッチD: 外部技法

| # | 技法 | 出どころと実測 | 裁定 |
|---|---|---|---|
| **C14** | **dead-flags elimination** — デコード時にブロック内でフラグ死活を後方スキャンし、死んだ定義は**lazy状態のストアすら省く** | Box64/QEMU恒久採用。Bochsは類似改良1件で全体+5%実測 | **D1の本命**。uopキャッシュがあるので解析はデコード時にほぼ無料。決定性・命令数に無影響 |
| **D5** | victim TLB (直写像の後ろに8エントリ全連想) | QEMU実測 SPECINT平均+10.7% | 🔒 TLBミス率censusが先 (C7ワッシュ=ヒット率が高い示唆もある) |
| **E7** | HLT時計warp (仮想時刻だけ次のタイマ期限へ) | QEMU icountと同型 | 命令数決定性を保つ唯一のidle高速化。定規には効かず**ブラウザのアイドルCPU**に効く製品品質玉 |
| **B7** | dispatch系 (threaded / tail-call `become`) | M1のcomputed goto実測+10%どまり、Rohou CGO'15「ITTAGE世代でdispatchミスは支配的でない」 | 💤 期待薄。becomeはnightly。やるならミス予測率の実測 (kperf系、要sudo) が先 |

Dolphinのspin loop自動検出は**採らない** — MMIO副作用を見落として最大25%退行し
revertされた実績があり、命令数決定性が門番の本機では危険度最高。

## 触らないと確定した土地 (今回の再調査で裏も取れた)

チェーン脱出のはしご (跨ぎ連結で退出1/3000にしてもワッシュ = 使い切り)、
ホットフィールド集約 (exp/hotstate ±0)、tick_devices (~2%で床下)、
REP内側 (ページ単位一括済み)、割り込み配送 (76万命令に1回)、
Hugepage (macOSは16KBページで既に圧が低い)。

## 教訓

- **コメントの数字は腐る** — 「768KB」を信じていたらEntry痩身の優先度を
  見誤っていた。サイズはレイアウト規則から計算するか実測する
- **外部技法は「実測値つき」だけ持ち込む** — dead-flags (Bochs+5%) と
  victim TLB (QEMU+10.7%) が残り、dispatch系はM1の実測 (+10%どまり) と
  論文で棄却方向へ。ペア融合の負けは「M1の予測器が文脈を学習済みだから」
  という事後説明を得た
