//! ホストブリッジ — Intel 440FX (8086:1237)。
//!
//! CPUバスとPCIバスの間に立つチップで、実機では**メモリコントローラを兼ねる
//! 主役**である。エミュレータでの仕事は「スロット0に居て、設定空間から
//! 見えること」だけ — OSはここを読んで「このマシンにPCIバスが在る」と判断する。
//!
//! 440FXを名乗るのはQEMUの既定と同じ選択で、Linuxが素直に認識する系統だから。
//! チップセット固有の細工 (メモリ穴の設定など) はどのOSも要求してこないので
//! 実装していない。**要求されたら足す** — 名乗った分だけ実装する原則は
//! CPUIDと同じである。

use super::PciFunction;

/// 440FX相当のホストブリッジ。**スロット0に居るのが慣例**で、
/// OSはここを見てPCIバスの存在を確かめる
pub fn host_bridge() -> PciFunction {
    // 8086:1237 = Intel 440FX。class 06 = ブリッジ、subclass 00 = ホスト
    PciFunction::new(0x8086, 0x1237, 0x06, 0x00, 0x00)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::pci::reg;

    /// 440FXの名乗り。**OSはスロット0のここを見てPCIバスの存在を確かめる**ので、
    /// 値が変わると「PCIの無いマシン」に見える
    #[test]
    fn identity_is_intel_440fx() {
        let f = host_bridge();
        assert_eq!(
            u16::from_le_bytes([f.cfg[reg::VENDOR_ID], f.cfg[reg::VENDOR_ID + 1]]),
            0x8086,
            "Intel"
        );
        assert_eq!(
            u16::from_le_bytes([f.cfg[reg::DEVICE_ID], f.cfg[reg::DEVICE_ID + 1]]),
            0x1237,
            "440FX"
        );
        assert_eq!(f.cfg[reg::CLASS_CODE + 2], 0x06, "クラス: ブリッジ");
        assert_eq!(f.cfg[reg::CLASS_CODE + 1], 0x00, "サブクラス: ホスト");
    }
}
