//! 8254 PIT — プログラマブル・インターバル・タイマ。
//!
//! OSがプロセスを切り替えるには「定期的に必ず戻ってくる」仕掛けが要る。
//! これが無いと、1つのプログラムがCPUを握ったまま離さない。**プリエンプティブな
//! マルチタスクはこのチップの周期割り込みの上に立っている**。
//!
//! ## 1.193182 MHz という半端な数
//!
//! 入力クロックは 1193182 Hz。設計者が選んだ数ではなく、初代IBM PCが
//! NTSCカラーサブキャリア (3.579545 MHz) の水晶を流用し、それを3分周した
//! 残りである。テレビ用の部品が安かったからで、**40年間このままになっている**。
//!
//! カウンタ0の出力がIRQ0に繋がる。分周値に 65536 を入れると
//! 1193182 / 65536 ≒ 18.2 Hz — DOSの「1秒18.2回」の正体がこれである。
//! Linuxは 11932 を入れて 100 Hz にする。

/// 入力クロック (Hz)
pub const CLOCK_HZ: u32 = 1_193_182;

/// 読み書きの順序 (制御バイトのbit4-5)。
/// 値0の「ラッチ」はアクセス方法ではなく一度きりのコマンドなので、
/// ここではなく [`Counter::latched`] で表す
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum Access {
    LoOnly,
    HiOnly,
    /// 下位→上位の2回に分けて読み書きする (16bitを8bitポートで扱うため)
    #[default]
    LoHi,
}

#[derive(Debug, Default)]
pub struct Counter {
    /// 分周値。0は65536を意味する
    pub reload: u16,
    /// 現在値
    pub count: u16,
    pub mode: u8,
    access: Access,
    /// LoHiアクセスで上位バイトを待っているか
    writing_hi: bool,
    reading_hi: bool,
    latched: Option<u16>,
    /// リロード値が書き込まれて動き出したか
    pub running: bool,
}

impl Counter {
    fn reload_value(&self) -> u32 {
        if self.reload == 0 {
            0x1_0000
        } else {
            self.reload as u32
        }
    }

    /// クロックを `n` 進める。カウンタが0を跨いだ回数を返す (=出力パルスの数)
    fn advance(&mut self, n: u32) -> u32 {
        if !self.running {
            return 0;
        }
        let period = self.reload_value();
        let cur = if self.count == 0 { period } else { self.count as u32 };
        if n < cur {
            self.count = (cur - n) as u16;
            return 0;
        }
        let rest = n - cur;
        let pulses = 1 + rest / period;
        self.count = (period - rest % period) as u16;
        pulses
    }
}

#[derive(Debug, Default)]
pub struct Pit8254 {
    pub counters: [Counter; 3],
}

impl Pit8254 {
    pub fn new() -> Self {
        Self::default()
    }

    /// 制御ポート (0x43)
    pub fn write_control(&mut self, val: u8) {
        let sel = (val >> 6) & 3;
        if sel == 3 {
            return; // リードバックコマンド (未実装)
        }
        let c = &mut self.counters[sel as usize];
        match (val >> 4) & 3 {
            0 => {
                // ラッチ: 読み出しの瞬間に値が動かないよう写しを取る。
                // 16bitを2回に分けて読む間に桁が繰り下がると壊れるため
                c.latched = Some(c.count);
                return;
            }
            1 => c.access = Access::LoOnly,
            2 => c.access = Access::HiOnly,
            _ => c.access = Access::LoHi,
        }
        c.mode = (val >> 1) & 7;
        c.writing_hi = false;
        c.reading_hi = false;
        c.running = false;
    }

    /// カウンタポート (0x40-0x42) への書き込み
    pub fn write_counter(&mut self, idx: usize, val: u8) {
        let c = &mut self.counters[idx];
        match c.access {
            Access::LoOnly => {
                c.reload = (c.reload & 0xFF00) | val as u16;
                c.start();
            }
            Access::HiOnly => {
                c.reload = (c.reload & 0x00FF) | (val as u16) << 8;
                c.start();
            }
            _ => {
                if c.writing_hi {
                    c.reload = (c.reload & 0x00FF) | (val as u16) << 8;
                    c.writing_hi = false;
                    c.start();
                } else {
                    c.reload = (c.reload & 0xFF00) | val as u16;
                    c.writing_hi = true;
                }
            }
        }
    }

    pub fn read_counter(&mut self, idx: usize) -> u8 {
        let c = &mut self.counters[idx];
        let v = c.latched.unwrap_or(c.count);
        match c.access {
            Access::LoOnly => {
                c.latched = None;
                v as u8
            }
            Access::HiOnly => {
                c.latched = None;
                (v >> 8) as u8
            }
            _ => {
                if c.reading_hi {
                    c.reading_hi = false;
                    c.latched = None;
                    (v >> 8) as u8
                } else {
                    c.reading_hi = true;
                    v as u8
                }
            }
        }
    }

    /// クロックを `n` 進め、カウンタ0が出力したパルスの数を返す。
    /// 呼び出し側はこれをIRQ0としてPICへ渡す
    pub fn tick(&mut self, n: u32) -> u32 {
        let pulses = self.counters[0].advance(n);
        // カウンタ1 (DRAMリフレッシュ) と2 (スピーカ) も進めておく。
        // 出力は使わないが、OSが現在値を読んで時間を測ることがある
        self.counters[1].advance(n);
        self.counters[2].advance(n);
        pulses
    }

    /// カウンタ0の割り込み周波数 (Hz)。設定を確認するための補助
    pub fn irq0_hz(&self) -> f64 {
        let c = &self.counters[0];
        if !c.running {
            return 0.0;
        }
        CLOCK_HZ as f64 / c.reload_value() as f64
    }
}

impl Counter {
    fn start(&mut self) {
        self.count = self.reload;
        self.running = true;
        self.latched = None;
    }
}

impl Counter {
    fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u16(self.reload);
        w.u16(self.count);
        w.u8(self.mode);
        w.u8(self.access as u8);
        w.bool(self.writing_hi);
        w.bool(self.reading_hi);
        match self.latched {
            Some(v) => {
                w.bool(true);
                w.u16(v);
            }
            None => w.bool(false),
        }
        w.bool(self.running);
    }

    fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.reload = r.u16()?;
        self.count = r.u16()?;
        self.mode = r.u8()?;
        self.access = match r.u8()? {
            0 => Access::LoOnly,
            1 => Access::HiOnly,
            2 => Access::LoHi,
            n => return Err(format!("PITのアクセス方法が不正: {n}")),
        };
        self.writing_hi = r.bool()?;
        self.reading_hi = r.bool()?;
        self.latched = if r.bool()? { Some(r.u16()?) } else { None };
        self.running = r.bool()?;
        Ok(())
    }
}

impl Pit8254 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        for c in &self.counters {
            c.save(w);
        }
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        for c in &mut self.counters {
            c.load(r)?;
        }
        Ok(())
    }
}
