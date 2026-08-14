//! 装置。**ハードウェアバスごとにディレクトリを分ける** ([ADR-0017](../../../docs/adr/0017-network-isa-first.md))。
//!
//! - [`isa`] — ISA (とその前身のPC/XTバス) の装置。アドレスは定数で、
//!   デコーダは `match` で足りる
//! - [`pci`] — 装置を数える仕組み (設定空間) を持つ側。**番地はBARで動く**ので、
//!   振り分けも定数の`match`ではなくPCI側が実行時に持つ
//!
//! 再エクスポートしているのは、使う側 (バスのデコーダ) にとって装置の所属バスが
//! 型名の一部である必要は無いからである。

pub mod isa;
pub mod pci;

pub use isa::{Cmos, Crtc, Kbd8042, Ne2000, Pic8259, Pit8254, Uart16550};
pub use pci::PciHost;
