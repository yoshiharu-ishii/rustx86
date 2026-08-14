//! ISA時代の装置。**1つの装置に1つのファイル**。
//!
//! 先頭の数個は **32bit Linuxでもそのまま使う**。だから捨てにならない順として
//! 最初に作っている ([ADR-0002](../../../../docs/adr/0002-devices-and-16bit-unix.md))。
//!
//! - [`pic`] 8259 — 割り込みの交通整理
//! - [`crtc`] MC6845 — カーソル位置と表示開始アドレス
//! - [`cmos`] MC146818 — 時計とマシンの構成情報
//! - [`kbd`] 8042 — キーボード。**ついでにA20ゲートも握っている**
//! - [`pit`] 8254 — 時を刻む
//! - [`uart`] 16550 — シリアルコンソール
//! - [`ne2000`] DP8390 — Ethernet。**PCI版 (RTL8029) の実体もこれ**で、
//!   [`pci::rtl8029`](super::pci::rtl8029) は設定空間の顔を着せるだけ
//!
//! 番地は定数で決め打ちできる (ISAには装置を数える仕組みが無く、
//! 「COM1は0x3F8」と全メーカーが示し合わせる世界だった)。振り分けは
//! [`mem::bus`](crate::mem::bus) の `match` が持つ。
//!
//! ne2000以外はいずれも1980年代前半の部品で、互換性のために現代のPCにも
//! 生き残っている。

pub mod cmos;
pub mod crtc;
pub mod kbd;
pub mod ne2000;
pub mod pic;
pub mod pit;
pub mod uart;

pub use cmos::Cmos;
pub use crtc::Crtc;
pub use kbd::Kbd8042;
pub use ne2000::Ne2000;
pub use pic::Pic8259;
pub use pit::Pit8254;
pub use uart::Uart16550;
