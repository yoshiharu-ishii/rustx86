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

/// フロッピーの種類 (レジスタ 0x10)。上位4bit=1台目、下位4bit=2台目
pub const FLOPPY_1440K: u8 = 4;

pub struct Cmos {
    /// 次に 0x71 で読み書きするレジスタ番号
    index: u8,
    regs: [u8; 128],
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
        Self { index: 0, regs }
    }

    /// 0x70 への書き込み: レジスタ番号の指定 (最上位ビットはNMIマスク)
    pub fn write_index(&mut self, val: u8) {
        self.index = val & 0x7F;
    }

    pub fn read_data(&self) -> u8 {
        self.regs[(self.index & 0x7F) as usize]
    }

    pub fn write_data(&mut self, val: u8) {
        self.regs[(self.index & 0x7F) as usize] = val;
    }
}
