---
title: "40年前の部品をRustで書く"
---

CPU が動いても OS は起動しません。OS が要求するのは**装置**です。

```
                    ┌──────────────────────┐
   PIT  ──IRQ0───▶  │                      │
   8042 ──IRQ1───▶  │  8259 PIC            │ ──▶ ベクタ番号 ──▶ CPU
   UART ──IRQ4───▶  │  (優先順位づけ)       │
                    └──────────────────────┘

   MC6845 CRTC  ──▶ カーソル位置・表示開始アドレス
   MC146818     ──▶ CMOS RTC
```

この章の装置はどれも 1980 年前後のもので、**そのまま現代の PC にも生きています**。
そして書いてみると、Rust の `enum` と `match` が当時のチップと
気味が悪いほど噛み合うことが分かります。

## 8259 PIC — 同じポートでも何番目かで意味が変わる

割り込みコントローラの初期化は、同じポートに 4 バイトを順番に書きます。
レジスタ番号を持たない代わりに、チップの中の**状態機械**が「今何番目か」を
覚えています。ポートを節約したかった時代の設計です。

Rust では素直に enum になります。

```rust:core/src/dev/pic.rs
/// 初期化コマンドの受け取り状態
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum InitState {
    /// 通常運転。0x21 への書き込みは割り込みマスク
    #[default]
    Ready,
    /// ICW1を受けた。次はICW2 (ベクタ番号のベース)
    ExpectIcw2,
    ExpectIcw3,
    ExpectIcw4,
}

/// データポート (0x21 / 0xA1) への書き込み。
/// **初期化中かどうかで意味が変わる**のがこのチップの癖である
pub fn write_data(&mut self, val: u8) {
    match self.init {
        InitState::ExpectIcw2 => {
            // ベクタのベース。下位3bitは無視される (8本単位で並ぶため)
            self.vector_base = val & 0xF8;
            self.init = InitState::ExpectIcw3;
        }
        InitState::ExpectIcw3 => {
            self.init = if self.expect_icw4 {
                InitState::ExpectIcw4
            } else {
                InitState::Ready
            };
        }
        InitState::ExpectIcw4 => self.init = InitState::Ready,
        InitState::Ready => self.imr = val,   // ← 通常運転ではマスクレジスタ
    }
}
```

優先順位の処理も面白いところです。
**処理中 (ISR) より優先度の低い線は、EOI が来るまで待たされます。**

```rust:core/src/dev/pic.rs
/// CPUへ渡せる割り込みがあれば、そのベクタ番号を返して受理する。
///
/// 優先順位は**線の番号が若いほど高い**。処理中 (ISR) より優先度の低い線は
/// 待たされる — これが「割り込みの交通整理」の中身である。
pub fn acknowledge(&mut self) -> Option<u8> {
    let pending = self.irr & !self.imr;
    if pending == 0 {
        return None;
    }
    let irq = pending.trailing_zeros() as u8;
    // 処理中により優先度の高い (=若い) 線があれば待つ
    if self.isr != 0 && self.isr.trailing_zeros() <= irq as u32 {
        return None;
    }
    self.irr &= !(1 << irq);
    self.isr |= 1 << irq;
    Some(self.vector_base.wrapping_add(irq))
}
```

`u8` のビット演算と `trailing_zeros()` だけで割り込みの交通整理が書けてしまいます。

なお 8259 が 2 個あるのは、割り込み線が 8 本しかなくて PC/AT で足りなくなり、
**2 個目を 1 個目の IRQ2 にぶら下げた**からです。IRQ2 が欠番なのはそこが
スレーブとの連結に使われているためで、「IRQ9 は IRQ2 の代わり」という
言い回しもここから来ています。

## 8254 PIT — テレビ用の水晶が40年生き残った

タイマの入力クロックは **1.193182 MHz** という中途半端な数字ですが、
これは設計値ではありません。初代 IBM PC が **NTSC カラーサブキャリアの水晶を
流用した残り** (14.31818 MHz ÷ 12) です。テレビ用の部品が安かった、
というだけの理由が 40 年生き残っています。

テストを見るのが一番早いでしょう。

```rust:core/tests/devices.rs
/// 分周値で割り込み周波数が決まる
#[test]
fn pit_divisor_sets_the_interrupt_frequency() {
    // 分周値0 = 65536 → DOSの 18.2 Hz
    let mut p = Pit8254::new();
    p.write_control(0x36);      // カウンタ0、LoHi、モード3
    p.write_counter(0, 0x00);
    p.write_counter(0, 0x00);
    assert!((p.irq0_hz() - 18.2).abs() < 0.1, "{}", p.irq0_hz());

    // Linuxの 100 Hz
    let mut p = Pit8254::new();
    p.write_control(0x36);
    let div = (CLOCK_HZ / 100) as u16;   // 11931
    p.write_counter(0, div as u8);
    p.write_counter(0, (div >> 8) as u8);
    assert!((p.irq0_hz() - 100.0).abs() < 0.1, "{}", p.irq0_hz());
}
```

「DOS の時計が 18.2 Hz」という有名な数字が `1193182 / 65536` からそのまま出てきます。

もうひとつ **ラッチ** という仕組みがあります。16bit のカウンタを 8bit のポートで
2 回に分けて読むと、読んでいる間に桁が繰り下がって壊れる。それを防ぐ写し取りです。

```rust:core/tests/devices.rs
/// 16bitの値を8bitポートで読むと、途中で桁が繰り下がって壊れる。
/// **ラッチはそれを防ぐための写し取り**である
#[test]
fn pit_latch_freezes_the_value_across_two_reads() {
    let mut p = Pit8254::new();
    p.write_control(0x36);
    p.write_counter(0, 0x00);
    p.write_counter(0, 0x10);   // 0x1000

    p.write_control(0x00);      // カウンタ0をラッチ
    let lo = p.read_counter(0);
    p.tick(0x500);              // 読んでいる間にカウンタは進む
    let hi = p.read_counter(0);
    assert_eq!(u16::from(hi) << 8 | u16::from(lo), 0x1000, "ラッチした瞬間の値");
}
```

## UART 16550 — DLAB という節約術

シリアルポートは「バイトを 1 つ書けば出る」だけの装置なので、画面より先に作りました。

面白いのは通信速度の置き場所です。8 本しかないポートに分周値を置く余裕が
なかったので、**LCR の bit7 (DLAB) を立てると先頭 2 つのポートの意味が化けます**。

```rust:core/tests/devices.rs
/// DLABを立てると**先頭2ポートの意味が分周値に化ける**。
/// 8本しかないポートに通信速度を置く余裕が無かった時代の節約術
#[test]
fn uart_dlab_repurposes_the_first_two_ports() {
    let mut u = Uart16550::new();
    u.write(3, 0x80);   // LCR: DLABを立てる
    u.write(0, 0x01);   // 分周値の下位
    u.write(1, 0x00);   // 分周値の上位
    assert_eq!(u.divisor, 1);
    assert_eq!(u.baud(), 115_200);

    u.write(3, 0x03);   // DLABを下ろす (8N1)
    u.write(0, b'Z');   // 同じポートが送信に戻る
    assert_eq!(u.tx_string(), "Z");
}
```

PIC の ICW と同じ「状態で意味を変える」節約術です。当時の設計の癖がよく出ています。

## 8042 キーボード — 割り込みはエッジで上げる

ここでバグを 1 つ踏みました。「キーを押すたびに `@` が 1 文字ずつ増える」という症状です。

奇妙なのは、**CLI 版では再現しない**こと。CLI は文字列を一気に流し込みますが、
ブラウザは 1 キーずつ送ります。この差が鍵でした。

原因は**レベルトリガとエッジトリガ**の違いです。最初の実装は
「バッファにデータがあれば IRQ1 を上げる」でした。すると、

1. キーを 1 つ押す → スキャンコードがバッファに入る → IRQ1
2. OS がハンドラでバッファを読む → 空になる
3. **次の命令でまた「データがある?」を評価 → もう一度 IRQ1**
4. OS がもう一度読みに行く → **空のバッファを読む**

そして空読みの戻り値が `0` で、OS はそれをスキャンコード 0 として解釈していました。

```rust:core/src/dev/kbd.rs
/// 今このタイミングでIRQ1を上げるべきか。**1バイトにつき1回だけ真を返す**
pub fn take_irq(&mut self) -> bool {
    if self.has_data() && !self.irq_asserted {
        self.irq_asserted = true;
        true
    } else {
        false
    }
}
```

```rust:core/src/dev/kbd.rs
// 空を読まれたら 0xFF を返す。0 は正当なスキャンコードと紛らわしい
self.keys.pop_front().unwrap_or(0xFF)
```

**「バルクでは出ず、1 つずつだと出る」という症状が、そのまま原因を指していた**のが
気持ちよかったところです。再現条件が答えそのものでした。

## MC146818 CMOS — 時計はホストではなく命令数から導く

RTC は実時間を刻む装置ですが、エミュレータでは**ホストの時計を読みません**。

```rust:core/src/dev/cmos.rs
//! ## 時計はホストではなく命令数から導く
//!
//! 実機のRTCは水晶で実時間を刻むが、ここでは**PITのクロックを数えて進める**。
//! 理由が2つある。
//!
//! - **決定的でなければスナップショットが再現しない。** 同じ状態から再開したら
//!   同じ時刻でなければ困る。ホストの時計を読むと再開のたびに違う値になる
//! - `core` は時計を持てない。`std::time::Instant` は wasm32 では動かない
```

```rust:core/src/dev/cmos.rs
/// PITのクロックを `n` 進め、1秒貯まったら時計を進める
pub fn tick(&mut self, n: u32) {
    self.sub_second += n;
    while self.sub_second >= super::pit::CLOCK_HZ {
        self.sub_second -= super::pit::CLOCK_HZ;
        self.now.advance_second();
    }
}
```

読み出しは、保存された値ではなく**今の時刻から組み立てて**返します。

```rust:core/src/dev/cmos.rs
pub fn read_data(&self) -> u8 {
    let t = &self.now;
    match self.index & 0x7F {
        0x00 => to_bcd(t.sec),
        0x02 => to_bcd(t.min),
        0x04 => to_bcd(t.hour),
        0x06 => to_bcd(t.weekday),
        0x07 => to_bcd(t.day),
        0x08 => to_bcd(t.month),
        0x09 => to_bcd((t.year % 100) as u8),
        0x32 => to_bcd((t.year / 100) as u8),
        i => self.regs[i as usize],
    }
}
```

## 装置を進める場所

装置は毎命令進めると重いので、**カウントダウン 1 本**で間引きます。

```rust:core/src/lib.rs
fn tick_devices(&mut self) {
    if self.devices.pit.tick(PIT_CLOCKS_PER_TICK) > 0 {
        self.devices.pic[0].raise(IRQ_TIMER);
    }
    // 時計もPITと同じクロックで進める。**ここで進めるのが要点**で、
    // INT 08h の中で進めるとOSが自前のハンドラを入れた瞬間に時計が止まる
    self.devices.cmos.tick(PIT_CLOCKS_PER_TICK);
    if self.devices.uart.irq_pending {
        self.devices.pic[0].raise(IRQ_COM1);
    }
    // キーボードは割り込み駆動。**1バイトにつき1回だけ**挙手する
    if self.devices.keyboard.take_irq() {
        self.devices.pic[0].raise(IRQ_KEYBOARD);
    }
    if self.pending_irq.is_none() {
        self.pending_irq = self.devices.pic[0].acknowledge();
    }
}
```

コメントにある「INT 08h の中で進めると OS が自前のハンドラを入れた瞬間に
止まる」は、実際に踏んで学んだことです。**OS はタイマ割り込みを乗っ取ります。**

## 保存と復元は各装置が自分で持つ

スナップショット (状態をまるごと保存して後から戻す) を作るとき、
**保存の形式を知っているのは各装置だけでよい**という形にしました。

```rust:core/src/dev/pic.rs
impl Pic8259 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u8(self.irr);
        w.u8(self.isr);
        w.u8(self.imr);
        w.u8(self.vector_base);
        w.bool(self.expect_icw4);
        w.u8(self.init as u8);
        w.bool(self.read_isr);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.irr = r.u8()?;
        // ...
        self.init = match r.u8()? {
            0 => InitState::Ready,
            1 => InitState::ExpectIcw2,
            2 => InitState::ExpectIcw3,
            3 => InitState::ExpectIcw4,
            n => return Err(format!("PICの初期化状態が不正: {n}")),
        };
        self.read_isr = r.bool()?;
        Ok(())
    }
}
```

**PIC のマスクの形式を知っているのは PIC だけでよい。** 中央に大きな
シリアライザを置かないことで、装置を足すときに触る場所が 1 箇所で済みます。
