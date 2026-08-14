# ADR-0017: ネットワークの境界とバス順 — ISA NE2000 から始める

- 状態: **決定** (2026-08-13 ユーザー決定)
- 日付: 2026-08-13
- 関連: [ADR-0011](0011-tier-redraw-after-compat.md) (Tier引き直し)、
  [ADR-0016](0016-platform-cfg.md) (ターゲット別の裁き方)、
  [roadmap Tier 5](../roadmap.md)、SLiRP backendは [wsslirp](https://github.com/yoshiharu-ishii/wsslirp)

## 背景

SLiRP backend (wsslirp、Go) は完成した — WS越しにDHCP・DNS・TCP (ハーフクローズ込み)・
外向きICMPまで実インターネットで検証済み。境界プロトコルは
**「1 WSバイナリメッセージ = 1 Ethernetフレーム」だけ**。残っていたのは
こちら側、ゲストの仮想NICである。

旧計画 (roadmap 5b) は virtio-net 先行だった。偵察で崩れた:

- **Alpine lts (現行カーネル)**: virtioは全て `=m`、かつ
  `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` 無効 — カーネル引数でデバイスを教える
  口が無く、virtio-mmioの発見にはACPIかPCIが要る。**ltsのままvirtio-mmioは不可能**
- **Alpine virt フレーバー**: `VIRTIO_MMIO=y` + `CMDLINE_DEVICES=y` で理想的だが、
  実際に起動させると `vector 0x0e is beyond IDT limit 0x0000` — **`CONFIG_X86_PAE=y`**
  で、PAEページング (3段・8バイトPTE) の実装が先に立つ
- **自前カーネル** (非PAE + virtio組み込み): Linuxには効くが、**カーネルを
  差し替えられないゲスト (ReactOS・DOS) には無力** — 装置を「本物」に
  寄せる方向と逆を向く

そこでバスの順番を決め直した。決め手はユーザーの一言:
**「16bitでpingを飛ばしてみたい。PCIで16bitはありえない」**。

## 決定

1. **ISAから行く。装置は NE2000 (DP8390)。** 16bitゲスト (ELKS・FreeDOS) の
   ネットワークドライバはISA NE2000が本流で、これが最短で「16bitからping」に届く。
   FreeDOSは NE2000.COM パケットドライバ + mTCP、ELKSは ne2k ドライバ + ktcp
2. **8390コアは書き捨てにならない。** Alpine lts には `NE2K_PCI=m` (RTL8029 =
   PCI版NE2000) がある — 次のPCI段で同じ8390コアをPCIの皮で包めば、
   **カーネル無変更のLinuxにも同じ装置で届く**。virtio-netに行く前に
   Tier 5 (Linuxからping) をこの道で通せる
3. **ディレクトリはハードウェアバスごとに分ける。** `core/src/dev/` は
   `dev/isa/` (既存の PIC/PIT/UART/KBD/CMOS/CRTC + NE2000) と、将来の
   `dev/pci/` に分割する。decode_ioの定数`match`はISAの流儀のまま —
   動的な振り分けはPCIが来たときにPCI側で持つ
4. **coreの境界はフレームのやり取りだけ。** wsslirpの `FrameIO` と同じ思想で、
   coreには「フレームを出す/受け取る」trait (送受キュー) だけを置く。
   WebSocket・再接続・時計といった非決定的I/Oは全部外側 (native: examples層 /
   wasm: JSシェル層) が持つ。**NICを繋がない起動のビット同一は不変条件** —
   CI の OS起動回帰がそのまま門番になる。フレーム注入はシリアル入力と同じく
   スライス境界のAPI経由で行い、coreは壁時計を知らない
5. **coreに手を入れるPRは、機能開発でも速度を交互A/B 5周で実測**して
   PR本文に書く (単発は熱の運。物差しはネイティブbootphase + `ab.sh` 5周)
6. **台帳 (棄却ではない、必要になったら取り出す)**:
   - virtio-net/virtio-blk — microVM段 (Tier 8) の道具。そこでは自前カーネル
     (Firecrackerの標準作法) とセットで再訪
   - PAEページング — x86_64 (Tier 9) のロングモード実装時にどうせ作る。前払いしない
   - Alpine virt フレーバー — PAEが入ったら選択肢に戻る
   - e1000 / rtl8139 — ReactOS・DOS向けの「枯れたPCI NIC」。PCIバスが
     できてから、ゲストの要求で選ぶ

## 検証計画

- 単体: NE2000のレジスタ・リングバッファ・DMAポートをテストで固める
  (送受のゴールデンフレーム)
- 結合: wsslirpをローカルに立て、FreeDOS + NE2000.COM + mTCP で
  `ping 1.1.1.1` → wsslirpのICMPプロキシ経由で実応答。ELKSはktcpで同経路
- 回帰: NIC無し3OS起動がビット同一 (既存CI)。速度はA/B 5周 (決定5)

## 効果

- 「ブラウザの中のFreeDOSから本物のインターネットへpingが返る」が最初の絵になる
- ISA→PCIの順は歴史の順そのもの — 「現代PCの成り立ちを辿る教材」の筋も通る
