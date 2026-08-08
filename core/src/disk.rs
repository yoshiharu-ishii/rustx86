//! ディスクの高位エミュレーション (BIOS INT 13h)。
//!
//! フロッピーディスクコントローラを回路として再現するのは数千行の仕事だが、
//! `INT 13h` を「イメージの該当セクタをメモリにコピーする関数」として実装すれば
//! 数十行で済む。ELKSはBIOS経由のディスクドライバを持つのでこれで足りる
//! ([ADR-0002](../../../docs/adr/0002-devices-and-16bit-unix.md))。
//!
//! Linuxに向かうときは virtio-blk に置き換わるので、**唯一の捨て仕事だが安い**。
//!
//! ## CHSという住所の付け方
//!
//! セクタの位置を「何番目」ではなく **シリンダ・ヘッド・セクタ**の3つ組で
//! 指定する。円盤が何枚あって、各面に磁気ヘッドがあって、1周が何個に
//! 区切られているか — 物理構造がそのままアドレスになっている。
//!
//! セクタ番号だけ**1から始まる**のは、当時のBIOS作者がそう決めたからで、
//! 理由は無い。off-by-oneの温床として40年生き残った。

/// 1セクタのバイト数。PCでは事実上 512 に固定
pub const SECTOR_SIZE: usize = 512;

pub struct Disk {
    pub data: Vec<u8>,
    pub cylinders: u16,
    pub heads: u8,
    /// 1トラックあたりのセクタ数
    pub sectors: u8,
}

impl Disk {
    /// イメージのサイズから形状を推測する。
    ///
    /// 実機ではフォーマット時に決まる情報だが、標準的なフロッピーは
    /// サイズと形状が1対1に対応しているので逆引きできる
    pub fn from_image(data: Vec<u8>) -> Result<Self, String> {
        let (cylinders, heads, sectors) = match data.len() {
            368_640 => (40, 2, 9),    // 360KB 5.25"
            737_280 => (80, 2, 9),    // 720KB 3.5"
            1_228_800 => (80, 2, 15), // 1.2MB 5.25"
            1_474_560 => (80, 2, 18), // 1.44MB 3.5"
            2_949_120 => (80, 2, 36), // 2.88MB 3.5"
            n => return Err(format!("形状の分からないイメージサイズ: {n} バイト")),
        };
        Ok(Self {
            data,
            cylinders,
            heads,
            sectors,
        })
    }

    /// CHS を先頭からの通し番号 (LBA) に直す
    pub fn chs_to_lba(&self, c: u16, h: u8, s: u8) -> Option<usize> {
        if s == 0 || s > self.sectors || h >= self.heads || c >= self.cylinders {
            return None;
        }
        Some(
            ((c as usize * self.heads as usize + h as usize) * self.sectors as usize)
                + (s as usize - 1),
        )
    }

    pub fn read_sector(&self, lba: usize) -> Option<&[u8]> {
        self.data.get(lba * SECTOR_SIZE..(lba + 1) * SECTOR_SIZE)
    }

    pub fn write_sector(&mut self, lba: usize, data: &[u8]) -> bool {
        match self
            .data
            .get_mut(lba * SECTOR_SIZE..(lba + 1) * SECTOR_SIZE)
        {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    pub fn total_sectors(&self) -> usize {
        self.data.len() / SECTOR_SIZE
    }
}
