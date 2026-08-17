# JIT高速化の候補台帳 (第2ラウンド、2026-08-17 広範囲調査)

3つの入力から作った台帳: (1) sampleプロファイル実測 (jboot/jcmd両定規のJIT on)、
(2) 外部技法サーベイ (QEMU TCG / Rosetta 2 / FEX-Emu / Box64 / v86 / Dolphin)、
(3) 入場路と雛形の静的命令数見積。効かなかった案もここに寝かせる (棄却しない)。

## 実測の時間内訳 (sample、自己時間ベース)

| 行き先 | jboot (カバレッジ43%) | gcc窓 (58%) | 中身 |
|---|---|---|---|
| インタプリタ (step_cached自己) | 48% | 37% | 未カバー命令の実行 |
| 生成コード (無名領域) | ~14% | ~20% | 雛形の実行 |
| try_enter + TLS | 8.3% | 8.0% | 入場機構 |
| ヘルパ群 (shift/eflags/alu/条件/push/pop/store) | ~13% | ~10% | 呼び側固定費が支配的 |
| translate_for | 2.7% | 2.4% | TLBプローブ (フルウォークは冷路) |
| bake系 (mmap/mprotect/memmove/emit) | ~4.4% | ~4.9% | 再焼き含む |

定量の要 (静的見積): **1入場 ≈ 90-100ホスト命令**の固定費
(ランタイム側60-70 + プロローグ/エピローグ26)。平均8命令/入場だから、
入場固定費だけでゲスト1命令あたり~12ホスト命令を払っている。

## 外部技法ランキング (決定性を壊さず効きそうな順)

1. **TB chaining (直接ブロック連結)** — QEMUの主砲。ブロック末尾のB命令を
   実行時パッチして次ブロックへ直接飛ぶ。dispatch税 (=上の8%+清算) を消す。
   icountモード同居の実績があり決定性と両立 (ブロック頭で予算検査を残す)
2. **間接分岐キャッシュ (tb_jmp_cache型)** — ret/jmp regはチェーンできない
   ので、PC→ホスト番地の小ハッシュをヘルパで引いて直接飛ぶ。1と対で効く
3. **guest→hostレジスタ固定写像** — AArch64はGPR31本。x86の8本を
   x23-x28+αに常駐させ、雛形からロード/ストアを消す。ヘルパ境界で書き戻す
   規約が本体。Rosetta 2の土台 (最適化パス無しでもネイティブ比7-8割の根拠)
4. **ストアのTLBインライン** — ロード (F1d-d) の書き込み版。note_writeの
   ビット検査 (page_has_codeが立っていなければ即done) までインライン化
5. **NZCV直写像 (FEX式)** — cc_op方式の上に「フラグはホストNZCVに常駐」を
   重ねる。FEX実測+17.6〜60%。g2 (条件インライン) はこの入口だった
6. **CALL/RETのreturn stack (Box64 +10%)** — 難度高・後回し
7. **fastmem (Dolphin式ホストMMU流用)** — 効果最大だが実装最重・wasm不可

## 今回の実測 (F1d-h: 入場路の痩身バッチ)

- entry引数化 (mov_abs×3→mov×3、-6命令/入場)・出口ip番地のx19相対化
  (-2命令/出口×5箇所)・ヘルパ表x22 (mov_abs→ldr、-2命令/ヘルパ呼び×26箇所)・
  TLS排除 (JitHook.ctx持ち込み、tlv呼び1本-5〜10命令/入場)・Slot 16B化
  (Block Box化 — 表128MiB→32MiB)
- **単体では交互A/Bのノイズ床 (±3%) 以下で判定不能**。構造根拠 (入場固定費
  90-100→70-80命令、表のキャッシュ密度) で採用。指紋2コースはビット同一
- 教訓: この規模 (期待0.2-0.3s/9s) の削減は単体では測れない。束ねて、
  より大きい梃子 (チェーン) と同時に裁くこと

## 寝かせた案 (理由つき)

- **store_ccのstp化**: 8→7命令にしかならない (str×4→add+stp×2は-1)。
  cc_op/wの隣接strhはフィールド整列が保証できない。レジスタ常駐化と一緒にやる
- **入場路のさらなる微調整**: 損益カウンタのRMW×2はPROFIT_SAMPLE到達まで
  毎入場払う — 「殿堂」を復活させると再審が消える (トレードオフ、様子見)

## 次の一手 (推奨順)

1. **F1d-i: TB chaining** — 直接分岐終端 (Jmp/CallRel/両側焼きJccのtaken側) の
   出口を「次ブロックのentryへ直接B」にパッチ。設計の要:
   - 予算の契約: 着地側ブロックの頭で「n ≦ 残り予算」を自前で検査する形に
     変える (今はtry_enterが検査)。残り予算はレジスタで持ち回す (x23等)
   - 清算: 連結中はtsc/tick/extraをレジスタで積算し、チェーン退出時に一括清算
     — ただし**割り込み受付点が変わらないこと**の証明が本体 (budgetが上限を
     保証するので、tick境界そのものは崩れない — ADRで裁く)
   - SMC/世代: パッチ済みリンクは世代が動いたら剥がす (リンク元台帳が要る)
   - 期待値: 入場機構8% + core清算 (step_cached自己時間の一部) + 生成コードの
     出入り26命令×53M — 合計10-15%級
2. **F1d-j: レジスタ固定写像** (3) — チェーンで入場回数が減った後の方が
   設計しやすい (フラッシュ境界が減る)
3. ストアTLBインライン (4) は独立に安い — チェーンの合間に

関連: [ADR-0023](../adr/0023-f1d-g-density.md) / [perf-log.md](perf-log.md)

## 出典 (外部サーベイ)

- QEMU: [Translator Internals](https://www.qemu.org/docs/master/devel/tcg.html) /
  [TB chaining](https://mail.gnu.org/archive/html/qemu-devel/2021-05/msg08441.html) /
  [TCG deep dive (airbus-seclab)](https://airbus-seclab.github.io/qemu_blog/tcg_p1.html) /
  [lazy cc](https://patchwork.kernel.org/patch/9367281/) /
  [victim TLB](https://lists.gnu.org/archive/html/qemu-devel/2014-08/msg02133.html)
- Rosetta 2: [Why is Rosetta 2 fast? (dougallj)](https://dougallj.wordpress.com/2022/11/09/why-is-rosetta-2-fast/) —
  1:1変換+命令境界の状態正準化でネイティブ比7-8割 (テンプレートJIT路線の裏付け)。
  PF/AFのハード支援・TSOはユーザーランド不可
- FEX-Emu: [FEX-2312 (NZCV直写像)](https://fex-emu.com/FEX-2312/) — Geekbench +17.6%
- Box64: [CHANGELOG (CALLRET +10%/FORWARD +30%)](https://github.com/ptitSeb/box64/blob/main/docs/CHANGELOG.md) /
  [dynarec解説](https://box86.org/2021/07/inner-workings-a-high%E2%80%91level-view-of-box86-and-a-low%E2%80%91level-view-of-the-dynarec/)
- v86: [how-it-works (ページ束ね+br_table)](https://github.com/copy/v86/blob/master/docs/how-it-works.md) —
  wasm側 (Stage 3の続き) の設計はこれが原本
- Dolphin: [fastmem/バックパッチ](https://www.tumblr.com/accidentallyquadratic/142829260822/dolphin-emulator-trampoline-generation) /
  [BLR最適化 PR #12079](https://github.com/dolphin-emu/dolphin/pull/12079)
