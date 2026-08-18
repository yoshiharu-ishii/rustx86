# MIPSの残り鉱脈 (第3ラウンド調査、2026-08-18)

「これ以上MIPSを稼ぐ方法はないのか」への答え。3本の入力 — (1) tick=256での
新プロファイル (混合が第2ラウンドから変わった)、(2) 生成コード密度の静的分析
(レジスタ常駐化の理論値)、(3) 未踏領域の外部サーベイ (fastmem/NZCV/トレース/
インタプリタ技法) — を統合した台帳。結論: **まだ2倍級の鉱脈が1つ (fastmem)、
10%級が2つ (常駐化バンドル・焼き直しの嵐)、実験枠が複数ある**。

## 新プロファイル (tick=256、JIT on、sample自己時間)

| 行き先 | ブート | gcc窓 | 備考 |
|---|---|---|---|
| インタプリタ (未カバー命令) | 44.6% | 28.0% | カバレッジはブート~56% / 窓**72%** (tick=256で予算が伸びた) |
| 生成コード | 14.8% | 23.1% | **JITはインタプリタの~3.2倍の密度** (窓: 72%の命令を23%の時間で) |
| bake系 (memmove+mprotect+munmap) | ~3.6% | **~9%** | 窓内10万回級の焼き直し (焼き108k vs 据付86k) |
| try_enter | 4.4% | 4.8% | |
| ヘルパ (st32/push/ld32/pop) | ~4.5% | ~7.7% | ストア系はTLBインライン未実装 |
| shift_rot / eflags | 4.3% / 3.3% | 1.6%程度 | シフトは全部ヘルパ・eflagsの呼び手は要調査 |

**重要な発見**: collect断片化はほぼ解消済み (センサス6k回のみ — grp3 not/neg
とimulが残るだけ)。未カバーの正体は語彙ではなく**最小ブロック長4未満の断片
(棄却59k) と降格**。つまり「語彙を増やす」フェーズは終わり、
「断片の経済」と「メモリアクセスの構造」のフェーズに入った。

## 鉱脈ランキング

### 1. fastmem (ホストMMU流用) — 2倍級、唯一の大物

ゲスト空間をホストにmmapし、ロード/ストアを `base+addr` の1-2命令に。
TLBプローブ (現状ヒットでも~7-10命令) と範囲検査とヘルパ呼びが全部消える。

- 実績: yuzu 15-60%改善・Dolphin「MMU完全エミュ比10x」・**Dolphin 2603は
  ページング有効ゲストでも成立させて全体2倍** (PR #13768 — ゲストページ
  テーブルの線形ミラーをmmapで構築、invlpg/CR3フックで差分更新、
  Dirtyビットは遅延マップで解決)
- macOS/Apple Silicon nativeの実例あり (Dolphin JitArm64_BackPatch.cpp)。
  SIGSEGV+SIGBUS両フック・pthread_jit_write_protect_npはper-thread
  (ハンドラ内でトグル可) ・16KiBホストページの扱いも先例あり
- **決定性◎** (実行経路が変わるだけで命令数・割り込み位置は不変)。
  tick粒度と独立
- 段階案: 第1段=物理恒等 (実モード・PG無効期) のみ直接写像 →
  第2段=線形ミラー (Dolphin式)。**インタプリタの読み書き速い道にも同じ
  ミラーが使える** = 最大バケツ (interp 28-44%) にも効く両取り
- 重量: 大。wasmには載らない (ネイティブ定規専用、方針とは整合)

### 2. レジスタ/cc常駐化バンドル — 10%級 (静的分析の裏取りあり)

生成コードの限界コストは現状**~13ホスト命令/ゲスト命令**、常駐化で**~8.7
(-33%)**。壁時計換算: gcc窓-6〜8% (JITの窓優位+4-5%→+10-13%圏)、
ブート-4〜6% (133→~140 MIPS)。ノイズ床±3%を超え単体判定可能。

推奨順 (密度分析の結論):
0. **無料の巻き上げ** (ABI不変): gen_addr/mem_len/CPL/segベースのブロック頭
   掴み置き — genckが8→3命令、これだけでストア系-30%
1. **ストアTLBインライン**: 単体はノイズ床未満 (~1%) だが、blr境界が減って
   常駐化のflush難所を物理的に減らす前座
2. **6本常駐+cc材料常駐**: ESP/EBPは初版メモリ残留 (h_push/pop/shiftが
   メモリのゲスト状態を読むため)。レジスタ予算はx19-x22使用済みで6本が現実解。
   全脱出点でdirty書き戻し+cc具現化の契約 (エミット時に静的確定 — 決定性無傷)
3. その先に**NZCV常駐 (FEX式)**: 実測17-60%の実績。carry反転は「反転のまま
   運んで消費点で吸収」(FEX-2409)、PF/AFは別レジスタに遅延分離。
   具現化点管理が本体 — 常駐化の帳簿ができてから

### 3. 焼き直しの嵐の鎮火 — 窓~9%が的、安い

窓のbake系9% (memmove 4.6=dynasmrt finalizeのコピー、mprotect/munmap)。
焼き108k vs 据付86k = 2万超の再焼き+初焼きも重い。手:
- スロットの**2-way化** (衝突退去の削減 — 256K→2Mで359k→129kに減った
  実績の続き。Blockは既にBox化済みでway追加は安い)
- dynasmrtのAssembler/バッファ再利用 (毎bakeのmmap+memmove+mprotectを
  アリーナに)
- 損益降格の閾値スイープ (PROFIT_MIN_AVG 3→4/5、MIN 4→6): 入場~70命令の
  今、平均3命令ブロックの黒字性は怪しい — 1行変更で測れる実験

### 4. 実験枠 (安い・すぐ測れる)

- **opcode fusion** (cmp+jccをdcache IRで融合 — 計数は2のまま):
  インタプリタとJIT両方に波及。wasmi 5xの主成分がIR+fusion
- **eflags 3.3% (ブート) の呼び手調査** — 誰が全フラグ具現化を踏むのか
- **シフトのインライン** (shift_rot 4.3%ブート — imm形の常用シフトだけでも)
- ベンチ安定化: QoS誘導 (`taskpolicy`/QOS_CLASS) — 熱ダレとの併用術

### 見送り (理由つき)

- **トレース/スーパーブロック**: tick粒度の天井が再来 (chainingと同型)。
  テンプレートJITではレジスタ昇格の利得も取れない (HQEMUの2.4xはLLVM込み)
- **2層JIT**: 重量特大・「税なしテンプレート」路線と衝突。copy-and-patchの
  「テンプレのバリアント増」だけ拝借価値あり
- **インタプリタtail-call化**: Rust `become` はstabilize 2027目標。正味1-5%
  (CPython検証) でcold pathのみ — 待ち
- **AMX/SME**: スカラエミュレーションに用途なし
- **JIT台帳のスナップショット同梱**: 速度非寄与 (起動レイテンシ専用)。
  ブラウザのbake嵐対策としてのみ再訪価値

## 出典 (第3ラウンド)

- Dolphin: [PR #13768 ページング下のfastmem](https://github.com/dolphin-emu/dolphin/pull/13768) /
  [JitArm64_BackPatch.cpp](https://github.com/dolphin-emu/dolphin/blob/master/Source/Core/Core/PowerPC/JitArm64/JitArm64_BackPatch.cpp)
- [yuzu fastmem](https://yuzu-testing.netlify.app/entry/yuzu-fastmem/) /
  [RPCS3 arm64移植記](https://blog.rpcs3.net/2024/12/09/introducing-rpcs3-for-arm64/) /
  [RPCS3 W^X RAII #18701](https://github.com/RPCS3/rpcs3/pull/18701)
- [FEX-2312 NZCV](https://fex-emu.com/FEX-2312/) / [FEX-2409 carry反転](https://fex-emu.com/FEX-2409/)
- [wasmi v0.32 (IR+fusion 5x)](https://wasmi-labs.github.io/blog/posts/wasmi-v0.32/) /
  [CPython tail-call検証 (正味1-5%)](https://blog.nelhage.com/post/cpython-tail-call/) /
  [Rust become #144232](https://github.com/rust-lang/rust/pull/144232)
- [Copy-and-Patch](https://arxiv.org/pdf/2011.13127) / [HQEMU](https://homepage.iis.sinica.edu.tw/papers/dyhong/18239-F.pdf) /
  [Firestorm資料 (L1i 192KB)](https://dougallj.github.io/applecpu/firestorm.html)

関連: [jit-roadmap2.md](jit-roadmap2.md) (第2ラウンド) / [ADR-0024](../adr/0024-f1d-i-chaining.md) / [ADR-0025](../adr/0025-tick-256.md)
