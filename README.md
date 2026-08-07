# rustx86

Rust製のx86エミュレータ。リアルモード8086から始めて、プロテクトモード、
ページング、ロングモード (x86_64) まで歴史の地層を順に登ることを目指す。
最終目標はブラウザ (WASM) でのLinux起動。

## 現在地

- [x] Tier 1着手: リアルモード8086のデコーダ骨格
  - ALUグリッド (8演算×6形式 = 48命令を1ハンドラで処理)
  - ModRM 16bitアドレッシング、セグメントオーバーライド
  - BIOSは実装せず INT 10h テレタイプ出力をHLEフック
  - ブートセクタの Hello, World が動く
- [ ] Tier 1完成: 命令網羅 + Unicorn co-simでの検証
- [ ] Tier 2: プロテクトモード + ページング + Linuxブートプロトコル → busybox
- [ ] Tier 3: ロングモード + x86_64 + SSE2サブセット

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
