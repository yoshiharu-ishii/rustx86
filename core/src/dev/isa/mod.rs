//! ISA時代の装置。
//!
//! この3つは **32bit Linuxでもそのまま使う**。だから捨てにならない順として
//! 最初に作っている ([ADR-0002](../../../../docs/adr/0002-devices-and-16bit-unix.md))。
//!
//! - [`pic`] 8259 — 割り込みの交通整理
//! - [`crtc`] MC6845 — カーソル位置と表示開始アドレス
//! - [`cmos`] MC146818 — 時計とマシンの構成情報
//! - [`kbd`] 8042 — キーボード。**ついでにA20ゲートも握っている**
//! - [`pit`] 8254 — 時を刻む
//! - [`uart`] 16550 — シリアルコンソール
//!
//! いずれも1980年代前半の部品で、互換性のために現代のPCにも生き残っている。

pub mod cmos;
pub mod crtc;
pub mod kbd;
pub mod pic;
pub mod pit;
pub mod uart;

pub use cmos::Cmos;
pub use crtc::Crtc;
pub use kbd::Kbd8042;
pub use pic::Pic8259;
pub use pit::Pit8254;
pub use uart::Uart16550;
