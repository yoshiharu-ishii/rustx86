# coreの残り鉱脈 (第4ラウンド調査、2026-08-18)

「jit-roadmap3の前にcore側の最適化余地を広範囲に」の調査結果。入力は
(1) インタプリタ側 (JIT off) の新プロファイル、(2) 熱経路の静的分析
(step_cachedの帳簿~35命令/命令の分解)、(3) 外部サーベイ (Bochs高速化史・
DOSBox文字列・wasmi融合・QEMU icount)、(4) opstatsセンサス。

**結論: 単発の大物はないが、安い上位3つで合算5-10% (インタプリタ側) が
見込める。JIT onの全体では未カバー分 (28-44%) に効いて~3-6%。**
wasm (ブラウザ) はインタプリタ主体なのでほぼ満額効く — ブラウザ体感には
JIT側より効率が良い投資。

## 実測の土台

- インタプリタ自己時間: step_cached一枚岩 69-72% / alu32 ~5% /
  **shift_rot ~5%** / **eflags具現化 3-4.5%** / condition ~2% /
  translate_for ~2-3% / decode_at ~1-2%
- 帳簿の静的分解: 1命令あたり帳簿~35 + 実行~10-30ホスト命令。帳簿の
  個別削りはOoOの影 (C12の教訓) — 効くのは「周回・呼び出しそのものを
  消す」型だけ
- センサス (ブート970M): **従来経路落ち11M命令 (1.1%) × 1回~150-250命令
  ≈ 時間の~6%**。内訳: **0x66プレフィクス 6M / REP 1M / 語彙外op 2M**
- M1の律速は帯域でなく**依存チェーン長と分岐ミス** (Firestorm 8-wide、
  分岐ミス~13-14cy) — 「ディスパッチ回数を減らす」方向が正解

## 候補 (期待値×コスト順)

### 1. StrRep — REP文字列をdcache語彙に入れる (小、~2%)

前提の訂正: REPの**要素側は一括化済み** (P0のbulk_movs/stos)。残る税は
**REP命令がdcacheの外に住む**こと — 毎実行、decode_atの徒労+フルCpu
~400Bクローンのguard+チェーン強制切断で~150-250命令。
`Uop::StrRep{op,seg,rep}` を足してstring::execへ丸投げ (原本1つ維持・
string.rs無改造)。命令数勘定は現行の「REP全体=1命令」を**維持** —
基線不変・門番無傷。INS/OUTSは語彙外のまま。~60-80行。

(参考: Bochs/QEMU/DOSBoxは「反復ごと1命令」勘定だが、それは彼らの基線。
うちの基線を動かす理由はない)

### 2. 0x66語彙 — 16bitオペランド命令の主要形をdcacheへ (中、~3-4%ブート)

センサス首位 (6M命令)。decode_atは0x66を一括拒否している (fb_reasons[0])
が、Mov16RmR/RRm uopは既に存在する — 拒否の解除+Alu16/Test16等の頻出形
追加。落ち先の分布 (どの0x66命令が多いか) をopstatsで見てから的を絞る。

### 3. 部分materialize — シフト等の全フラグ具現化をやめる (小、~2-3%)

新発見 (プロファイル+コード読み): shift_rot等の部分フラグ書きは
set_flag→**materialize()で全6フラグを具現化** — 古いPF (popcount) や
AFまで計算してから、直後にほぼ全部上書きしている。
`materialize_for(preserved_mask)` で**残るビットだけ**具現化すれば、
シフト~5%+eflags~4%の複合税の大半が消える。決定性無傷 (可視のflags値は
同一)。Bochsの「常時lazy・carry-outベクタ」はこの先の完成形 (大工事) —
まず部分materializeで安く取る。

### 4. 語彙外の負キャッシュ (極小、<1%)

decode_atがNoneを返した頭に「語彙外」印 (tag+gen) — 次回はdecodeを
飛ばして直接step。1と同PRの小玉。

### 5. REPE/REPNE CMPS/SCASの一括実行 (中、WL依存)

bulk_movsと同形でページ内スライス比較、最初の不一致で止めてフラグは
cmp_wで立てる — 1要素単位でビット同一。strlen/strcmp密度の高いWLで効く。
op_counts[A6/A7/AE/AF]のセンサスが先。

### 6. cmp/test+jcc融合uop (中、1-3%、B3の前科あり)

レジスタ/即値形3種に限定・2命令会計・予算ゲート (extra≥1 && tick≥2で
ペア内tickなし保証)。条件を満たさなければcmp半分だけ実行して分割 —
cosim/lockstepの毎命令観測は無傷。**JITカバー領域と重複** (JIT内は既に
融合相当) なので取り分はインタプリタ実行分のみ。B3 (-20〜28%) の再演
リスクあり — 交互8周で即決の覚悟で、優先度は1-3の後。

### 7. 小物 (各<1%、常設可能)

- victim cache 8エントリ (Bochs式エイリアシング対策)
- codegen-units=1 + fat LTO (未設定なら)
- PFのpopcnt化等のbranchless小物
- ディスパッチmatchのjump table化をcargo asmで現物確認

### 見送り (理由つき)

- **タイマの単一デッドライン化**: Bochs/QEMU型の正解だがtick=256で
  tick処理は~0.2命令/命令相当まで床下 — StrRepのクリップ枠が必要に
  なったら再訪
- dead-flags (C14 🔒)・tick一括払い (C5 +10%悪化)・Entry痩身/dTLB等:
  実測ワッシュ済み (台帳)
- 世代照合のホイスト: 「照合は毎命令」の契約に抵触 — 却下
- PGO/BOLT: 裁定済み分類 (運用判断)。BOLTはそもそもMach-O未対応
- handlers chaining (Bochs 2.5): tail-call相当 — Rust `become` 待ち

## 判定の順序 (推奨)

1+4を1本のPR (基線不変・門番のみ) → 3を1本 → 2 (センサスで的を絞って)
→ 5/6は実測次第。全部でインタプリタ側5-10%・ブラウザ(wasm)にほぼ満額・
ネイティブJIT on全体で3-6%の見込み。**jit-roadmap3 (常駐化、窓-6〜8%)
とは独立に積める** — 順序はどちらが先でも干渉しない。

## 出典 (第4ラウンド)

- [Bochs faststring.cc](https://github.com/bochs-emu/Bochs/blob/master/bochs/cpu/string.cc) /
  [How Bochs Works Under the Hood 2nd ed.](https://bochs.sourceforge.io/How%20the%20Bochs%20works%20under%20the%20hood%202nd%20edition.pdf) /
  [Bochs 2.3.7 (trace cache、2.3.5比2倍)](https://sourceforge.net/p/bochs/news/2008/06/bochs-237-released/)
- [QEMU icountとrep勘定](https://lists.gnu.org/archive/html/qemu-devel/2014-12/msg00607.html) /
  [tcg-icount](https://www.qemu.org/docs/master/devel/tcg-icount.html)
- [DOSBox-X string.h (再開可能な文字列命令)](https://github.com/joncampbell123/dosbox-x/blob/master/src/cpu/core_normal/string.h)
- [wasmi v0.32 (cmp+branch融合)](https://wasmi-labs.github.io/blog/posts/wasmi-v0.32/) /
  [Ertl & Gregg superinstructions](https://www.scss.tcd.ie/David.Gregg/papers/toplas05.pdf)
- [dougallj Firestorm (依存チェーンが律速)](https://dougallj.github.io/applecpu/firestorm.html)
- Rust: [cold_path安定化](https://github.com/rust-lang/rust/pull/151576) /
  [BOLT (Mach-O未対応)](https://github.com/llvm/llvm-project/blob/main/bolt/README.md)

関連: [jit-roadmap3.md](jit-roadmap3.md) / [perf.md](perf.md) / [perf-log.md](perf-log.md)
