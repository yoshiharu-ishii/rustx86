---
title: "命令表は、実は格子だった"
---

## この本について

Rust でゼロから x86 エミュレータを書き、WebAssembly でブラウザに載せて、
16bit UNIX と FreeDOS を起動するまでの記録です。

引用しているコードはすべて実際に動いているものです。リポジトリはこちら:
https://github.com/yoshiharu-ishii/rustx86

構成はワークスペース4つに分かれています。

```
rustx86/
├── core/     ← エミュレータ本体 (CPU・装置・BIOS)。std だけ、依存なし
├── cosim/    ← Unicorn Engine と突き合わせる検証ハーネス
├── wasm/     ← wasm-bindgen の薄いラッパー
└── web/      ← ブラウザ側 (端末とフレームループ)
```

`core` は**外部クレートにまったく依存していません**。教材として書いているので、
読んだときに「この行が何をしているか」がクレートの向こう側へ消えないようにしています。

```toml
[package]
name = "rustx86-core"
version = "0.1.0"
edition = "2021"

[dependencies]
```

## x86 の命令表は格子になっている

最初に書くのは命令デコーダです。x86 は「巨大で不規則」と言われますが、
実際に表を眺めると**ビットを折り畳むと格子になっている**部分がかなりあります。

たとえば ALU 演算。`0x00` から `0x3D` までの 48 個はこう分解できます。

```
  76543210
  --kkk-ff
    │   └── 形式 3bit (r/m8,r8 / r/m16,r16 / r8,r/m8 / ... / AX,imm16)
    └────── 演算種別 3bit (ADD OR ADC SBB AND SUB XOR CMP)
```

**8 種の演算 × 6 種の形式**。48 命令を個別に書く必要はなく、1 つのハンドラで済みます。

```rust:core/src/cpu/mod.rs
match op {
    // --- ALUグリッド: 0x00-0x3D (演算3bit x 形式3bit) ---
    0x00..=0x3F if op & 7 <= 5 && (op & 0x27) != 0x26 && (op & 0x27) != 0x27 => {
        let kind = (op >> 3) & 7;          // ← 演算種別を取り出す
        match op & 7 {                      // ← 形式で分岐
            0 => {
                // r/m8, r8
                let (reg, rm) = modrm(m, &d);
                let a = read_op8(m, &rm);
                let b = m.cpu.reg8(reg);
                let r = alu8(&mut m.cpu, kind, a, b);
                if kind != 7 { write_op8(m, &rm, r); }   // 7 = CMP は書き戻さない
            }
            1 => {
                // r/m16,r16 または r/m32,r32 (`0x66` が付いていれば後者)
                let (reg, rm) = modrm(m, &d);
                let w = d.opsize32;
                let a = read_op_w(m, &rm, w);
                let b = m.cpu.reg_w(reg, w);
                let r = alu_w(&mut m.cpu, kind, a, b, w);
                if kind != 7 { write_op_w(m, &rm, r, w); }
            }
            // ... 2..=5 も同じ調子
        }
    }
```

`kind != 7` の 1 行が `CMP` の「結果を捨てる」を表現しています。
`CMP` は `SUB` とまったく同じ計算をしてフラグだけ残す命令なので、
格子の 8 番目の席に座っているのは偶然ではありません。

同じ折り畳みは他にもあります。

| 命令 | ビットの意味 |
|---|---|
| `IN` / `OUT` | bit0 = 幅、bit1 = 方向、bit3 = ポートの出どころ (即値/DX) |
| セグメントの `PUSH`/`POP` | `(op >> 3) & 3` がそのまま ES/CS/SS/DS |
| `Jcc` (`0x70`-`0x7F`) | 下位3bitが条件、bit0 が反転 |

**40 年前の設計者がビット単位でケチっていた痕跡**が、そのまま実装の短さになって返ってきます。

条件分岐の判定もこの構造をそのまま使えます。

```rust:core/src/cpu/alu.rs
pub fn condition(c: &Cpu, cc: u8) -> bool {
    let r = match cc >> 1 {
        0 => c.flag(OF),
        1 => c.flag(CF),
        2 => c.flag(ZF),
        3 => c.flag(CF) || c.flag(ZF),
        4 => c.flag(SF),
        5 => c.flag(PF),
        6 => c.flag(SF) != c.flag(OF),
        _ => c.flag(ZF) || (c.flag(SF) != c.flag(OF)),
    };
    if cc & 1 != 0 { !r } else { r }   // bit0 が立っていれば否定
}
```

16 種類の条件分岐が、これだけで書けます。

## フラグ計算がバグの主産地

計算そのものは `alu.rs` に分けています。x86 のフラグで厄介なのは
**AF (下位 4bit からの桁上がり)** と **OF (符号付きオーバーフロー)** で、
どちらも境界値でしか姿を現しません。

タプルの並びは `(結果, CF, OF, AF)` です。

```rust:core/src/cpu/alu.rs
pub fn alu8(c: &mut Cpu, op: u8, a: u8, b: u8) -> u8 {
    let carry = c.flag(CF) as u16;
    let (r, cf, of, af) = match op {
        0 => {
            let r = a as u16 + b as u16;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) > 0xF)
        }
        1 => ((a | b) as u16, false, false, false),
        2 => {
            let r = a as u16 + b as u16 + carry;
            (r, r > 0xFF, ((a ^ !b) & (a ^ r as u8)) & 0x80 != 0, (a & 0xF) + (b & 0xF) + carry as u8 > 0xF)
        }
        // ...
        _ => ((a ^ b) as u16, false, false, false),    // XOR
    };
    let r8 = r as u8;
    c.set_flag(CF, cf);
    c.set_flag(OF, of);
    c.set_flag(AF, af);
    set_szp8(c, r8);
    if op == 7 { a } else { r8 }   // CMP は結果を書き戻さない
}
```

OF の `((a ^ !b) & (a ^ r)) & 0x80` は「両オペランドの符号が同じで、
結果だけ符号が違う」を 1 行にしたものです。美しいのですが、
**手で書いたテストでこれが全境界で正しいと確信するのは無理**でした。

そこで別の手を使います。次章の話です。

## 振り分け表は1箇所に集める

`cpu/mod.rs` は**振り分け表に徹する**設計にしています。オペコードを読んで
どの処理へ渡すかまでが仕事で、実際の計算は用途ごとのモジュールが持ちます。

```
cpu/mod.rs      オペコードの振り分け表 (巨大な match)
cpu/operand.rs  ModRM解決、オペランド読み書き、アドレス変換、スタック
cpu/alu.rs      8種の演算とフラグ計算
cpu/shift.rs    シフトと回転
cpu/string.rs   ストリング命令とREP
cpu/decimal.rs  十進補正 (BCD)
```

**命令ごとにファイルを分けるやり方は採っていません。** x86 は命令が数百あり、
ALU グリッドのように 48 命令が 1 ハンドラで処理される構造を壊してしまうからです。
`match` が 1 箇所に集まっていること自体が「この命令はどこで処理されるか」に
即答してくれる価値になります。

未実装のオペコードは**即 panic** させます。

```rust:core/src/cpu/mod.rs
_ => panic!(
    "unimplemented opcode {op:#04x} at {:04x}:{:04x}",
    m.cpu.sregs[CS], start_ip
),
```

黙って 0 を返すと、遥か後方で意味不明な暴走として現れます。
panic なら**名前を教えてくれる**ので、埋めるのが安く済みます。
この方針は実 OS を動かす段階でとても効きました。
