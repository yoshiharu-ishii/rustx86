//! アドレスの振り分け (バス)。
//!
//! x86には**アドレス空間が2つある**。メモリ空間と、64KしかないI/Oポート空間で、
//! 後者には `IN`/`OUT` 命令だけが届く。8080時代からの名残で、PIC/PIT/UARTといった
//! ISA時代の装置は今もこちら側に居る。このファイルはその2つのデコーダを持つ。
//!
//! 16bit時代のPCはアドレスが**定数として焼かれている**。ISAには装置を列挙する
//! 仕組みが無く、「VGAのテキスト画面は 0xB8000」「COM1 は 0x3F8」と決め打ちで
//! 全メーカーが合わせていた。だからデコーダは `match` で足りる。
//! 装置を数える仕組み (PCIの設定空間) が要るのは Tier 4 からである。
//!
//! ## なぜブリッジを作らないか
//!
//! 現代PCにはノースブリッジ/サウスブリッジやPCIホストブリッジがあるが、
//! あれは**装置が動的に増えるようになってから**必要になったものである。
//! アドレスが定数なら、間に立って経路を決める者は要らない。

/// メモリ空間の区画。
///
/// IBM PCは1MBを「下位640KBはRAM、上位384KBは装置とROMのための窓」と
/// 決めた。この区切りが後年の「640KBの壁」になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemRegion {
    /// 0x00000-0x9FFFF: 通常のRAM (いわゆるコンベンショナルメモリ 640KB)
    Ram,
    /// 0xA0000-0xAFFFF: グラフィックス画面 (未実装。Tier 6)
    VideoGraphics,
    /// 0xB0000-0xB7FFF: モノクロテキスト画面 (MDA。未実装)
    VideoMono,
    /// 0xB8000-0xBFFFF: **カラーテキスト画面**。文字と属性が交互に並ぶ
    VideoText,
    /// 0xC0000-0xFFFFF: 拡張ROMとシステムBIOS
    Rom,
}

/// カラーテキスト画面の先頭
pub const VRAM_TEXT_BASE: u32 = 0xB_8000;
/// 同 末尾 (0xBFFFF まで)
pub const VRAM_TEXT_END: u32 = 0xB_FFFF;
pub const TEXT_COLS: usize = 80;
pub const TEXT_ROWS: usize = 25;
/// 1文字が2バイト (文字コード + 属性) なのがテキストVRAMの肝
pub const TEXT_CELL: usize = 2;
pub const TEXT_LEN: usize = TEXT_COLS * TEXT_ROWS * TEXT_CELL;

pub fn decode_mem(addr: u32) -> MemRegion {
    match addr & 0xF_FFFF {
        0x00000..=0x9FFFF => MemRegion::Ram,
        0xA0000..=0xAFFFF => MemRegion::VideoGraphics,
        0xB0000..=0xB7FFF => MemRegion::VideoMono,
        0xB8000..=0xBFFFF => MemRegion::VideoText,
        _ => MemRegion::Rom,
    }
}

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
    /// 誰も名乗り出ないポート
    Unmapped,
}

pub fn decode_io(port: u16) -> IoTarget {
    match port {
        0x20 | 0x21 => IoTarget::Pic { slave: false },
        0xA0 | 0xA1 => IoTarget::Pic { slave: true },
        0x40..=0x43 => IoTarget::Pit,
        0x60 | 0x64 => IoTarget::Keyboard,
        0x61 => IoTarget::SystemControl,
        0x70 | 0x71 => IoTarget::Cmos,
        0x300..=0x31F => IoTarget::Net,
        0x3D4 | 0x3D5 => IoTarget::Crtc,
        0x3F8..=0x3FF => IoTarget::Uart,
        _ => IoTarget::Unmapped,
    }
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
    pub net: Option<crate::dev::Ne2000>,
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
        }
    }
}
