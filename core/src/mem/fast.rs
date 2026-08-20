//! translate-first の速い道 (F1c-d5) — 平坦セグメント・ページ内・変換成功が
//! 全部揃うときだけ Some を返す部品群。**成功が確定してから実行する**ので
//! 呼び手はguard控えを省ける。揃わなければ None (呼び手は控えてから従来経路へ —
//! フォールトの配送は常に従来経路 = 控えの不変条件は無傷)。
//! dcache/exec.rs の速い道と mem/jit.rs のJITヘルパが共用する。

use crate::{bus, Machine};

impl Machine {
    #[inline]
    pub(crate) fn fast_read32(&mut self, seg: usize, off: u32) -> Option<u32> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC {
            return None; // ページ跨ぎは従来経路 (write16×2系の意味を守る)
        }
        match self.translate_for(off, false) {
            Ok(pa) => Some(self.read_phys32(pa)),
            Err(_) => None, // フォールトは従来経路が控えつきでやり直す
        }
    }

    /// fast_read32 の8bit版 (1バイトは跨げない)
    #[inline]
    pub(crate) fn fast_read8(&mut self, seg: usize, off: u32) -> Option<u8> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        match self.translate_for(off, false) {
            Ok(pa) => Some(self.read_phys8(pa)),
            Err(_) => None,
        }
    }

    /// fast_read32 の16bit版
    #[inline]
    pub(crate) fn fast_read16(&mut self, seg: usize, off: u32) -> Option<u16> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFE {
            return None;
        }
        match self.translate_for(off, false) {
            Ok(pa) => {
                let a = pa as usize;
                if a + 2 <= self.mem.len() {
                    Some(self.mem[a] as u16 | (self.mem[a + 1] as u16) << 8)
                } else {
                    Some(0xFFFF) // read16と同じ器
                }
            }
            Err(_) => None,
        }
    }

    /// fast_read32 の書き込み版。Some(()) = 書き終えた (RAM超えの捨ても含む —
    /// write_wideと同じ意味)。None = 従来経路へ (VRAM窓・デバッガ含む)
    #[inline]
    pub(crate) fn fast_write32(&mut self, seg: usize, off: u32, val: u32) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 4 > self.mem.len() {
            return Some(()); // RAM超えは捨てる (write_wideと同じ完了扱い)
        }
        if a + 3 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None; // テキストVRAM窓は遅い道 (vram_dirtyの約束)
        }
        self.mem[a..a + 4].copy_from_slice(&val.to_le_bytes());
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        Some(())
    }

    /// RMW (`alu [mem], b`) の速い道: **書き込み権限で先に変換** (x86に
    /// 書き込み専用ページは無い — writable ⊆ readable) すれば、cc更新後に
    /// 失敗する道が消える。返り値は物理index (RAM内・VRAM外・ページ内)
    #[inline]
    pub(crate) fn fast_rmw32_addr(&mut self, seg: usize, off: u32) -> Option<usize> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 4 > self.mem.len() {
            return None; // RAM外RMWは従来経路 (読める器0xFFの意味を守る)
        }
        if a + 3 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None;
        }
        Some(a)
    }

    /// fast_rmw32_addr の8bit版 (跨ぎ無し)
    #[inline]
    pub(crate) fn fast_rmw8_addr(&mut self, seg: usize, off: u32) -> Option<usize> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a >= self.mem.len() {
            return None;
        }
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            return None;
        }
        Some(a)
    }

    /// fast_write32 の8bit版 (write8の写し — VRAMはdirtyを立てて書く)
    #[inline]
    pub(crate) fn fast_write8(&mut self, seg: usize, off: u32, v: u8) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a >= self.mem.len() {
            return Some(()); // write8と同じ捨て
        }
        self.mem[a] = v;
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            self.vram_dirty = true;
        }
        Some(())
    }

    /// fast_write32 の16bit版 (VRAM窓は遅い道)
    #[inline]
    pub(crate) fn fast_write16(&mut self, seg: usize, off: u32, v: u16) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        if off & 0xFFF > 0xFFE {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 2 > self.mem.len() {
            return Some(());
        }
        if a + 1 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None;
        }
        self.mem[a..a + 2].copy_from_slice(&v.to_le_bytes());
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        Some(())
    }
}
