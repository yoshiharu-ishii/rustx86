//! 8259 PIC — 割り込みコントローラ。
//!
//! CPUは「今キーが押された」を知る手段を持たない。装置が非同期に手を挙げる
//! 仕組みが割り込みで、その挙手を束ねて**優先順位をつける**のがこのチップである。
//!
//! ## なぜ2個あるのか
//!
//! 8259は割り込み線を8本しか持たない。初代IBM PCはそれで足りたが、PC/ATで
//! 足りなくなり、**2個目を1個目のIRQ2にぶら下げた**。以来ずっとこの形で、
//! IRQ2が欠番なのはそこがスレーブとの連結に使われているためである。
//! 「IRQ9はIRQ2の代わり」という言い回しもここから来ている。
//!
//! ## 初期化がやたら長い理由
//!
//! OSは起動時にICW1〜ICW4という4バイトを順番に書き込む。これは**同じポートに
//! 順番に書くことで意味が変わる**方式で、レジスタ番号を持たない代わりに
//! 内部状態機械で何番目かを覚えている。ポート数を節約したかった時代の設計である。
//!
//! ICW2で「このPICのIRQ0番は割り込みベクタ何番か」を決める。BIOSはIRQ0を
//! ベクタ0x08に置くが、**プロテクトモードではCPUの例外番号と衝突する**ため、
//! Linuxは起動時にここを0x20へ付け替える。同じチップの同じレジスタが、
//! 32bit化の障害物としてもう一度顔を出す。

/// 初期化コマンドの受け取り状態
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum InitState {
    /// 通常運転。0x21 への書き込みは割り込みマスク
    #[default]
    Ready,
    /// ICW1を受けた。次はICW2 (ベクタ番号のベース)
    ExpectIcw2,
    /// 次はICW3 (カスケード接続の設定)
    ExpectIcw3,
    /// 次はICW4 (動作モード)
    ExpectIcw4,
}

#[derive(Debug, Default)]
pub struct Pic8259 {
    /// IRR: 挙手中の割り込み (Interrupt Request Register)
    pub irr: u8,
    /// ISR: 処理中の割り込み (In-Service Register)。EOIが来るまで下りない
    pub isr: u8,
    /// IMR: マスク。立っているビットは無視する (Interrupt Mask Register)
    pub imr: u8,
    /// ICW2で設定される割り込みベクタのベース
    pub vector_base: u8,
    /// ICW4を要求するか (ICW1のbit0)
    expect_icw4: bool,
    init: InitState,
    /// 読み出しでISRを返すか (OCW3で切り替える)。falseならIRR
    read_isr: bool,
}

impl Pic8259 {
    pub fn new() -> Self {
        Self {
            imr: 0xFF, // 初期状態は全マスク。OSが必要な線だけ開ける
            ..Default::default()
        }
    }

    /// コマンドポート (0x20 / 0xA0) への書き込み
    pub fn write_command(&mut self, val: u8) {
        if val & 0x10 != 0 {
            // ICW1: 初期化開始。これを書かれると状態が全部リセットされる
            self.expect_icw4 = val & 0x01 != 0;
            self.init = InitState::ExpectIcw2;
            self.isr = 0;
            self.irr = 0;
            self.read_isr = false;
            return;
        }
        if val & 0x08 != 0 {
            // OCW3: 読み出し対象の切り替え
            if val & 0x02 != 0 {
                self.read_isr = val & 0x01 != 0;
            }
            return;
        }
        // OCW2: 主にEOI (End Of Interrupt)
        if val & 0x20 != 0 {
            if val & 0x40 != 0 {
                // 特定EOI: 指定された線だけ下ろす
                self.isr &= !(1 << (val & 7));
            } else {
                // 非特定EOI: 処理中のうち最も優先度が高い線を下ろす。
                // ハンドラは「どれを処理したか」を言わずに済む
                if self.isr != 0 {
                    self.isr &= !(1 << self.isr.trailing_zeros());
                }
            }
        }
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
            InitState::Ready => self.imr = val,
        }
    }

    pub fn read_command(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }

    pub fn read_data(&self) -> u8 {
        self.imr
    }

    /// ICW2 で決めたベクタの先頭 (連結の判定に使う)
    pub fn vector_base(&self) -> u8 {
        self.vector_base
    }

    /// 装置が手を挙げる
    pub fn raise(&mut self, irq: u8) {
        self.irr |= 1 << (irq & 7);
    }

    /// CPUへ渡せる割り込みがあれば、そのベクタ番号を返して受理する。
    ///
    /// 優先順位は**線の番号が若いほど高い**。処理中 (ISR) より優先度の低い線は
    /// 待たされる — これが「割り込みの交通整理」の中身である。
    /// 未処理の (マスクされていない) 要求があるか。ベクタはまだ決めない
    pub fn has_pending(&self) -> bool {
        self.irr & !self.imr != 0
    }

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
}

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
        self.isr = r.u8()?;
        self.imr = r.u8()?;
        self.vector_base = r.u8()?;
        self.expect_icw4 = r.bool()?;
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
