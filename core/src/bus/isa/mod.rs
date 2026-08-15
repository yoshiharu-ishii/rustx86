//! ISA — **番地が定数で焼かれている流儀**と、その配線。
//!
//! ISAには装置を列挙する仕組みが無い。「VGAのテキスト画面は 0xB8000」
//! 「COM1 は 0x3F8」と決め打ちで全メーカーが合わせていたので、デコーダは
//! `match` で足りる。装置を数える仕組みが要るのは [`pci`](super::pci) からである。
//!
//! ## 「ISAの装置」ではない
//!
//! ここに番地を持つ 8259・8254・8042・MC146818・MC6845・16550 は、
//! **マザーボードに半田付けされたチップ**でありISAカードではない (どれもISAの
//! 標準化より前の部品である)。スロットに挿さるのは NE2000 くらいのもので、
//! ここでISAと呼んでいるのは**その固定番地の流儀**の呼び名にすぎない。
//! 素子そのものは [`dev::chip`](crate::dev::chip) に居る。
//!
//! ## 配線もここにある
//!
//! どの装置がどの割り込み線に繋がっているかは、装置の性質ではなく**機械の配線**
//! である (実機ならジャンパで決めるところ)。だから素子側ではなくバス側が持つ。
//! PCIの INTA# → IRQ のルーティングと同じ棚に並ぶ

use super::IoTarget;

/// IRQ0 (PIT) の割り込み線
pub const IRQ_TIMER: u8 = 0;
/// IRQ1 (キーボード) の割り込み線
pub const IRQ_KEYBOARD: u8 = 1;
/// IRQ4 (COM1) の割り込み線
pub const IRQ_COM1: u8 = 4;
/// NE2000の定番IRQ。DOSのパケットドライバの既定値に合わせる
pub const IRQ_NET: u8 = 3;

/// NE2000カードの窓の先頭 (実機ならジャンパで選ぶ。0x300はISA NICの定番)
pub const NET_BASE: u16 = 0x300;
/// 同 末尾 (DP8390のレジスタは32バイトに収まる)
pub const NET_LAST: u16 = NET_BASE + 0x1F;

/// 固定番地の表。名乗り手が居なければ None (呼ぶ側がPCI側へ回す)
pub fn decode(port: u16) -> Option<IoTarget> {
    Some(match port {
        0x20 | 0x21 => IoTarget::Pic { slave: false },
        0xA0 | 0xA1 => IoTarget::Pic { slave: true },
        0x40..=0x43 => IoTarget::Pit,
        0x60 | 0x64 => IoTarget::Keyboard,
        0x61 => IoTarget::SystemControl,
        0x70 | 0x71 => IoTarget::Cmos,
        NET_BASE..=NET_LAST => IoTarget::Net,
        0x3D4 | 0x3D5 => IoTarget::Crtc,
        0x3F8..=0x3FF => IoTarget::Uart,
        _ => return None,
    })
}
