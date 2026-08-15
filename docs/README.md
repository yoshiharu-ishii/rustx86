# rustx86 のドキュメント — 現代PCの成り立ちを辿る

このプロジェクトは「x86エミュレータを作る」と同時に、
**現代のPCがなぜこの形をしているのか**を辿る教材でもある。

PCは40年分の後方互換が地層になってできている。今のCPUも電源投入直後は
1978年の8086として起動し、そこから32bit、64bitへと自分で自分を昇格させていく。
その地層を下から順に自分の手で作り直すと、普段ブラックボックスとして
扱っているものが繋がって見えてくる。

## 目次 — 4つの棚

文書は役割で4つに分けてある ([Diátaxis](https://diataxis.fr/) の分類に倣った)。
「読み物」と「引くもの」を混ぜないのが、だれさせないコツ。

### 🧠 理解する (explanation — 通して読む読み物)

| 文書 | 一言で |
|---|---|
| [アーキテクチャ](explanation/architecture.md) | CPUと装置の接続図。なぜPCはこの形なのか。**最初はここから** |
| [ロードマップ](roadmap.md) | 深さ (Tier 1〜9) × 広さ (箱 B1〜B5) の計画全文。原本はここ (READMEには書かない) |
| [JITの考え方と仕組み](explanation/jit.md) | インタプリタ/AOT/JITの違い、なぜ速くなるのか、結果論の台帳 |
| [ネットワーク](explanation/network.md) | ブラウザの中のPCがインターネットに出るまで。灯りの読み方、ゲストごとのNIC |
| [ディスク](explanation/disk.md) | rootfsがRAMから引っ越すまで。virtio-blkの1往復、squashfs+overlayの乗り物 |
| [踏んだ罠の型](explanation/pitfalls.md) | 「教科書どおりに書いたのに罠」10型。他所でも踏み得るものだけ |

### 🔧 手を動かす (how-to — 作業のとき開く)

| 文書 | 一言で |
|---|---|
| [チートシート](howto/cheatsheet.md) | よく打つコマンドの早見表 (走らせる/測る/検証する/焼く) |
| [ビルドの最小構成](howto/build.md) | Rust導入〜wasm-packの中身、wasmの落とし穴 |
| [イメージの焼き方](howto/images.md) | 道具箱 (Docker) と5つのスクリプト。いつ何を焼き直すか |
| [perf.md の「起動経路の測り方」](reference/perf.md#起動経路の測り方) | bootphase / headless / ブラウザでの測定コマンド |
| [ルートREADME](../README.md) | 実行方法の本体 (用意するもの〜デバッガ〜ベンチ) |

### 📖 引く (reference — 必要な行だけ見る台帳)

| 文書 | 一言で |
|---|---|
| [CPU最適化の台帳](reference/perf.md) | 地図: 現在地・北極星 (v86実測)・全量カタログ・測定の規律 |
| [最適化の実験記](reference/perf-log.md) | 物語: 日付つきの謎解きの経過。追記専用 |
| [レジスタ一覧](reference/registers.md) | 何があり、何を決め、何をまだ持たないか |
| [CI — 何を見張らせるか](reference/ci.md) | パイプラインの段構えと「どの事故を止めるか」 |

### ⚖️ 判断の記録 (ADR — なぜそうしたかを追う)

| ADR | 決定 |
|---|---|
| [0001](adr/0001-16bit-cpu-and-cosim.md) | 16bitから始め、Unicornをオラクルに検証する |
| [0002](adr/0002-devices-and-16bit-unix.md) | 装置を作り、16bit UNIX (ELKS) を起動する |
| [0003](adr/0003-networking-in-the-browser.md) | ブラウザ内ネットワークをどこまで本物にするか |
| [0004](adr/0004-how-far-to-follow-the-bios.md) | BIOSにどこまで付き合うか (需要の有界/無界) |
| [0005](adr/0005-local-first-roadmap.md) | ローカルで完結するものを先にやる |
| [0006](adr/0006-hidden-segment-registers.md) | セグメントの隠しレジスタを実機と同じ構造で持つ |
| [0007](adr/0007-cpu-optimization-steps.md) | 最適化はデコード済みキャッシュを本丸に段階を刻む |
| [0008](adr/0008-template-jit.md) | テンプレートJITの設計・Fシリーズのロードマップ・wasm凍結 |
| [0009](adr/0009-pgo-shelved.md) | PGOは効いたが寝かせる — 運用判断を開発に持ち込まない |
| [0010](adr/0010-test386-full-compat.md) | test386でCPU互換を完璧にする — 速さの前に正しさを積む |
| [0011](adr/0011-tier-redraw-after-compat.md) | 互換達成後のTier引き直し — JIT完走を前倒し、バスは作らない |
| [0012](adr/0012-f1c-native-jit.md) | F1c-a — ネイティブJIT (Cranelift) の骨格と背景焼き |
| [0013](adr/0013-f1c-freeze.md) | F1c を凍結して削除する — 収支がリンク税を割った |
| [0014](adr/0014-external-review-hotpath.md) | 外部レビュー2本を台帳で裁く — ホットパス付帯処理の一掃 |
| [0015](adr/0015-cpu-opt-phase-close.md) | CPU最適化フェーズの終了 — 判別則4条と「理論ラウンドの門」 |
| [0016](adr/0016-platform-cfg.md) | プラットフォーム分岐の作法 — 同じコードでターゲット別の速さを取る |
| [0017](adr/0017-network-isa-first.md) | ネットワークの境界とバス順 — ISA NE2000 から始める |
| [0018](adr/0018-devices-chip-card-bus.md) | 装置は「なにか」と「どう見つかるか」で分ける — chip / card と bus |

## 読む順番 (初見の人向けの道)

1. **[アーキテクチャ](explanation/architecture.md)** — 図で全体像。扱っている問い:
   なぜブートセクタは0x7C00か / なぜセグメントは16倍か / IVTを「OSが乗っ取る」とは /
   なぜ1981年の8254 PITが現代PCにも生きているのか / 32bit化は「命令が増える」ことなのか
2. **[ビルドの最小構成](howto/build.md)** — `wasm32-unknown-unknown`の「OS無し」とは /
   wasm単体では文字列すら渡せないのになぜ`String`が返せるのか /
   wasmのメモリが伸びるとJSの`Uint8Array`に何が起きるのか
3. **[CI](reference/ci.md) → [perf.md](reference/perf.md)** — 検証と測定の流儀。
   「検査で見張るより、事故れない構造にする」の実例
4. **[jit.md](explanation/jit.md) → [perf-log.md](reference/perf-log.md) → [pitfalls.md](explanation/pitfalls.md)** —
   最適化の旅を仕組み→経過→教訓の順で
5. **ADRを0001から** — 判断の過程を時系列で追体験できる

## 実物を見る

図で位置を掴んでからソースを読むと早い。

| 見たいもの | 場所 |
|---|---|
| オペコードの振り分け (意味の原本) | [`core/src/cpu/onebyte.rs`](../core/src/cpu/onebyte.rs) |
| デコード済み命令キャッシュ (速い写し) | [`core/src/cpu/dcache/`](../core/src/cpu/dcache/) |
| JITの語彙と生成器 | [`core/src/cpu/dcache/jit.rs`](../core/src/cpu/dcache/jit.rs) / [`wasm/src/jit.rs`](../wasm/src/jit.rs) |
| フラグ計算の実体 (8/16/32bit) | [`core/src/cpu/alu.rs`](../core/src/cpu/alu.rs) |
| 電源投入時にBIOSがやること | [`core/src/bios.rs`](../core/src/bios.rs) |
| 装置 — 素子と基板 (PIC / PIT / UART / 8042 / CMOS / CRTC / DP8390) | [`core/src/dev/`](../core/src/dev/) |
| バス — 番地の地図と配線 (ISAの固定番地・PCIの設定空間) | [`core/src/bus/`](../core/src/bus/) |
| IBM PCの文字集合 | [`core/src/cp437.rs`](../core/src/cp437.rs) |
| 検証ハーネス | [`cosim/tests/alu.rs`](../cosim/tests/alu.rs) / [`tools/webtest/`](../tools/webtest/) |
| 実OSが起動するかのテスト | [`core/tests/elks.rs`](../core/tests/elks.rs) / [`freedos.rs`](../core/tests/freedos.rs) |
| 動く実例 | [`asm/hello.asm`](../asm/hello.asm) |

## 書き方のルール (だれさせないための運用規約)

perf.mdが469行に膨らんだ反省 (2026-08-12の分冊) から、置き場の規則を固定する:

1. **1ファイル1役割** — 地図 (引く) と物語 (読む) を同じファイルに書かない
2. **実験の経過は [perf-log.md](reference/perf-log.md) へ追記** — 日付見出しで足すだけ。
   perf.md側はカタログの状態欄とタグ (`exp/*`) への参照を1行更新する
3. **横断の教訓は [pitfalls.md](explanation/pitfalls.md) へ** — 「型→サンプルコード→事件→検知」
   の並びで。rustx86固有のものは載せない (それはperf-logの仕事)
4. **決定は ADR** — 追記はしても書き換えない。効いたのに見送る判断は
   賛否の議論と**復帰条件**まで書く (実例: ADR-0009)
5. **索引はこのREADMEだけ** — 目次の二重持ちをしない。ロードマップは
   [docs/roadmap.md](roadmap.md) が原本 (ルートREADMEにはリンクだけ。
   現在地は頻繁に動くので二重に持つとズレる。実際ズレた)
6. ツール (mdBook等のサイト生成) は入れない — ビルド工程という運用を増やさない。
   公開教材としてサイト化したくなったら、そのとき判断する (ADR-0009と同じ理屈)
7. **docsの変更もPRを通す** — mainへの直接pushはブランチ保護で塞いである
   (下記)。「docsだけだから」で例外を作ると、例外の判断そのものが運用になる

## mainへの直接pushは塞いである (2026-08-15)

変更は**必ずブランチ+PR**を通す。GitHubのブランチ保護で機械的に強制してあり、
管理者にも適用される (`enforce_admins`)。守るのは3つ:

- **PRを経ないpushを拒否する** (承認者の数は0 — 1人開発なので自分でマージできる)
- **CIの「OK (全層)」が緑でないとマージできない**
- **force push と ブランチ削除を拒否する** — 履歴は事故でも消えない

`OK (全層)` を必須にできるのは、ワークフローが `paths-ignore` を**使っていない**
からである。docsだけの変更でワークフローごと止めると、必須チェックが永久に
pendingのまま残って**docsのPRがマージできなくなる** (GitHubの定番の罠)。
代わりに中で絞っていて、重い検査 (CPU照合・互換) は paths-filter が
false なら skip され、`OK` は skipped を成功として数える。
**「必ず緑のOKが出る」と「重い検査を払わない」を両立させるための形**である。

緊急時に外すときは、外したことと理由を残す:

```bash
gh api -X DELETE repos/yoshiharu-ishii/rustx86/branches/main/protection
```

## この教材の立ち位置

エミュレータの解説は「命令セットの実装方法」に寄りがちだが、
実際に難しいのは**命令ではなく機構**である。割り込み、特権、アドレス変換、
装置との協調 — この辺りは仕様書を読んでも掴みづらく、
動くものを作ると腑に落ちる。

そこで本プロジェクトでは、各段階で**必ず何かが動く**ように目標を置いている。
「CPUは完成したが動かすものがない」という状態を作らないのが方針である。

64bit (ロングモード) はこのリポジトリでは扱わない。やるなら fork して
x86_64 専用機として設計し直す (Tier 9 構想)。
