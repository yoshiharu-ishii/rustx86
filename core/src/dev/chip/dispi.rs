//! Bochs VGA の拡張レジスタ (DISPI / "VBE extensions") — Linux の `bochs-drm`、Xorg の
//! `modesetting`、QEMU の `-vga std` と同じ顔。PCI 1234:1111 (class 03 VGA) の BAR0 が
//! リニアフレームバッファ (VRAM)、設定はポート 0x1CE (index) / 0x1CF (data) の 16bit。
//!
//! なぜこれか (2026-08-23): DSL 2024 (antiX) の init は PCI の VGA を探し、無ければ Xorg を
//! vesa に振る。vesa は**実物の VGA BIOS (ROM) を x86emu で実行**する作法なので、BIOS を
//! HLE で持つこの機械では原理的に動かない。bochs-drm は ROM を見ず、このレジスタだけで
//! モードを組む。VESA BIOS (INT 10h 4Fxx) を書くより小さく、DRM → fbcon → X まで一本で通る。
//!
//! ## レジスタ (index → 意味)
//!
//! 0 ID (0xB0C0〜B0C5、版)  1 XRES  2 YRES  3 BPP  4 ENABLE  5 BANK  6 VIRT_WIDTH
//! 7 VIRT_HEIGHT  8 X_OFFSET  9 Y_OFFSET  A VIDEO_MEMORY_64K (VRAM の大きさ、64KB 単位)
//!
//! ENABLE: bit0 = 有効、bit1 = GETCAPS (立てて XRES/YRES/BPP を読むと最大値が返る)、
//! bit6 = LFB (リニア)、bit7 = NOCLEARMEM (有効化で VRAM を消さない)
//!
//! ## 描画
//!
//! VRAM は RAM の末尾 (16MB) をそのまま使う — ゲストは BAR0 の物理番地に書き、うちは
//! そこを LFB として表示側に見せる (efifb と同じ「ただの RAM の窓」)。画素は bochs の
//! 32bpp = XRGB8888 (LE の u32 で 0x00RRGGBB、バイト列は B,G,R,X) — efifb の [pad,R,G,B] と
//! 並びが逆なので、表示側は `lfb_xrgb()` で見分ける。16/24/8bpp は台帳 (bochs-drm は 32 しか使わない)

/// index ポート
pub const PORT_INDEX: u16 = 0x1CE;
/// data ポート
pub const PORT_DATA: u16 = 0x1CF;
/// PCI の身元
pub const VENDOR: u16 = 0x1234;
pub const DEVICE: u16 = 0x1111;
/// PCI スロット
pub const SLOT: usize = 5;
/// VRAM の大きさ (16MB = 1600×1200×4 が入る)
pub const VRAM_BYTES: u32 = 16 << 20;
/// 名乗る版 (bochs-drm は 0xB0C0 以上なら受ける)
pub const ID: u16 = 0xB0C5;
/// 最大解像度 (GETCAPS の答え)
pub const MAX_XRES: u16 = 1600;
pub const MAX_YRES: u16 = 1200;

const IDX_ID: u16 = 0;
const IDX_XRES: u16 = 1;
const IDX_YRES: u16 = 2;
const IDX_BPP: u16 = 3;
const IDX_ENABLE: u16 = 4;
const IDX_BANK: u16 = 5;
const IDX_VIRT_WIDTH: u16 = 6;
const IDX_VIRT_HEIGHT: u16 = 7;
const IDX_X_OFFSET: u16 = 8;
const IDX_Y_OFFSET: u16 = 9;
const IDX_VIDEO_MEMORY_64K: u16 = 0xA;

pub const ENABLE_ENABLED: u16 = 0x01;
pub const ENABLE_GETCAPS: u16 = 0x02;
pub const ENABLE_LFB: u16 = 0x40;
pub const ENABLE_NOCLEARMEM: u16 = 0x80;

#[derive(Debug, Clone)]
pub struct Dispi {
    index: u16,
    pub xres: u16,
    pub yres: u16,
    pub bpp: u16,
    pub enable: u16,
    pub bank: u16,
    pub virt_width: u16,
    pub virt_height: u16,
    pub x_offset: u16,
    pub y_offset: u16,
    /// VRAM (= LFB) の物理番地。RAM 末尾の 16MB
    pub vram_base: u32,
    /// 最後の書き込みで表示の形が変わった (Machine が lfb を組み直す合図)
    pub dirty: bool,
}

impl Dispi {
    pub fn new(vram_base: u32) -> Self {
        Dispi {
            index: 0,
            xres: 0,
            yres: 0,
            bpp: 0,
            enable: 0,
            bank: 0,
            virt_width: 0,
            virt_height: 0,
            x_offset: 0,
            y_offset: 0,
            vram_base,
            dirty: false,
        }
    }

    pub fn write_index(&mut self, v: u16) {
        self.index = v;
    }

    pub fn read_index(&self) -> u16 {
        self.index
    }

    pub fn read_data(&self) -> u16 {
        let caps = self.enable & ENABLE_GETCAPS != 0;
        match self.index {
            IDX_ID => ID,
            IDX_XRES => {
                if caps {
                    MAX_XRES
                } else {
                    self.xres
                }
            }
            IDX_YRES => {
                if caps {
                    MAX_YRES
                } else {
                    self.yres
                }
            }
            IDX_BPP => {
                if caps {
                    32
                } else {
                    self.bpp
                }
            }
            IDX_ENABLE => self.enable,
            IDX_BANK => self.bank,
            IDX_VIRT_WIDTH => self.virt_width,
            IDX_VIRT_HEIGHT => self.virt_height,
            IDX_X_OFFSET => self.x_offset,
            IDX_Y_OFFSET => self.y_offset,
            IDX_VIDEO_MEMORY_64K => (VRAM_BYTES >> 16) as u16,
            _ => 0,
        }
    }

    pub fn write_data(&mut self, v: u16) {
        match self.index {
            IDX_XRES => self.xres = v.min(MAX_XRES),
            IDX_YRES => self.yres = v.min(MAX_YRES),
            IDX_BPP => self.bpp = v,
            IDX_ENABLE => {
                self.enable = v;
                // 有効化の瞬間に仮想幅が無ければ実幅に揃える (bochs の作法)
                if v & ENABLE_ENABLED != 0 && self.virt_width == 0 {
                    self.virt_width = self.xres;
                }
            }
            IDX_BANK => self.bank = v,
            IDX_VIRT_WIDTH => self.virt_width = v,
            IDX_VIRT_HEIGHT => self.virt_height = v,
            IDX_X_OFFSET => self.x_offset = v,
            IDX_Y_OFFSET => self.y_offset = v,
            _ => {}
        }
        self.dirty = true;
    }

    /// 表示が有効か (ENABLE bit0 と、形が揃っているか)
    pub fn active(&self) -> bool {
        self.enable & ENABLE_ENABLED != 0 && self.xres > 0 && self.yres > 0 && self.bpp == 32
    }

    /// 今の表示の先頭 (VRAM の先頭 + Y_OFFSET 行 + X_OFFSET 画素)。1 行のバイト数は仮想幅
    pub fn frame_base(&self) -> u32 {
        let stride = self.stride();
        self.vram_base
            .wrapping_add(self.y_offset as u32 * stride)
            .wrapping_add(self.x_offset as u32 * 4)
    }

    /// 1 行のバイト数 (仮想幅 × 4)
    pub fn stride(&self) -> u32 {
        let w = if self.virt_width >= self.xres {
            self.virt_width
        } else {
            self.xres
        };
        w as u32 * 4
    }

    /// PCI の設定空間の顔。BAR0 = VRAM (16MB、prefetchable)
    pub fn pci_function(vram_base: u32) -> crate::bus::pci::PciFunction {
        use crate::bus::pci::{Bar, PciFunction};
        PciFunction::new(VENDOR, DEVICE, 0x03, 0x00, 0x00)
            .with_bar(
                0,
                Bar {
                    size: VRAM_BYTES,
                    io: false,
                },
                vram_base | 0x08, // bit3 = prefetchable
            )
            .with_subsystem(VENDOR, DEVICE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_caps_and_enable() {
        let mut d = Dispi::new(0x0700_0000);
        d.write_index(IDX_ID);
        assert_eq!(d.read_data(), ID);
        d.write_index(IDX_VIDEO_MEMORY_64K);
        assert_eq!(d.read_data(), 256);
        // GETCAPS: 最大値が読める
        d.write_index(IDX_ENABLE);
        d.write_data(ENABLE_GETCAPS);
        d.write_index(IDX_XRES);
        assert_eq!(d.read_data(), MAX_XRES);
        d.write_index(IDX_ENABLE);
        d.write_data(0);
        // モードを組む
        for (i, v) in [(IDX_XRES, 1024u16), (IDX_YRES, 768), (IDX_BPP, 32)] {
            d.write_index(i);
            d.write_data(v);
        }
        assert!(!d.active());
        d.write_index(IDX_ENABLE);
        d.write_data(ENABLE_ENABLED | ENABLE_LFB);
        assert!(d.active());
        assert_eq!(d.stride(), 4096);
        assert_eq!(d.frame_base(), 0x0700_0000);
        d.write_index(IDX_Y_OFFSET);
        d.write_data(10);
        assert_eq!(d.frame_base(), 0x0700_0000 + 10 * 4096);
    }
}
