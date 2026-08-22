//! 素子 — **チップそのもの。番地もバスも知らない**。
//!
//! ここに居るのは部品であって「ISAの装置」ではない。8259も8254も8042も
//! **マザーボードに半田付けされたチップ**で、ISAカードとして挿さっていた
//! わけではない (どれもISAの標準化より前の部品である)。どの番地で見つかるかは
//! 機械の配線の話なので [`bus`](crate::bus) が持ち、ここには持ち込まない。
//!
//! - [`pic`] 8259 — 割り込みの交通整理
//! - [`crtc`] MC6845 — カーソル位置と表示開始アドレス
//! - [`dac`] RAMDAC (IMS G171系) — 256色パレット。色番号を実際の色に変える
//! - [`cmos`] MC146818 — 時計とマシンの構成情報
//! - [`kbd`] 8042 — キーボード。**ついでにA20ゲートも握っている**
//! - [`mouse`] PS/2マウス — 8042の第2ポート (AUX) の向こう側
//! - [`pit`] 8254 — 時を刻む
//! - [`uart`] 16550 — シリアルコンソール
//! - [`dp8390`] DP8390 — Ethernet。**ISAのNE2000もPCIのRTL8029も中身はこれ1つ**
//! - [`virtio`] virtio (legacy) — 準仮想化の共通の口。リングとレジスタ窓
//!
//! 素子は機械を組まずに単体で試せる ([`Dp8390`] のテストが `Machine` を要らないのが
//! その形)。「なにか」と「どう見つかるか」を分けた効き目がここに出る。
//!
//! dp8390以外はいずれも1980年代前半の部品で、互換性のために現代のPCにも
//! 生き残っている ([ADR-0002](../../../../docs/adr/0002-devices-and-16bit-unix.md))。

pub mod cmos;
pub mod crtc;
pub mod dac;
pub mod dp8390;
pub mod kbd;
pub mod mouse;
pub mod opl2;
pub mod pic;
pub mod pit;
pub mod uart;
pub mod vga;
pub mod virtio;

pub use cmos::Cmos;
pub use crtc::Crtc;
pub use dac::Dac;
pub use dp8390::Dp8390;
pub use kbd::Kbd8042;
pub use mouse::Mouse;
pub use opl2::Opl2;
pub use pic::Pic8259;
pub use pit::Pit8254;
pub use uart::Uart16550;
pub use vga::Vga;
pub use virtio::VirtioPci;
