# 0027: core第4ラウンド — フォールバック税と具現化税を削る

日付: 2026-08-18 / 状態: 採用 / 関係: [core-roadmap](../reference/core-roadmap.md), [0020](0020-external-review-round3.md)

## 背景

第4ラウンド調査 ([core-roadmap.md](../reference/core-roadmap.md)) の実測:
従来経路落ちは命令の1.1%だが**時間の~6%** (1回~150-250ホスト命令 —
decode徒労+フルCpuクローンguard+チェーン切断)。別口で、部分フラグ書き
(シフト等) が**全6フラグを具現化してから上書き**する税 (eflags 3-4.5%+
shift_rot ~5%の複合)。JIT off/on両方に効き、**wasm (ブラウザ) にはほぼ満額**。

## 決定 (3本のPRに分けて、各段で交互A/B off/on)

### PR1: StrRep — REP文字列をdcache語彙へ + 語彙外の負キャッシュ

- **REPの要素側は一括化済み** (P0 bulk)。残る税は「REP命令が語彙の外に
  住む」こと。`Uop::StrRep{op, seg, rep}` を足し、execはcold_stroneと同形で
  `Decoder{rep: Some(...)}` を組んで **string::execへ丸投げ** — 意味論の
  原本は1つ、string.rsは無改造
- **命令数勘定は「REP全体=1命令」を維持** — 基線不変・門番無傷。割り込み
  受付粒度も現状 (REP完走後) と同一。巻き戻しはslim guard (76B) で従来の
  フルクローンと同値 (文字列opが書くのはregs/flags/メモリだけ —
  メモリ書きはどちらの経路でも巻き戻さない=再実行で同値)
- 対象はA4-A7/AA-AF (0x66/0x67つき・INS/OUTSは従来どおり語彙外 —
  io_permittedがtrap_ipを使う)
- **負キャッシュ**: decode_atがNoneを返した頭を (pa, gen) で控え、次回は
  デコードを飛ばして直接従来経路へ。世代照合つきなので自己書き換えで
  語彙内に化けても安全。fill_or_fallback (cold) 内だけの変更 —
  ホット路には1命令も足さない

### PR2: 部分materialize — 上書きされるフラグを具現化しない

shift_rot等のset_flag (部分フラグ書き) は materialize() で全6フラグを
具現化する — 直後にほぼ全部上書きするのに、古いPF (popcount) やAFまで
計算している。`materialize_for(kept: u32)` を足し、**書き手が保存する
ビットだけ**具現化する。可視のflags値はビット同一 (書き手が直後に
上書きするビットの中間値は誰にも見えない — 観測点は命令境界)。

### PR3: 0x66語彙の拡張 (センサスで的を絞ってから)

従来経路落ちの首位 (ブート6M命令)。現状の0x66語彙はmov 89/8Bだけ —
落ち先の命令分布をopstatsで取り、上位のAlu16等を足す。別ADRにせず
本ADRの範囲とするが、実装は分布を見てから。

## 検証

- 指紋2コース (jboot/jcmd) は**値も不変のはず** (勘定を変えないため) —
  変わったら設計違反として差し戻す
- cosim/ELKS/FreeDOS/3OS門番 + 交互A/B (JIT off/on両方・ブート/gcc窓両定規)

## 棄却・保留 (調査済み、core-roadmap参照)

- cmp+jcc融合 (B3前科・JIT重複 — PR1-3の後に実測次第)
- REPE/REPNE CMPS/SCASの一括実行 (WL依存 — センサス後)
- タイマ単一デッドライン (tick=256で床下)・PGO/BOLT (裁定済み)・
  handlers chaining (Rust `become` 待ち)
