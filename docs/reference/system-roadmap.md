# エンジン外の鉱脈 (第5ラウンド調査、2026-08-20)

core (インタプリタ) とJITの中身以外に取れる最適化の台帳。3方面
(装置/システム層・ゲスト側・ホスト/ブラウザ層) を外部事例
(QEMU/v86/Firecracker/Dolphin/Bochs系) から調査した。

## 前提: 2種類のレバーを混ぜない

- **エンジンのレバー** (ホスト側だけの変化): ゲスト可視の遷移が不変 →
  従来どおり交互A/Bで裁ける
- **イメージ/コースのレバー** (ゲスト側の変化): 命令数そのものが変わる =
  **コース変更**。指紋基線の張り替えを伴い、交互A/B不能。裁くのは
  「新コースでの絶対時間」と体感。速度の定規 (jboot/jcmd) とは帳簿を分ける

## 本命: 起動済み状態イメージの配布 (v86/Firecracker式)

**ブートの970M命令を丸ごと消す、この台帳で唯一の「桁」級。**

- 一度ブートした状態を丸ごと控えて配布し、ユーザーは復元から始める。
  v86の公開デモは全部この方式 (`initial_state` + zstd圧縮)。Firecrackerの
  スナップショット復元は4-10ms、Lambda SnapStart実測p50 3.2ms
- **土台は実装済み**: save_state/from_snapshot はCPU+装置+メモリ+ディスク
  全部入り (snapshot.rs)、復元0.93s実測済み (PR #37)。残作業は「配布物」化 —
  圧縮・web UIの起動フロー・状態イメージの焼き方 (tools化)
- **SnapStart型の発展**: ブート直後でなく「gccを一度走らせてページキャッシュが
  温まった状態」で撮れば、squashfs読み+ゲスト内zstd展開+ld.so解決まで消える
- 決定性との関係: スナップショットからの実行はリセットからの実行と同格に
  決定的。**状態イメージのハッシュが新しい基線になる**だけで契約は無傷
- リスク: 状態イメージはデバイスモデルの版に密結合 (v86も版ずれで壊れる)。
  版番号を焼き込み、ずれたら「普通にブート」へフォールバック
- 出典: [v86 archlinux.md](https://github.com/copy/v86/blob/master/docs/archlinux.md) /
  [Firecracker snapshot-support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md) /
  [SnapStart (Brooker)](https://brooker.co.za/blog/2022/11/29/snapstart.html)

## イメージ/コースのレバー (ゲスト側、価値/工数順)

1. **ブートパラメータ一式 (即日・可逆)**: Firecrackerのベンチ用argsが
   そのまま買い物リスト —
   `quiet` (printk 1文字ごとのLSRポーリング+ポートI/Oが消える —
   エミュレータでは効きが増幅される)、`lpj=` (calibrate_delayスキップ。
   決定的なので一度ログから読めば固定できる)、`i8042.*` 4点
   (不在HWのprobe空回り)、**`mitigations=off`/`nopti`**
   (32bitはPCID無し=syscall毎にTLB 2回フラッシュ — ブートよりgcc窓の
   実行命令数に効く。エミュレータ内ゲストに投機の脅威モデルは無い)。
   [test_boottime.py](https://github.com/firecracker-microvm/firecracker/blob/main/tests/integration_tests/performance/test_boottime.py)
2. **カーネル差し替え/自作**: まずAlpineの **linux-virt** フレーバー
   (32bit x86有り、virtio特化でltsより小さい — リビルド不要) → 効けば
   Firecracker式の自作config (125msでinit到達の実在比。ただし公開configは
   全部64bit、32bitは自作)。initcall_debug+命令カウンタで970Mの内訳を
   区間割りしてから削る — 決定的なので区間切りが正確に出るのは
   ネイティブに無い利点
3. **圧縮の選択**: bzImage−vmlinux≒**190M命令が展開コストの実測値**
   (770M vs 970M級、2026-08-14の再予算)。vmlinux直接ロードは実装済みなので
   残る玉は (a) jboot課程のbzImage→LZ4再圧縮 (XZ/gzipより展開が桁で軽い)、
   (b) squashfsの圧縮をzstd/lz4へ (ゲスト内展開はゲスト命令)
4. **gcc課程の軽量化**: `gcc -pipe` (一時ファイルのsyscall/squashfs往復を
   排除)、`norandmaps` (速度でなく再現性 — gcc実行の命令列がrun間で同一化し
   ベンチ分散が消える)。muslは既に正解 (glibc化は逆行)

注: 1-4は本命 (状態イメージ) が完成すると価値が下がる — ただし
「状態イメージを焼く初回ブート」と指紋用コースには残る。

## 装置/I/O層のレバー (エンジン側、決定性無傷)

- **シリアル/コンソール経路のバッチ**: ゲスト可視 (UARTレジスタ挙動) は
  そのまま、ホスト側だけ束ねる — worker→UIのpostMessageをバイト毎でなく
  チャンク毎に、xterm側はフロー制御 (xterm.jsは main thread bound で
  ~5-35MB/sが上限、postMessage毎バイトは定番のアンチパターン)。安い。
  [xterm.js #3368](https://github.com/xtermjs/xterm.js/issues/3368)
- **決定的な仮想完了時刻つき非同期I/O**: ゲストがキューを蹴った瞬間に
  ホスト側の実I/O (fetch/WSS送信/ファイル読み) を非同期発行し、完了割り込みは
  **仮想時刻の純関数**として予定 — 間に合わなければホストが待つ (壁時計だけ
  損して命令数は不変)。NW編の「刻み+寝」の一般化で、ブラウザ配布の
  ディスク遅延ブロック取得と組む前提技術。QEMU iothread/io_uringの
  「分解だけ移植」版
- **遅延ブロック取得 (v86 9p / Firecracker uffd式)**: 128MBのdisk-gcc.imgを
  先に全部落とさず、触られたブロックをHTTP Rangeでオンデマンド取得+
  ブロック毎zstd。状態イメージ配布と合わせると「即対話・ワーキングセットは
  流れてくる」— 50並行microVM構想のブラウザ側前提でもある
- **virtio EVENT_IDX (割り込み抑制)**: VM exitの無いrustx86では効きは
  「ゲスト側ISR実行の命令数減」のみ。RX高負荷でだけ意味があり、
  早出し棄却 (刻みモデル) と整合させる制約つき — センサスしてから
- **HLT警備の確認**: HLT早送りは実装済み (PR #37)。gcc課程のI/O待ちで
  warp半分 (deadline直行) が取り切れているかの点検だけ残す

## ホスト/ツールチェーン (エンジン側、安い順)

- **wasm-opt `-O`→`-O3`**: wasm-packのreleaseは**既に `-O` で走っている**
  (manifest既定)。Cargo.tomlのmetadata 1行で-O3へ。期待0-10%、-O4は
  巨大関数 (ディスパッチ) でビルド爆発/退行リスクあり要A/B。
  wasm側の計測は配置ノイズに注意 (native-ruler-onlyの教訓 — 5周以上)
- **mimalloc (`#[global_allocator]`)**: macOSのsystem mallocは遅い部類。
  効くのはdcache構築/起動フェーズのみ (定常はアロケーションフリー、
  dynasmrtのmmapには乗らない)。2行で試せる。期待ブート0-5%
- **`#[cold]`/cold_path/手書き分岐ヒント**: 無税0-3%。cold_pathは
  stabilization目前、それまでは#[cold]関数属性で
- **Wasm Branch Hinting (中期の玉)**: W3C標準・全エンジン実装済み・
  CheerpX (x86エミュレータ!) が実運用、数%〜十数%の報告。**手書きの静的
  ヒントなのでPGO棄却ポリシーに抵触しない** — ただしRust→wasmの生成口が
  未整備でpost-processツールが要る。
  [Leaning Tech](https://labs.leaningtech.com/blog/branch-hinting)
- **+simd128**: コアループは分岐/データ依存でベクトル化されずwash予想。
  試すのは安いが期待±0-2%。FP経路はNaNビットの確認条件つき
- **wasmのメモリ事前確保**: `--initial-memory` でゲストRAM分を最初から確保し
  実行中のmemory.grow (再確保+全ビューdetach) を踏ませない — ジッタ削減

## 速度以外の副産物 (調査で拾った価値)

- **NW入力の記録/再生 (QEMU rr式)**: WSS入着フレーム+仮想時刻の注入点を
  ログすれば、どのrunもwsslirpd無しでオフライン再現可能 — 「残骸の古い
  バイナリが偽のバグを生む」類の問題が再生可能になる。速度ゼロ・デバッグ価値大
- **norandmaps**: gcc窓の命令列がrun間で完全一致 → 窓FNVを「値も固定」の
  門番に昇格できる可能性

## 不可・見送り (理由つき)

- **BOLT**: Mach-O未対応で技術的に不可、かつperf訓練ベース=PGOと同じ
  ポリシーブロック。二重に閉じている
- **`-C target-cpu=apple-m1`**: aarch64-apple-darwinの既定が既にapple-m1
  (rust#109899)。no-op。`target-cpu=native` はM1でむしろ古いCPUが選ばれる
  既知問題 (rust#93889) — 触らない
- **Huge pages**: macOS arm64にsuperpage APIが無い (16KiB基でTLBリーチは
  元々4倍)。Linux THPは madvise 3行で0-3%だが、ソフトMMU越しなのでKVM事例
  より効かない — Linux定規が立ったときの小物
- **io_uring/iothread/multiqueue そのもの**: VM exitもsyscall洪水も無い
  rustx86には的が無い。多スレッド完了は壁時計依存=契約違反。
  「非同期発行+決定的完了」の分解だけ上で採った
- **coalesced MMIO / halt-polling / 適応的割り込み調停**: exit費用の無い
  世界では的無し、または壁時計依存
- **requestIdleCallback**: deadline 50ms+バックグラウンド絞りで連続実行に不適

## 判定の順序 (推奨)

即日で安い: wasm-opt -O3・mimalloc・シリアルバッチ (エンジン側、A/B可)。
イメージ側は boot args→linux-virt の順に「新コースの絶対時間」で裁く。
**桁を変えるのは状態イメージ配布**で、これは最適化というよりプロダクト
(microVM構想の一里塚) として別枠で計画する価値がある。
