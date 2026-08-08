//! MC6845系 CRTC (0x3D4 / 0x3D5) — 画面の制御。
//!
//! 文字そのものはVRAMに置かれるが、**カーソルがどこにあるか**と
//! **VRAMのどこから表示するか**はここが持つ。だから画面を正しく描くには
//! VRAMだけでは足りない。
//!
//! ポートが2本しかないので、0x3D4 にレジスタ番号を書いてから 0x3D5 で
//! 読み書きする (PICのICW、UARTのDLAB、CMOSと同じ間接指定)。
//! 1980年前後のチップはどれもこの形で、ポート数が高価だった時代の作法である。
//!
//! アドレスが 0x3D4 (カラー) と 0x3B4 (モノクロ) の2つあるのは、
//! MDAとCGAを同じ機械に挿せるようにしたため。BIOSデータエリアの 0x463 に
//! 「どちらを使うか」が入っており、OSはそれを読んでから話しかける。

/// カーソル位置 上位バイト
pub const REG_CURSOR_HI: u8 = 0x0E;
/// カーソル位置 下位バイト
pub const REG_CURSOR_LO: u8 = 0x0F;
/// 表示開始アドレス 上位/下位
pub const REG_START_HI: u8 = 0x0C;
pub const REG_START_LO: u8 = 0x0D;

#[derive(Debug, Default)]
pub struct Crtc {
    index: u8,
    regs: [u8; 32],
}

impl Crtc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_index(&mut self, val: u8) {
        self.index = val & 0x1F;
    }

    pub fn write_data(&mut self, val: u8) {
        self.regs[self.index as usize] = val;
    }

    pub fn read_data(&self) -> u8 {
        self.regs[self.index as usize]
    }

    /// カーソルの位置 (画面先頭からの文字数)
    pub fn cursor_offset(&self) -> u16 {
        (self.regs[REG_CURSOR_HI as usize] as u16) << 8 | self.regs[REG_CURSOR_LO as usize] as u16
    }

    /// 表示を開始するVRAM上の位置 (文字単位)。
    /// ここを動かすとメモリを触らずにスクロールできる (ハードウェアスクロール)
    pub fn start_offset(&self) -> u16 {
        (self.regs[REG_START_HI as usize] as u16) << 8 | self.regs[REG_START_LO as usize] as u16
    }
}

impl Crtc {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u8(self.index);
        w.bytes(&self.regs);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.index = r.u8()?;
        let regs = r.bytes()?;
        if regs.len() != self.regs.len() {
            return Err("CRTCのレジスタ数が合わない".into());
        }
        self.regs.copy_from_slice(&regs);
        Ok(())
    }
}
