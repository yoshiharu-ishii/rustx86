//! メモリ空間の地図。
//!
//! IBM PCは1MBを「下位640KBはRAM、上位384KBは装置とROMのための窓」と決めた。
//! **地図であってバスではない**ので、バスのディレクトリと並ぶ平のファイルに置く
//! ([ADR-0018](../../../docs/adr/0018-devices-chip-card-bus.md))。

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

/// グラフィックス画面 (mode 13h) の先頭。**ただのRAM**であり書き込みフックは
/// 無い — 表示側が毎フレーム全読みして描く (64KBなのでdirty追跡は要らない。
/// テキストVRAMのようにフック式にすると、ゲストの画素ストアが全部JITの
/// 高速路から弾かれる)
pub const VRAM_GFX_BASE: u32 = 0xA_0000;
/// mode 13h の画面幅・高さ・画素数 (1画素1バイト = 色番号)
pub const GFX_COLS: usize = 320;
pub const GFX_ROWS: usize = 200;
pub const GFX_LEN: usize = GFX_COLS * GFX_ROWS;

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
