//! RTL8029AS — **PCIの皮を被ったNE2000** (Realtek 10EC:8029)。
//!
//! ## 実体はここに無い
//!
//! 中身のレジスタはISA版と同じDP8390なので、装置の実体は
//! [`crate::dev::isa::ne2000`] をそのまま使う。**このファイルが作るのは
//! 設定空間の顔 (身元・BAR・割り込み線) だけ**である。
//!
//! 実装を2つ持たないのはこのリポジトリの原則 (意味論の原本は1つ) だが、
//! ここでは歴史の方が先にそう作っている。実物のRTL8029ASは「NE2000互換で
//! あること」を売りにした廉価チップで、Linuxのドライバ (ne2k-pci) もISA版
//! (ne) とコアの `lib8390.c` を共有している。**皮だけ替わるのが正しい姿**で、
//! 中身まで写すとエミュレータの方が実機より複雑になってしまう。
//!
//! 唯一実体側に影響するのがPROMの並べ方で、ISAの8bit経路では各バイトが
//! 2度ずつ並ぶのに対し、PCI版は連続バイトで読む
//! ([`Ne2000::flatten_prom`](crate::dev::isa::ne2000::Ne2000::flatten_prom))。
//! 倍幅のまま渡すとMACが `52:52:54:…` に化ける (実際に化けた)。

use super::{Bar, PciFunction};

/// NICが挿さるスロット。**位置は固定でよい** — 実機でも挿した場所は
/// 動かないし、決まっていれば装置への振り分けが `match` で書ける
pub const NET_SLOT: usize = 3;

/// RTL8029のI/O窓の番地 (firmwareが配ったことにする値)。
/// Linuxは既に割り当て済みなら尊重するので、BIOSを書かずに済む
pub const NET_IO_BASE: u32 = 0xC000;

/// RTL8029ASの設定空間の顔を作る。`irq_line` はOSに知らせる割り込み線
pub fn rtl8029(irq_line: u8) -> PciFunction {
    // class 02 = ネットワーク、subclass 00 = Ethernet
    PciFunction::new(0x10EC, 0x8029, 0x02, 0x00, 0x00)
        .with_bar(0, Bar { size: 32, io: true }, NET_IO_BASE)
        .with_irq(irq_line, 1) // INTA#
        // サブシステムIDも自分自身を指す (実物のRTL8029ASと同じ)。
        // 空 (0000:0000) だと一部のドライバが「素性不明」と判断する
        .with_subsystem(0x10EC, 0x8029)
}
