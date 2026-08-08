---
title: "BIOSを自分で書く — 死んだ役割と、育った役割"
---

装置が揃っても、まだ OS は起動しません。**BIOS が要ります。**

ただし本物の ROM は載せません。目的は「BIOS が何をしているか」を自分で書くことなので、
ブラックボックスを増やしたくないからです。必要なサービスだけを Rust の関数として
肩代わりします (HLE / 高位エミュレーション)。

## 1サイクルの全体像

`Machine::step()` がすべてです。この 5 ステップしかありません。

```rust:core/src/lib.rs
pub fn step(&mut self) {
    // 1. 保留中のハードウェア割り込み (IFが立っているときだけ)
    if self.pending_irq.is_some() && self.cpu.flag(cpu::IF) {
        let vec = self.pending_irq.take().unwrap();
        self.halted = false;
        cpu::interrupt(self, vec);
        return;
    }
    // 2. 装置を進める (カウントダウン方式)
    self.tick_countdown -= 1;
    if self.tick_countdown == 0 {
        self.tick_countdown = INSTRUCTIONS_PER_TICK;
        self.tick_devices();
    }
    if self.halted { return; }

    // 3. BIOS HLE の入口
    if self.cpu.sregs[cpu::CS] == BIOS_SEG {
        let vec = self.cpu.ip as u8;
        if self.bios_interrupt(vec) {
            self.return_flags_to_caller();
            cpu::iret(self);
        }
        return;
    }
    // 4. 命令実行 (TFは実行前の値)
    let tf = self.cpu.flag(cpu::TF);
    cpu::step(self);

    // 5. トラップフラグ
    if tf && !self.halted {
        cpu::interrupt(self, 1);
    }
}
```

BIOS の呼び出しは「`CS` が特殊なセグメント値だったら Rust の関数へ振り分ける」形です。
IVT に `BIOS_SEG:ベクタ番号` を並べておけば、`INT 10h` は `BIOS_SEG:0x10` へ飛び、
そこで捕まえられます。

`bios_interrupt()` が **`bool` を返している**のが工夫した点です。
「まだ IRET するな」を表現しています。

```rust:core/src/bios.rs
0x00 | 0x10 => match self.take_key() {
    Some(v) => self.cpu.regs[cpu::AX] = v as u32,
    None => {
        // **待つなら割り込みを開ける。**
        //
        // `INT` 命令はIFを落とす (x86の仕様)。落としたまま待つと
        // キーボード割り込みが永久に来ず、待っているものが
        // 二度と届かない。実BIOSの待ちループに `STI` があるのは
        // このためで、ここでも同じことをする
        self.cpu.set_flag(cpu::IF, true);
        return false;
    }
},
```

`false` を返すと `IP` も `CS` も動かないので、次の `step()` でまた同じ入口に来ます。
**エミュレータ側にブロッキング呼び出しを持ち込まずに、ゲストから見た
ブロッキングを実現できます。**

コメントの `STI` の話は実際に踏んだバグです。`INT` 命令は IF を落とす —
x86 の仕様どおりの正しい挙動です。だから**待つ側が自分で開けなければ、
待っているものは二度と来ません。**実 BIOS の待ちループに `STI` があるのは
飾りではありませんでした。

## フラグを呼び出し元へ返す

もうひとつ落とし穴がありました。BIOS サービスは成否を **CF** で返しますが、
`IRET` はスタックに積まれた FLAGS で上書きしてしまいます。

```rust:core/src/lib.rs
/// BIOSサービスが返した成否をフラグとして呼び出し元へ届ける。
/// **`IRET` はスタックに積まれたFLAGSで上書きしてしまう。**
fn return_flags_to_caller(&mut self) {
    let sp = self.cpu.regs[cpu::SP] as u16;
    let addr = cpu::operand::linear(self.cpu.sregs[cpu::SS], sp.wrapping_add(4));
    let stacked = self.read16(addr);
    let keep = (cpu::CF | cpu::ZF) as u16;
    self.write16(addr, (stacked & !keep) | (self.cpu.flags as u16 & keep));
}
```

積まれた FLAGS を直接書き換えてから IRET します。実機の BIOS も同じことをしています。

## 電源投入時にやること

```rust:core/src/bios.rs
pub(crate) fn power_on_self_test(&mut self) {
    self.install_bios_rom_id();
    self.install_bios_vectors();
    self.install_bios_data_area();
    self.install_pic_defaults();
    self.install_pit_defaults();
}
```

この 5 行が、この章でいちばん伝えたいところです。**サービスは 1 つも含まれていません。**

## BIOS には役割が2つあって、死んだのは片方だけ

「OS は起動したらハードを直接叩く。ではなぜ BIOS があるのか」という問いに、
実装しながら答えが出ました。

**死んだ役割 = サービス提供者。** `INT 13h` でディスクを読み、`INT 10h` で字を出す。
32bit 以降の OS はこれを使いません。16bit で再入不可で遅く、自前のドライバに敵わない
からです。UEFI の `ExitBootServices()` は「もう自分でやるからサービスは要らない」を
**明示的に宣言する関数**で、この引き渡しを設計として認めたものです。

**生きている役割 = ハードウェアの立ち上げと、マシンの説明書。** こちらは OS には
代われません。電源投入直後は DRAM すら使えず、PCI にはアドレスも振られていない。
OS は RAM が無ければロードすらできないので、原理的に代われないのです。

そして面白いことに、**実 OS を起動させるまでに踏んだ穴は全部「死なない側」でした。**

| 詰まった箇所 | どちらの役割か |
|---|---|
| A20ゲートが開かない | ハードウェアの立ち上げ |
| BIOSデータエリアが空 | マシンの説明書 |
| PICのvector_baseが0 | ハードウェアの立ち上げ |
| PITが未設定 | ハードウェアの立ち上げ |
| IRQ0/1 がマスクされたまま | ハードウェアの立ち上げ |
| ROMの機種コードが無い | マシンの説明書 |
| カーソル位置がBDAに無い | マシンの説明書 |

ELKS は 8042 も VRAM も直接叩く OS で、BIOS サービスをほとんど呼びません。
それでも**立ち上げと説明書が欠けているだけで何度も死にました**。

### 例: PICのvector_base

一番印象的だったのがこれです。ELKS が `panic: DIVIDE FAULT` で止まりました。
ゼロ除算です。真っ先に疑うのは自作の `DIV` ですが、こういう診断を仕込んでありました。

```rust:core/src/cpu/mod.rs
fn divide_error(m: &mut Machine, start_ip: u16) {
    m.cpu.ip = start_ip;   // フォールトなので失敗した命令の先頭
    if m.first_fault.is_none() {
        m.first_fault = Some((0, m.cpu.sregs[CS], start_ip));
    }
    interrupt(m, 0);
}
```

見てみると `first_fault` は `None`。**つまり自作の `DIV` は一度も例外を出していない。**
ベクタ 0 のハンドラは呼ばれているのに、呼んだのは `DIV` ではありませんでした。

犯人は PIC の `vector_base` でした。実 BIOS は起動時に PIC を初期化して IRQ0 を
ベクタ 0x08 に置きます。それをやっていなかったので `vector_base` が 0 のまま。
**タイマ割り込みが、ベクタ 0 = ゼロ除算例外として CPU に届いていました。**

```rust:core/src/bios.rs
/// マスタをベクタ 0x08-0x0F、スレーブを 0x70-0x77 に置くのがPC/ATの決まりである。
///
/// なお 0x08 はプロテクトモードではCPUの例外番号 (#DF) と衝突する。
/// Linuxが起動時にわざわざ 0x20 へ付け替えるのはこのためで、
/// この衝突は Tier 3 でもう一度顔を出す。
fn install_pic_defaults(&mut self) {
    for (i, base, icw3) in [(0usize, 0x08u8, 0x04u8), (1, 0x70, 0x02)] {
        let p = &mut self.devices.pic[i];
        p.write_command(0x11); // ICW1: 初期化開始 + ICW4あり
        p.write_data(base);    // ICW2: ベクタのベース
        p.write_data(icw3);    // ICW3: カスケードの結線
        p.write_data(0x01);    // ICW4: 8086モード
        // **タイマ(0)・キーボード(1)・スレーブ連結(2)は開けておく。**
        p.write_data(if i == 0 { 0xF8 } else { 0xFF });
    }
}
```

### 例: PITが未設定

DOS を載せたときに出たのがこれです。実 BIOS は POST でタイマを 18.2 Hz に設定します。
ELKS は自分で設定するので気づきませんでしたが、**DOS は BIOS が設定済みである
ことを前提**にしています。

```rust:core/src/bios.rs
fn install_pit_defaults(&mut self) {
    let pit = &mut self.devices.pit;
    pit.write_control(0x36); // カウンタ0、LoHi、モード3 (方形波)
    pit.write_counter(0, 0x00);
    pit.write_counter(0, 0x00); // 分周値0 = 65536 → 18.2 Hz
    pit.write_control(0x54); // カウンタ1、LoOnly、モード2 (レート生成)
    pit.write_counter(1, 18); // DRAMリフレッシュ
}
```

時計が進まないだけでなく、**「HLT して待つ」形の待ち合わせが永久に目を覚まさない**。

## 誰が割り込みを持っているか

「BIOS を直したのに効かない」に何度もぶつかったので、診断を足しました。

```
--- 割り込みベクタの持ち主: 0x08=0070:000f(ゲスト) 0x09=0070:0016(ゲスト)
                            0x10=f000:0010(BIOS)   0x16=f000:0016(BIOS) ---
```

**FreeDOS は INT 08h と 09h を自分のものにしています。** つまり FreeDOS 下では
こちらのキーボード割り込み処理は一度も走らない。直しても効かないわけです。

**OS がベクタを乗っ取るとはどういうことか**が、数字で見えるようになりました。

## 未実装は黙って0を返さず、即panic

```rust:core/src/bios.rs
_ => panic!(
    "INT {n:#04x} AH={ah:#04x} 未実装 (CS:IP={:04x}:{:04x})",
    self.cpu.sregs[cpu::CS],
    self.cpu.ip
),
```

これは実 OS を動かす段階で決定的に効きました。詰まるたびに
`INT 10h AH=0x13 未実装` と**名前**が出るので、次にやることが 1 行で分かります。

黙って 0 を返すと、遥か後方で意味不明な暴走として現れます。
**穴が有限だと分かっている相手に対しては、これが最も速い進め方でした。**
