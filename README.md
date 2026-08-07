# rustx86

Rust製のx86エミュレータ。リアルモード8086から始めて、プロテクトモード、
ページングと歴史の地層を順に登る。

## ゴール (2026-08-07 決定)

**32bit Linuxがブラウザで起動し、busyboxシェルが操作できること (Tier 2)。**

x86_64 (ロングモード) への拡張はゴールに含めない。Tier 2到達後に、
やる価値があると判断したら改めて検討する。

## ロードマップ

- [x] **Tier 1a: リアルモードの骨格** — ブートセクタの Hello, World が動く
  - ALUグリッド (8演算×6形式 = 48命令を1ハンドラで処理)
  - ModRM 16bitアドレッシング、セグメントオーバーライド
  - BIOSは実装せず INT 10h テレタイプ出力をHLEフック
- [ ] **Tier 1b: 命令網羅とco-sim検証** — Unicorn Engineをオラクルにした比較実行で
      EFLAGS意味論を潰す。シフト/回転、MUL/DIV、残りのMOV系を追加。
      **ここまで到達したらブログ記事化する (図解付き)**
- [ ] **Tier 2a: プロテクトモード** — GDT、セグメントディスクリプタ、CR0、リング
- [ ] **Tier 2b: ページング** — CR3、2段ページテーブル、TLB
- [ ] **Tier 2c: Linuxブートプロトコル** — BIOSは作らず bzImage + initrd を直接ロードして
      32bitエントリへ (QEMUの `-kernel` 方式)
- [ ] **Tier 2d: 最小デバイス一式** — UART 16550、8254タイマー、8259 PIC、virtio-blk
- [ ] **Tier 2e: ブラウザ化** — WASM + xterm.js でbusyboxシェルが叩ける状態にする

## 実行

```bash
# テスト
cargo test

# ブートセクタ実行
cargo run --example run -- asm/hello.bin

# ブートセクタのビルド (要nasm)
nasm -f bin -o asm/hello.bin asm/hello.asm
```

## 検証戦略

- ブートセクタ実プログラムによるE2Eテスト (`core/tests/`)
- Unicorn Engine (QEMUのCPU部) をオラクルにした比較実行 (`cosim/`、構築中)。
  ランダム命令列を両方で実行してレジスタ・フラグを突き合わせ、
  EFLAGSの意味論をfuzzingで検証する
- 未実装オペコードは即panic (静かに壊れない方針)

## 設計メモ

- レジスタは最初からu32で保持 (386拡張を見据える)。リアルモードは16bitビューで操作
- x86のオペコードグリッド (規則的な部分) はrustboyで確立した「ビットで畳む」方式で処理し、
  歴史的な不規則部分だけ個別実装する
