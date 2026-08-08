//! UART 16550 — シリアルポート (COM1 = 0x3F8)。
//!
//! **Tier 2c でELKSのコンソールになる装置**である。テキストVRAMより先にこちらを
//! 作るのは、UARTが「バイトを1つ書けば出る」だけの装置で、シェルに到達するまでが
//! 最短だからである。画面は後から落ち着いて作ればよい。
//!
//! Linuxのシリアルコンソールも、組み込みの `earlyprintk` も、今なお同じ
//! レジスタ配置を使っている。1987年の部品が現役なのは、**仕様が小さくて
//! 誰も置き換える理由が無かった**からである。
//!
//! ## DLAB という発想
//!
//! 通信速度は分周値 (divisor) で決めるが、8本しかないポートに置く余裕が無かった。
//! そこでLCRのbit7 (DLAB) を立てると、**先頭2つのポートの意味が分周値に化ける**。
//! PICのICWと同じ「状態で意味を変える」節約術で、当時の設計の癖がよく出ている。

/// 送信保持レジスタが空 (Transmit Holding Register Empty)
pub const LSR_THRE: u8 = 1 << 5;
/// 送信器が完全に空
pub const LSR_TEMT: u8 = 1 << 6;
/// 受信データあり (Data Ready)
pub const LSR_DR: u8 = 1 << 0;

/// 受信データありで割り込む (IER bit0)
pub const IER_RX: u8 = 1 << 0;
/// 送信可能で割り込む (IER bit1)
pub const IER_TX: u8 = 1 << 1;

#[derive(Debug, Default)]
pub struct Uart16550 {
    /// 分周値 (DLABを立てている間に書かれる)
    pub divisor: u16,
    /// IER: どの事象で割り込むか
    pub ier: u8,
    /// LCR: 語長・パリティ・DLAB
    pub lcr: u8,
    /// MCR: モデム制御線
    pub mcr: u8,
    /// FCR: FIFOの設定 (16550から)
    pub fcr: u8,
    /// ホストへ出て行ったバイト列 (画面/端末に相当)
    pub tx: Vec<u8>,
    /// ホストから入れたバイト列 (キーボード入力に相当)
    pub rx: std::collections::VecDeque<u8>,
    /// 割り込み要求中か
    pub irq_pending: bool,
}

impl Uart16550 {
    pub fn new() -> Self {
        Self::default()
    }

    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    pub fn read(&mut self, off: u16) -> u8 {
        match off {
            0 if self.dlab() => self.divisor as u8,
            0 => {
                // RBR: 受信バッファ。読むとFIFOから1バイト取れる
                let v = self.rx.pop_front().unwrap_or(0);
                self.update_irq();
                v
            }
            1 if self.dlab() => (self.divisor >> 8) as u8,
            1 => self.ier,
            2 => {
                // IIR: 割り込み要因。読むと要因が下がる。
                // bit0が立っていると「割り込みは無い」の意味 — 論理が反転している
                let v = if self.rx.is_empty() { 0x01 } else { 0x04 };
                self.irq_pending = false;
                v
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                // LSR: 送信は常に空 (ホスト側は詰まらない) とし、
                // 受信はキューの有無で決める
                let mut v = LSR_THRE | LSR_TEMT;
                if !self.rx.is_empty() {
                    v |= LSR_DR;
                }
                v
            }
            6 => 0xB0, // MSR: CTS/DSR/DCDを立てておく (相手が居ることにする)
            _ => 0x00, // SCR: スクラッチ
        }
    }

    pub fn write(&mut self, off: u16, val: u8) {
        match off {
            0 if self.dlab() => self.divisor = (self.divisor & 0xFF00) | val as u16,
            0 => self.tx.push(val), // THR: 書いたバイトがそのまま外へ出る
            1 if self.dlab() => self.divisor = (self.divisor & 0x00FF) | (val as u16) << 8,
            1 => {
                self.ier = val;
                self.update_irq();
            }
            2 => self.fcr = val,
            3 => self.lcr = val,
            4 => self.mcr = val,
            _ => {}
        }
    }

    /// ホスト側からの入力 (キー入力など)
    pub fn feed(&mut self, bytes: &[u8]) {
        self.rx.extend(bytes.iter().copied());
        self.update_irq();
    }

    fn update_irq(&mut self) {
        self.irq_pending = self.ier & IER_RX != 0 && !self.rx.is_empty();
    }

    /// 通信速度 (bps)。分周値から逆算する。基準は 115200 bps
    pub fn baud(&self) -> u32 {
        if self.divisor == 0 {
            0
        } else {
            115_200 / self.divisor as u32
        }
    }

    /// 出力を文字列として取り出す
    pub fn tx_string(&self) -> String {
        String::from_utf8_lossy(&self.tx).into_owned()
    }
}

impl Uart16550 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u16(self.divisor);
        w.u8(self.ier);
        w.u8(self.lcr);
        w.u8(self.mcr);
        w.u8(self.fcr);
        w.bool(self.irq_pending);
        // 送信済みの内容は状態ではなく履歴なので保存しない。
        // 受信待ちの列は「まだ読まれていない入力」なので状態である
        let rx: Vec<u8> = self.rx.iter().copied().collect();
        w.bytes(&rx);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.divisor = r.u16()?;
        self.ier = r.u8()?;
        self.lcr = r.u8()?;
        self.mcr = r.u8()?;
        self.fcr = r.u8()?;
        self.irq_pending = r.bool()?;
        self.rx = r.bytes()?.into();
        Ok(())
    }
}
