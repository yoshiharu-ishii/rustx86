//! ISA時代の装置。
//!
//! この3つは **32bit Linuxでもそのまま使う**。だから捨てにならない順として
//! 最初に作っている ([ADR-0002](../../../docs/adr/0002-devices-and-16bit-unix.md))。
//!
//! - [`pic`] 8259 — 割り込みの交通整理
//! - [`pit`] 8254 — 時を刻む
//! - [`uart`] 16550 — シリアルコンソール
//!
//! いずれも1980年代前半の部品で、互換性のために現代のPCにも生き残っている。

pub mod pic;
pub mod pit;
pub mod uart;

pub use pic::Pic8259;
pub use pit::Pit8254;
pub use uart::Uart16550;
