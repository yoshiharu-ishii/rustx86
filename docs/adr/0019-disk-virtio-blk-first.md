# ADR-0019: ディスクは virtio-blk-pci から — rootfsをRAMから引っ越すために

- 状態: **決定** (2026-08-15 ユーザー決定、2026-08-16 実装)
- 日付: 2026-08-16
- 関連: [ADR-0018](0018-devices-chip-card-bus.md) (chip/card/busの置き場所)、
  [ADR-0017](0017-network-isa-first.md) (virtio-mmio偵察の結果)、
  [ADR-0002](0002-devices-and-16bit-unix.md) (INT 13h HLE = 唯一の捨て仕事)、
  [disk.md](../explanation/disk.md)、[PR #159](https://github.com/yoshiharu-ishii/rustx86/pull/159) / [#160](https://github.com/yoshiharu-ishii/rustx86/pull/160)

## 背景 — なぜディスクが要ることになったか

きっかけは gcc だった。「このPCモドキでgccを噛まして hello, world を実行したい」
を initramfs 方式で叶えたら、**構造的な限界**に正面からぶつかった:

- initramfs は起動時に**全部**をtmpfsへ展開する。gcc一式 (展開88MB) を積むと、
  展開中は圧縮イメージと展開済みの中身が同時にRAMに載り、**256MBを要求**した
- 足りないときカーネルは落ちずに**黙って途中でやめる** — 192MBでは
  `collect2` だけが欠け、「`gcc -c` は通るのにリンクだけ失敗する」機械になった
  ([pitfalls #14](../explanation/pitfalls.md))。ローダの算数 (`initrd_ram_needed`)
  で入口は塞いだが、それは症状の検知であって原因の解消ではない
- この先に積んであるもの — ReactOS、X付きのOS、純正UNIX (386BSD/Minix) —
  は**全部ディスクを前提にしたOS**である。initramfsで運べるのはLinuxだけ

つまり「ディスクをやるか」ではなく「どの口から始めるか」が問いだった。

## 選択肢 — ATA / SATA / virtio-blk

エミュレータでは**I/Oトラップこそがコスト**である。ゲストがIN/OUTを打つたびに
実行ループを抜けてデコーダ→装置と往復する — 実機のポートアクセスより
桁違いに高くつく。この物差しで3案を比べた:

| | 1セクタ (512B) の運び方 | うちでの実コスト | 実装の面 |
|---|---|---|---|
| ATA (PIO) | `insw` ×256 | **トラップ256回** | 狭いが、タイミングの癖・IDENTIFYの方言が本体 |
| ATA (UDMA) | バスマスタIDE + PRD表 | DMAで安い | virtio並の量 + PIIXの癖が付いてくる |
| SATA (AHCI) | HBA + コマンドリスト + FIS | DMAで安い | **コマンド層が厚い**。同じ結果に面が広すぎる |
| virtio-blk | 記述子にゲスト物理が載る | **通知1回 + memcpy** | リング1本。レジスタ窓24バイト |

virtio-netのときの偵察 (ADR-0017) と違い、ブロックには障害物が無かった:

- **PCIは既に動いている** (RTL8029で実証済み)。virtio-mmioのときの
  「発見の口が無い」問題は、PCIの上に建てるなら最初から存在しない
- **ドライバは全部 Alpine lts の initramfs に居る** (`virtio_blk.ko` ほか5つ、
  すべて `=m`)。NICのモジュールと同じ荷物から借りるので vermagic が必ず合う
- transport は **legacy (0.9.5)** を選ぶ。modern (1.0) はPCI capability経由の
  MMIO窓で面が広い。legacyはBAR0のI/Oポートで完結し、Linuxは両方喋る

## 決定

1. **virtio-blk-pci (legacy) を最初のディスクの口にする。** 速さの理由
   (トラップ1回 vs 256回) と、土台が全部揃っている理由の両方で
2. **ATA/IDEは捨てない — 順番を後にするだけ。** ReactOS・純正UNIX・DOSは
   virtioを知らない。互換の相手が来たら「遅い口」として足す。
   **速さはvirtioで取り、互換はATAで取る** — 役割を分ければどちらも無理をしない
3. **rootfsの乗り物は squashfs + overlayfs + switch_root。**
   rootfsは読み専用が正しい姿 (Live CDと同じ)、書き込みはtmpfsの上の層が受ける。
   ミニinitramfsは「ディスクを見つけて移る係」に戻る — 実機Linuxの標準の役割分担
4. **中身は `Vec<u8>` で丸ごと持つ** (フロッピーの `Disk` と同じ流儀)。
   スナップショットにも丸ごと入る (RLE)。メモリに収まらない相手が来たら考える
5. **置き場所はADR-0018の通り**: リング機構は `dev/chip/virtio.rs` (素子)、
   ブロック要求の解釈と 1AF4:1001 の名乗りは `dev/card/virtio_blk.rs` (基板)。
   スロット4・I/O 0xC040・IRQ 5 — NIC (スロット3・0xC000・IRQ 3) の隣
6. **DMAはdcacheに申告する。** virtio-blkは初のバスマスタで、ゲストRAMへの
   書き込みが自己書き換え検出 (`write_phys8`) の横を通る。黙って書くと
   デコード済みの写しが腐るので、書いた範囲を `note_write_range` へ必ず知らせる
7. **要求はその刻みで完結させる。** 実機のディスクは遅いが、うちの中身は
   メモリである。シーク時間の再現は**嘘のリアリズム**で、速い方が正しい
   (時間の意味論は clock rein が別で持っている)

## 効果 (実測)

- gcc一式が **256MB → 128MB** で動く (buff/cache 3MB — 読んだ分しか載らない)
- `md5sum /dev/vda` がホスト側のmd5と一致 (読み)、200セクタの往復一致 (書き)
- ディスク無し起動は従来どおりビット同一 (挿さなければ装置が居ない —
  ADR-0017の不変条件はNICと同じ形で守られる)
- INT 13h HLE (ADR-0002で「唯一の捨て仕事」とした床) はそのまま —
  あれは16bit機のフロッピーの口で、この決定とは相手が違う

## 先送りにしたもの (台帳)

- **ATA/IDE** — ReactOS・純正UNIXが来たら。virtio-blkでブロック層の抽象
  (イメージ・スナップショット連携) が既に立っているので、足すのは口だけ
- **virtio-mmio** — PCIの無い機械に挿す口。PAEの壁 (ADR-0017) が先
- **ブラウザ側の口** (`?disk=` / ルートFS選択UIへの統合)
- **書き込みの持ち帰り** — ゲストが書いた中身はスナップショット経由でしか
  外に出ない。必要になったら dirty セクタの差分書き出しを考える
