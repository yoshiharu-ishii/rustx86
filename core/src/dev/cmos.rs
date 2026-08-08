//! MC146818 CMOS RTC (0x70 / 0x71)。
//!
//! 電池でバックアップされた小さなRAMで、時計と**マシンの構成情報**を持つ。
//! BIOS設定画面で変えた内容が残るのはここである。
//!
//! ポートが2本しか無いので、0x70 にレジスタ番号を書いてから 0x71 で読み書きする
//! (PICのICWやUARTのDLABと同じ、ポート節約のための間接指定)。
//!
//! 0x70 の最上位ビットはNMIのマスクという**全く無関係な機能**が同居している。
//! 空きビットがあったから使われた、という以上の理由は無い。
//!
//! ELKSはここからフロッピーの種類を読む。応答しないと「unknown」と判断され、
//! 後続の計算が壊れる。
//!
//! ## 時計はホストではなく命令数から導く
//!
//! 実機のRTCは水晶で実時間を刻むが、ここでは**PITのクロックを数えて進める**。
//! 理由が2つある。
//!
//! - **決定的でなければスナップショットが再現しない。** 同じ状態から再開したら
//!   同じ時刻でなければ困る。ホストの時計を読むと再開のたびに違う値になる
//! - `core` は時計を持てない。`std::time::Instant` は wasm32 では動かない
//!
//! 起動時刻は固定にしてある。PITは64命令ごとに1クロック進むので、
//! 1秒 = 1,193,182クロック = 約7,600万命令。手元の実測 (90 MIPS) では
//! **偶然ほぼ実時間と同じ速さ**で進む。

/// フロッピーの種類 (レジスタ 0x10)。上位4bit=1台目、下位4bit=2台目
pub const FLOPPY_1440K: u8 = 4;

/// 起動時の日時 (固定)。**ホストの時計は読まない** — 上記の理由による
const EPOCH: Time = Time {
    year: 2026,
    month: 1,
    day: 1,
    weekday: 5,
    hour: 0,
    min: 0,
    sec: 0,
};

/// 2進で持つ日時。CMOSのレジスタとして読まれるときにBCDへ直す
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Time {
    year: u16,
    month: u8,
    day: u8,
    /// 1 = 日曜 (MC146818の流儀)
    weekday: u8,
    hour: u8,
    min: u8,
    sec: u8,
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
    }
}

impl Time {
    /// 1秒進める。桁上がりを素直に書き下す
    fn advance_second(&mut self) {
        self.sec += 1;
        if self.sec < 60 {
            return;
        }
        self.sec = 0;
        self.min += 1;
        if self.min < 60 {
            return;
        }
        self.min = 0;
        self.hour += 1;
        if self.hour < 24 {
            return;
        }
        self.hour = 0;
        self.weekday = self.weekday % 7 + 1;
        self.day += 1;
        if self.day <= days_in_month(self.year, self.month) {
            return;
        }
        self.day = 1;
        self.month += 1;
        if self.month <= 12 {
            return;
        }
        self.month = 1;
        self.year += 1;
    }
}

/// 2進 → BCD。CMOSは既定でBCD表記なので、読まれるときにここを通す
pub fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

/// BCD → 2進。ゲストが時刻を設定してくるときに通す
pub fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0F)
}

pub struct Cmos {
    /// 次に 0x71 で読み書きするレジスタ番号
    index: u8,
    regs: [u8; 128],
    /// 現在時刻 (2進)
    now: Time,
    /// 1秒に満たない端数 (PITクロック)
    sub_second: u32,
}

impl Default for Cmos {
    fn default() -> Self {
        Self::new()
    }
}

impl Cmos {
    pub fn new() -> Self {
        let mut regs = [0u8; 128];
        regs[0x0A] = 0x26; // ステータスA: 分周設定 (更新中フラグは立てない)
        regs[0x0B] = 0x02; // ステータスB: 24時間表記、BCD
        regs[0x0D] = 0x80; // ステータスD: 電池は生きている
                           // 1台目を1.44MB、2台目は無し
        regs[0x10] = FLOPPY_1440K << 4;
        regs[0x14] = 0x21; // 装置構成
        regs[0x15] = 640u16 as u8; // ベースメモリ (KB)
        regs[0x16] = (640u16 >> 8) as u8;
        Self {
            index: 0,
            regs,
            now: EPOCH,
            sub_second: 0,
        }
    }

    /// 0x70 への書き込み: レジスタ番号の指定 (最上位ビットはNMIマスク)
    pub fn write_index(&mut self, val: u8) {
        self.index = val & 0x7F;
    }

    /// 時刻のレジスタは保存された値ではなく**今の時刻から組み立てて**返す。
    /// それ以外は素通し
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

    pub fn write_data(&mut self, val: u8) {
        self.regs[(self.index & 0x7F) as usize] = val;
    }

    /// PITのクロックを `n` 進め、1秒貯まったら時計を進める。
    /// **実時間ではなく命令数から導いている** (モジュールの説明を参照)
    pub fn tick(&mut self, n: u32) {
        self.sub_second += n;
        while self.sub_second >= super::pit::CLOCK_HZ {
            self.sub_second -= super::pit::CLOCK_HZ;
            self.now.advance_second();
        }
    }

    /// 現在時刻 (時, 分, 秒)。BIOSの INT 1Ah AH=02 が使う
    pub fn time_bcd(&self) -> (u8, u8, u8) {
        (
            to_bcd(self.now.hour),
            to_bcd(self.now.min),
            to_bcd(self.now.sec),
        )
    }

    /// 現在の日付 (世紀, 年, 月, 日)。BIOSの INT 1Ah AH=04 が使う
    pub fn date_bcd(&self) -> (u8, u8, u8, u8) {
        (
            to_bcd((self.now.year / 100) as u8),
            to_bcd((self.now.year % 100) as u8),
            to_bcd(self.now.month),
            to_bcd(self.now.day),
        )
    }

    /// 時刻を設定する (INT 1Ah AH=03 / DOSの `TIME` コマンド)。
    /// 端数は捨てる — 秒を設定した瞬間が秒の頭になる
    pub fn set_time_bcd(&mut self, hour: u8, min: u8, sec: u8) {
        self.now.hour = from_bcd(hour).min(23);
        self.now.min = from_bcd(min).min(59);
        self.now.sec = from_bcd(sec).min(59);
        self.sub_second = 0;
    }

    /// 日付を設定する (INT 1Ah AH=05 / DOSの `DATE` コマンド)
    pub fn set_date_bcd(&mut self, century: u8, year: u8, month: u8, day: u8) {
        self.now.year = from_bcd(century) as u16 * 100 + from_bcd(year) as u16;
        self.now.month = from_bcd(month).clamp(1, 12);
        self.now.day = from_bcd(day).clamp(1, days_in_month(self.now.year, self.now.month));
    }
}

impl Cmos {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u8(self.index);
        w.bytes(&self.regs);
        // 時計も状態である。戻したときに時刻が飛ぶとゲストのタイムアウトが壊れる
        w.u16(self.now.year);
        w.u8(self.now.month);
        w.u8(self.now.day);
        w.u8(self.now.weekday);
        w.u8(self.now.hour);
        w.u8(self.now.min);
        w.u8(self.now.sec);
        w.u32(self.sub_second);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.index = r.u8()?;
        let regs = r.bytes()?;
        if regs.len() != self.regs.len() {
            return Err("CMOSのレジスタ数が合わない".into());
        }
        self.regs.copy_from_slice(&regs);
        self.now = Time {
            year: r.u16()?,
            month: r.u8()?,
            day: r.u8()?,
            weekday: r.u8()?,
            hour: r.u8()?,
            min: r.u8()?,
            sec: r.u8()?,
        };
        self.sub_second = r.u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_conversion() {
        assert_eq!(to_bcd(0), 0x00);
        assert_eq!(to_bcd(9), 0x09);
        assert_eq!(to_bcd(10), 0x10);
        assert_eq!(to_bcd(59), 0x59);
        assert_eq!(to_bcd(23), 0x23);
    }

    /// 秒→分→時→日→月→年の桁上がりが全部繋がっていること。
    /// 大晦日の23:59:59を1秒進めると年が変わる
    #[test]
    fn new_year_rolls_over_every_field() {
        let mut t = Time {
            year: 2026,
            month: 12,
            day: 31,
            weekday: 5,
            hour: 23,
            min: 59,
            sec: 59,
        };
        t.advance_second();
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.min, t.sec),
            (2027, 1, 1, 0, 0, 0)
        );
        assert_eq!(t.weekday, 6, "曜日も回る");
    }

    /// うるう年の2月29日が存在し、平年には存在しないこと
    #[test]
    fn leap_year_february() {
        assert_eq!(days_in_month(2024, 2), 29, "4で割れる");
        assert_eq!(days_in_month(2026, 2), 28, "平年");
        assert_eq!(days_in_month(2100, 2), 28, "100で割れるが400で割れない");
        assert_eq!(days_in_month(2000, 2), 29, "400で割れる");
    }

    /// ゲストが設定した時刻が、そのまま読み戻せること
    #[test]
    fn guest_can_set_the_clock() {
        let mut c = Cmos::new();
        c.set_time_bcd(0x13, 0x45, 0x30); // 13:45:30
        assert_eq!(c.time_bcd(), (0x13, 0x45, 0x30));
        c.set_date_bcd(0x19, 0x81, 0x08, 0x12); // 1981-08-12
        assert_eq!(c.date_bcd(), (0x19, 0x81, 0x08, 0x12));
    }

    /// PITのクロックを1秒ぶん流すと、時計がちょうど1秒進むこと
    #[test]
    fn ticks_advance_the_clock_by_one_second() {
        let mut c = Cmos::new();
        c.write_index(0x00);
        assert_eq!(c.read_data(), to_bcd(0));
        c.tick(crate::dev::pit::CLOCK_HZ - 1);
        assert_eq!(c.read_data(), to_bcd(0), "1クロック足りない");
        c.tick(1);
        assert_eq!(c.read_data(), to_bcd(1), "ちょうど1秒");
    }
}
