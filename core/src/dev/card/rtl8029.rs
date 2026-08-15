//! RTL8029AS — **PCIの基板に載ったDP8390** (Realtek 10EC:8029)。
//!
//! 中身のレジスタはISAのNE2000と同じ素子なので、実体は
//! [`Dp8390`](crate::dev::chip::Dp8390) をそのまま使う。**この基板の都合は2つだけ**:
//!
//! - **PROMが平坦**。ISAの8bit経路では各バイトが2度ずつ並ぶが、PCIのドライバ
//!   (ne2k-pci) は連続バイトをそのままMACとして読む。倍幅のまま渡すと
//!   `52:54:00:…` が `52:52:54:…` に化ける (実際に化けた)
//! - **設定空間で名乗る** (身元・BAR・割り込み線)。番地はfirmwareかOSが配るので、
//!   ISAのように定数で決め打ちしない
//!
//! 実物のRTL8029ASも「NE2000互換であること」を売りにした廉価チップで、
//! Linuxのドライバ (ne2k-pci) もISA版 (ne) とコアの `lib8390.c` を共有している。
//! **皮だけ替わるのが正しい姿**である。

use crate::bus::pci::{Bar, PciFunction};
use crate::dev::chip::Dp8390;

/// PCIのRTL8029ASカードを1枚組み立てる。**PROMは平坦**
pub fn build(mac: [u8; 6]) -> Dp8390 {
    let mut nic = Dp8390::new(mac);
    let mut prom = [0u8; 32];
    prom[..6].copy_from_slice(&mac);
    // 'W' の印はPCI版では 14/15 に移る (QEMUのne2000も同じ使い分けをする)
    prom[14] = 0x57;
    prom[15] = 0x57;
    nic.write_prom(&prom);
    nic
}

/// NICが挿さるスロット。**位置は固定でよい** — 実機でも挿した場所は
/// 動かないし、決まっていれば装置への振り分けが `match` で書ける
pub const NET_SLOT: usize = 3;

/// RTL8029のI/O窓の番地 (firmwareが配ったことにする値)。
/// Linuxは既に割り当て済みなら尊重するので、BIOSを書かずに済む
pub const NET_IO_BASE: u32 = 0xC000;

/// RTL8029ASの設定空間の顔 (名乗り) を作る。`irq_line` はOSに知らせる割り込み線
pub fn pci_function(irq_line: u8) -> PciFunction {
    // class 02 = ネットワーク、subclass 00 = Ethernet
    PciFunction::new(0x10EC, 0x8029, 0x02, 0x00, 0x00)
        .with_bar(0, Bar { size: 32, io: true }, NET_IO_BASE)
        .with_irq(irq_line, 1) // INTA#
        // サブシステムIDも自分自身を指す (実物のRTL8029ASと同じ)。
        // 空 (0000:0000) だと一部のドライバが「素性不明」と判断する
        .with_subsystem(0x10EC, 0x8029)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::pci::reg;

    /// RTL8029ASの名乗り — **ne2k-pci ドライバはこの値でカードを選ぶ**。
    /// 1つでも違うとLinuxはNICを見つけられない
    #[test]
    fn identity_is_realtek_rtl8029() {
        let f = pci_function(3);
        assert_eq!(
            u16::from_le_bytes([f.config()[reg::VENDOR_ID], f.config()[reg::VENDOR_ID + 1]]),
            0x10EC,
            "Realtek"
        );
        assert_eq!(
            u16::from_le_bytes([f.config()[reg::DEVICE_ID], f.config()[reg::DEVICE_ID + 1]]),
            0x8029,
            "RTL8029AS"
        );
        assert_eq!(
            f.config()[reg::CLASS_CODE + 2],
            0x02,
            "クラス: ネットワーク"
        );
        assert_eq!(
            f.config()[reg::CLASS_CODE + 1],
            0x00,
            "サブクラス: Ethernet"
        );
        // サブシステムも自分自身を指す (空だと素性不明と見るドライバがある)
        assert_eq!(
            u16::from_le_bytes([
                f.config()[reg::SUBSYS_VENDOR],
                f.config()[reg::SUBSYS_VENDOR + 1]
            ]),
            0x10EC
        );
    }

    /// DP8390のレジスタは32バイトに収まる。**幅を間違えるとOSが配る番地が
    /// 重なる**ので、窓の大きさは装置の側の責任
    #[test]
    fn io_window_is_thirty_two_bytes_at_the_firmware_address() {
        let f = pci_function(3);
        assert_eq!(f.bar_base(0), NET_IO_BASE, "firmwareが配ったことにする番地");
        assert_eq!(f.io_hit(NET_IO_BASE as u16), None, "許可前は名乗らない");
    }

    /// 割り込み線はfirmwareが配る。**INTA# (pin=1) を名乗る**
    #[test]
    fn interrupt_line_is_what_firmware_assigned() {
        let f = pci_function(3);
        assert_eq!(f.config()[reg::INTERRUPT_LINE], 3);
        assert_eq!(f.config()[reg::INTERRUPT_PIN], 1, "INTA#");
    }
}
