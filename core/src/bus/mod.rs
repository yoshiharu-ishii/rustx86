//! アドレスの振り分け (バス) — **どこに何が居るか**。
//!
//! x86には**アドレス空間が2つある**。メモリ空間と、64KしかないI/Oポート空間で、
//! 後者には `IN`/`OUT` 命令だけが届く。8080時代からの名残で、PIC/PIT/UARTといった
//! 古い装置は今もこちら側に居る。**このディレクトリはその2つの地図を持つ。**
//!
//! 装置が「なにか」は [`dev`](crate::dev) の話で、ここは「どう見つかるか」だけを扱う
//! ([ADR-0018](../../../docs/adr/0018-devices-chip-card-bus.md))。
//!
//! - [`memmap`] — メモリ空間の地図 (RAM・VRAM窓・ROM窓)
//! - [`isa`] — 固定番地の表と、割り込み線の配線
//! - [`pci`] — 装置を数える仕組み (設定空間・BAR・スロット)
//!
//! **ディレクトリはバス、ファイルは地図。** 例外を作らないので、次に増える
//! バス (virtio-mmioなど) の置き場所も迷わない。
//!
//! ## なぜブリッジを作らないか
//!
//! 現代PCにはノースブリッジ/サウスブリッジがあるが、あれは**装置が動的に
//! 増えるようになってから**必要になったものである。アドレスが定数なら、
//! 間に立って経路を決める者は要らない。

pub mod isa;
pub mod memmap;
pub mod pci;

pub use memmap::{
    decode_mem, MemRegion, TEXT_CELL, TEXT_COLS, TEXT_LEN, TEXT_ROWS, VRAM_TEXT_BASE, VRAM_TEXT_END,
};

/// I/Oポート空間の宛先。
///
/// 番号がばらばらに見えるのは、IBM PCが装置を足していった順に空いている番地を
/// 割り当てた結果である。設計ではなく履歴なので、規則を探しても見つからない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoTarget {
    /// 0x20-0x21 / 0xA0-0xA1: 8259 PIC (マスタ / スレーブ)。
    /// 2個あるのはIRQが8本では足りなくなり、片方をもう片方にぶら下げたため
    Pic { slave: bool },
    /// 0x40-0x43: 8254 PIT。OSのスケジューラが時を刻むのに使う
    Pit,
    /// 0x60 / 0x64: キーボードコントローラ (8042)
    Keyboard,
    /// 0x70 / 0x71: CMOS RTC (時計とマシンの構成情報)
    Cmos,
    /// 0x61: システム制御 (スピーカ、リフレッシュビット)
    SystemControl,
    /// 0x3D4 / 0x3D5: CRTC (カーソル位置、表示開始アドレス)
    Crtc,
    /// 0x3F8-0x3FF: UART 16550 (COM1)。Linuxのシリアルコンソールもここ
    Uart,
    /// 0x300-0x31F: NE2000 (Ethernet)。ISAカードの定番アドレス。
    /// カードを挿していない機械では Unmapped と同じ顔をする
    Net,
    /// 0xCF8-0xCFF: PCIの設定空間の窓 (機構#1)。**ここだけが定数**で、
    /// この窓の向こうにある装置の番地は実行時に決まる (BARが動かす)
    PciConfig,
    /// 誰も名乗り出ないポート
    Unmapped,
}

/// I/Oポートの宛先を決める。**固定番地が先、動く番地は後**。
///
/// ISAの表に名乗り手が居なければPCIの設定空間の窓を見る。どちらでもない番地は
/// 未接続で、BARが配った窓に当たるかどうかは実行時にしか分からないので
/// [`Machine`](crate::Machine) 側が探す
pub fn decode_io(port: u16) -> IoTarget {
    if let Some(target) = isa::decode(port) {
        return target;
    }
    // 設定空間の窓**だけ**が定数である。この窓の向こうの装置の番地はBARが動かす
    if (pci::CONFIG_ADDRESS..=pci::CONFIG_ADDRESS + 7).contains(&port) {
        return IoTarget::PciConfig;
    }
    IoTarget::Unmapped
}

/// I/Oポート空間にぶら下がる装置一式。
///
/// トレイトオブジェクトの表は作らない。アドレスが定数である以上、
/// 実行時に宛先を探す必要が無く、名前付きのフィールドと `match` で足りる。
/// 動的な登録が要るのはPCIからである。
pub struct Devices {
    /// 8259 PIC。マスタ (0x20-0x21) とスレーブ (0xA0-0xA1) の2個
    pub pic: [crate::dev::Pic8259; 2],
    /// 8254 PIT (0x40-0x43)
    pub pit: crate::dev::Pit8254,
    /// UART 16550 / COM1 (0x3F8-0x3FF)
    pub uart: crate::dev::Uart16550,
    /// 8042 キーボードコントローラ (0x60, 0x64)。A20ゲートもここが握る
    pub keyboard: crate::dev::Kbd8042,
    /// MC146818 CMOS RTC (0x70, 0x71)
    pub cmos: crate::dev::Cmos,
    /// MC6845 CRTC (0x3D4, 0x3D5)
    pub crtc: crate::dev::Crtc,
    /// システム制御ポート (0x61)。bit4がDRAMリフレッシュの矩形波で、
    /// OSはこれを数えて時間を測ることがある
    pub sysctl: u8,
    /// NE2000 (0x300-0x31F)。**挿さっていないのが既定** — NIC無し起動の
    /// ビット同一 (ADR-0017の不変条件) は、装置が居ないことで自明に守られる
    pub net: Option<crate::dev::Dp8390>,
    /// PCIのホストブリッジ。**16bit機には挿さない** — 1980年代の機械に
    /// PCIは無く、挿すと起動の命令列が変わる。世代で分けるのが史実にも合う
    pub pci: Option<pci::PciHost>,
    /// virtio-blk (PCIスロット4)。**挿さっていないのが既定** — NICと同じで、
    /// 装置が居なければ起動はビット同一のまま (ADR-0017の不変条件)
    pub blk: Option<crate::dev::VirtioBlk>,
}

impl Default for Devices {
    fn default() -> Self {
        Self::new()
    }
}

impl Devices {
    pub fn new() -> Self {
        Self {
            pic: [crate::dev::Pic8259::new(), crate::dev::Pic8259::new()],
            pit: crate::dev::Pit8254::new(),
            uart: crate::dev::Uart16550::new(),
            keyboard: crate::dev::Kbd8042::new(),
            cmos: crate::dev::Cmos::new(),
            crtc: crate::dev::Crtc::new(),
            sysctl: 0,
            net: None,
            pci: None,
            blk: None,
        }
    }
}
