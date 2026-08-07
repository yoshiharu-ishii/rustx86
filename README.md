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
- [x] **Tier 1b: 命令網羅とco-sim検証** — Unicorn Engineをオラクルにした比較実行。
      シフト/回転 (GRP2)、MUL/DIV/NEG/NOT (GRP3)、INC/DEC/PUSH/CALL/JMP (GRP4/5)、
      TEST/XCHG/LEA/CBW/CWD/SAHF/LAHF/PUSHF/POPF、十進補正 (DAA/DAS/AAA/AAS/AAM/AAD)、
      ストリング命令 (MOVS/CMPS/STOS/LODS/SCAS、REP対応) を追加。
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
- **Unicorn co-sim** (`cosim/`): Unicorn Engine (QEMUのCPU部) をオラクルに、
  同じ初期状態を両方に与えて1命令実行し、レジスタ・フラグ・メモリを突き合わせる。
  命令テンプレート + 境界値混じりのランダム生成で、EFLAGS意味論 (特にAF/OF/PF) を
  機械的に潰す。x86には網羅テストROMが存在しないため、これが主要な検証手段になる
- 未実装オペコードは即panic (静かに壊れない方針)

```bash
cargo test                    # ブートセクタE2E (高速)
cargo test -p rustx86-cosim   # co-sim (Unicornのビルドに数分かかる)
```

### co-simは「バグを検出できること」を確認済み

緑のテストは、それ自体では検証能力を証明しない。意図的にバグを注入して
検出されるかを確かめている (変異テスト)。

ADCのAFフラグ計算からキャリー加算を落とすと、即座に検出された:

```
[ADC r/m8,r8] code=[10, d4] regs=[8000, 19ff, ...] flags_in=CF|PF
  FLAGS: ours=PF|SF oracle=PF|AF|SF (差分 AF)
```

一方、DAAの境界値を `0x99` から `0x9A` にずらす変異は、ランダム生成では
3000ケース流しても**検出できなかった**。ALがちょうど 0x9A になるケースを
踏み損ねるためである。十進補正命令はALとCF/AFだけで分岐が決まり状態空間が
小さい (256 x 3 x 4) ので、総当たりに切り替えた。今は一発で捕まる:

```
co-sim mismatch [DAA] code=[27] AX=009a flags_in=-
  AX: ours=00a0 oracle=0000
```

**状態空間が小さいならランダムより総当たり**、という使い分けが要る。

## コード構成

`cpu/mod.rs` は**振り分け表**に徹し、実際の計算は用途ごとのモジュールが持つ。
命令ごとにファイルを分けるやり方は採らない。x86は命令数が数百あり、
ALUグリッドのように48命令が1ハンドラで処理される構造を壊してしまうため。
matchが1箇所に集まっていること自体が「この命令はどこで処理されるか」を
即座に答えてくれる価値になる。

| ファイル | 役割 | 行数 |
|---|---|---|
| `cpu/mod.rs` | オペコードの振り分け表 (巨大なmatch) | 502 |
| `cpu/operand.rs` | ModRM解決、オペランド読み書き、アドレス変換、スタック | 116 |
| `cpu/alu.rs` | 8種の演算とフラグ計算 (AF/OFの意味論) | 96 |
| `cpu/shift.rs` | シフトと回転 (8/16bit共通) | 94 |
| `cpu/string.rs` | ストリング命令とREPループ | 119 |
| `cpu/decimal.rs` | 十進補正 (BCD) | 73 |

## 設計メモ

- レジスタは最初からu32で保持 (386拡張を見据える)。リアルモードは16bitビューで操作
- x86のオペコードグリッド (規則的な部分) はrustboyで確立した「ビットで畳む」方式で処理し、
  歴史的な不規則部分だけ個別実装する
