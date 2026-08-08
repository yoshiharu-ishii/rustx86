---
title: "正解を知っているCPUに答え合わせさせる"
---

## x86 には「これを流せば分かる」テストが無い

ゲームボーイには Blargg のテスト ROM という定番があり、流せば CPU の正しさが
ほぼ判定できます。**x86 には相当するものがありません。**

前章で見たとおり、EFLAGS の意味論 — 特に AF と OF — は境界値でしか姿を現しません。
`ADD 0x0F, 0x01` で AF が立ち、`0x0E, 0x01` では立たない。
この手のものを手書きテストで網羅するのは非現実的です。

そこで **Unicorn Engine** (QEMU の CPU 部分をライブラリにしたもの) を
オラクル、つまり「正解を知っている装置」として使います。

**同じ初期状態を自作 CPU と Unicorn の両方に与えて 1 命令だけ実行し、
実行後の全状態を突き合わせる。**食い違えばこちらのバグです。

## ハーネス

1 ケース分の初期状態と、実行後に観測する状態を型で持ちます。

```rust:cosim/src/lib.rs
/// 1ケース分の初期状態
#[derive(Clone, Debug)]
pub struct TestCase {
    pub code: Vec<u8>,
    pub regs: [u16; 8],
    /// ES CS SS DS。CS/SS/DSは0固定 (コード・スタック・データの配置を単純に保つ)。
    /// ESだけは自由に振れるので PUSH ES / ストリング命令の宛先を検証できる
    pub sregs: [u16; 4],
    pub flags: u16,
    /// DATA_ADDR に置く16バイト
    pub data: [u8; 16],
    /// STACK_BASE に置く32バイト。POP系の入力を意味のある値にする
    pub stack: [u8; STACK_WINDOW],
}

/// 実行後の観測状態
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct State {
    pub regs: [u16; 8],
    pub sregs: [u16; 4],
    pub flags: u16,
    pub ip: u16,
    pub data: [u8; 16],
    pub stack: [u8; STACK_WINDOW],
}
```

自作 CPU 側の実行は素直です。

```rust:cosim/src/lib.rs
/// 自作CPUで1命令実行する
pub fn run_ours(tc: &TestCase) -> State {
    let mut m = Machine::new();
    for (i, b) in tc.code.iter().enumerate() {
        m.write8(CODE_ADDR as u32 + i as u32, *b);
    }
    // ... data / stack も同様に置く
    m.cpu.regs[..8].copy_from_slice(&tc.regs.map(|v| v as u32));
    m.cpu.sregs[..4].copy_from_slice(&tc.sregs);
    m.cpu.flags = tc.flags as u32 | 0x0002;
    m.cpu.set_cs_ip(0, CODE_ADDR);

    m.step();   // ← 1命令だけ

    State {
        regs: std::array::from_fn(|i| m.cpu.regs[i] as u16),
        sregs: std::array::from_fn(|i| m.cpu.sregs[i]),
        flags: m.cpu.flags as u16 & FLAG_MASK_ALL,
        ip: m.cpu.ip,
        data, stack,
    }
}
```

**スタックの観測窓**を持っているのが少しだけ工夫した点です。
`PUSH` / `POP` / `CALL far` は結果がレジスタではなくメモリに出るので、
そこを見ないと検証になりません。

## ケースはテンプレートから生成する

命令ごとに「どんなバイト列を作るか」をテンプレートとして持ち、乱数で埋めます。
ALU グリッドはマクロで一気に並べられます。

```rust:cosim/tests/alu.rs
let templates: Vec<Template> = alu_templates![
    "ADD" => 0x00u8,
    "OR"  => 0x08u8,
    "ADC" => 0x10u8,
    "SBB" => 0x18u8,
    "AND" => 0x20u8,
    "SUB" => 0x28u8,
    "XOR" => 0x30u8,
    "CMP" => 0x38u8,
];
check(&templates, 200, 0xC0DE_1234);   // 各テンプレート200ケース、シード固定
```

乱数値は一様に振りません。`interesting_u8()` は `0x00 0x01 0x7F 0x80 0xFF` といった
**境界値を優先的に出します**。AF や OF は境界でしか姿を現さないからです。

もうひとつ大事なのが `undefined` フィールドです。

```rust:cosim/src/lib.rs
/// 比較対象のフラグ (x86が「未定義」と定めるものは呼び出し側でマスクする)
pub const FLAG_MASK_ALL: u16 = (cpu::CF | cpu::PF | cpu::AF | cpu::ZF | cpu::SF | cpu::OF) as u16;
```

x86 には「このフラグは未定義」と仕様が明言している箇所があります
(`MUL` 後の SF など)。そこは Unicorn と一致しなくて構いません。
**どこを比較しないかを明示的に持つ**のが、この手のオラクル比較の勘所だと思います。

## 緑のテストは、それ自体では何も証明しない

テストが全部通ったとして、それは「バグが無い」ことを意味しません。
**「このテストはバグを見つけられるのか」を確かめる必要があります。**

そこで意図的にバグを注入します (変異テスト)。ADC の AF 計算からキャリー加算を
落としてみると、即座に検出されました。

```
[ADC r/m8,r8] code=[10, d4] regs=[8000, 19ff, ...] flags_in=CF|PF
  FLAGS: ours=PF|SF oracle=PF|AF|SF (差分 AF)
```

一方、DAA の境界値を `0x99` から `0x9A` にずらす変異は、ランダム生成では
**3000 ケース流しても検出できませんでした**。AL がちょうど `0x9A` になるケースを
踏み損ねるためです。

十進補正命令は AL と CF/AF だけで分岐が決まり、状態空間が小さい (256 × 3 × 4)。
そこで**総当たり**に切り替えました。今は一発で捕まります。

```
co-sim mismatch [DAA] code=[27] AX=009a flags_in=-
  AX: ours=00a0 oracle=0000
```

**状態空間が小さいならランダムより総当たり。** この使い分けが要ります。

## 実例: 仕様書の疑似コードが間違っていた

この仕組みが釣り上げた一番の大物が `ENTER` でした。スタックフレームを作る命令で、
Intel SDM には疑似コードがこう書いてあります。

```
BP ← FrameTemp;
SP ← BP − Size;      ← ここ
```

素直に実装したら、co-sim が差分を報告してきました。しかも `level > 0` のときだけ。

```
[ENTER imm16,level] code=[c8, 02, 00, 02] regs=[..., 2ff8, 2ff8, ...]
  SP: ours=2ff4 oracle=2ff0
```

`ENTER` は `level` の数だけ「display」と呼ばれる親フレームへのポインタを積みます。
SDM の式は**その分 (level×2 バイト) を勘定に入れていません**。
`level = 0` のときだけたまたま正しくなる式でした。

AMD のマニュアルと QEMU の実装は**現在の SP から引いて**おり、実挙動はそちらです。

```rust:core/src/cpu/mod.rs
0xC8 => {   // ENTER imm16, imm8
    // ... display を level 個積む ...
    push16(m, frame);
    m.cpu.set_reg16(BP, frame);
    // 最後のSP調整は「今のSP」から引く。Intel SDMの疑似コードは
    // `SP <- BP - Size` と書いているが、これが正しいのは level=0 のときだけで、
    // level>0 では display を積んだ分 (level*2バイト) が抜け落ちる。
    // AMDのマニュアルとQEMUの実装は現在のSPから引いており、そちらが実挙動。
    // co-simがこの差を捕まえた
    let sp = m.cpu.reg16(SP).wrapping_sub(size);
    m.cpu.set_reg16(SP, sp);
}
```

**仕様書を読んで実装し、仕様書どおりに動くことをテストしていたら、
永久に見つからないバグでした。**

疑似コードを写経した実装は、読んだ本人には正しく見えます。
**仕様書を疑うきっかけは、動くオラクルとの突き合わせでしか得られません。**

## co-sim の限界

co-sim は 1 命令単位なので、**装置の状態遷移も割り込みの受付タイミングも
検証できません**。ここから先は別の手段が要ります。

その答えが「実 OS を動かすこと自体をテストにする」でした。

```rust:core/tests/elks.rs
/// カーネルがルートをマウントし、loginプロンプトまで到達する
#[test]
fn elks_boots_to_login_prompt() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "login:", 100_000_000), "loginプロンプトに到達せず");
    let screen = m.text_screen_string();
    assert!(screen.contains("ELKS 0.9.1"), "バージョン表示が無い:\n{screen}");
    assert!(
        screen.contains("Mounted root device"),
        "ルートがマウントされていない:\n{screen}"
    );
}
```

検証手段が階段状に切り替わります。**命令は co-sim、機構は実 OS。**
