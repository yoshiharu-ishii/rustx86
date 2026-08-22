//! VGA のシーケンサ (0x3C4/0x3C5) とグラフィックスコントローラ (0x3CE/0x3CF) —
//! **Mode Y (unchained 256色)** の器。
//!
//! mode 13h の 0xA0000 は「ただの RAM」(チェーン4: 番地がそのまま画素) で、
//! 書き込みフックを付けない約束 (ロードマップ 6a、JIT の高速路のため)。
//! ところが DOS 版 DOOM は mode 13h に入った直後にシーケンサの**チェーン4を
//! 切り**、4枚のプレーンへマップマスクで書き、CRTC の開始番地でページを
//! めくる (Mode Y / Mode X)。これだと同じ番地への書き込みがマスクごとに別の
//! プレーンへ落ちるので、線形 64KB では区別できない — タイトル画面が横に
//! 4枚・縦に3枚並んで見えた (2026-08-22)。
//!
//! そこで**チェーン4が切れている間だけ**窓を装置にする。熱い経路 (write8 /
//! write_wide / fast_write / JIT) が見るのは bool 1つ (`planar`) で、
//! 普段の mode 13h も Linux もこれまでどおり素通り。
//!
//! 読み出しにはフックを入れない (読み出しは最も回数の多い経路)。代わりに
//! **RAM の窓を「読み出しマップ (GC reg 4) が指すプレーンの写し」に保つ**:
//! プレーンへ書くたびに、そのプレーンが読み出し対象なら窓も更新し、読み出し
//! マップが変わったらプレーンを窓へ写す (64KB、稀)。ゲストが窓を読めば
//! 実機と同じ値が返る。
//!
//! 表示は合成: 画素 (x, y) = プレーン (x & 3) のオフセット (start + y*80 + x/4)。
//! start は CRTC の開始番地 (0x0C/0x0D) — DOOM のページめくり。

/// 1プレーンの大きさ (64KB)
pub const PLANE: usize = 0x1_0000;

/// シーケンサ/GC への書き込みが起こす、機械側 (RAM の窓) の仕事
#[derive(Debug, PartialEq, Eq)]
pub enum VgaEvent {
    None,
    /// チェーン4が切れた: 窓の中身をプレーンへ展開する (番地 a → プレーン a&3、オフセット a>>2)
    EnteredPlanar,
    /// チェーン4が戻った: プレーンを窓へ畳む
    LeftPlanar,
    /// 読み出しマップが変わった: そのプレーンを窓へ写す
    ReadMapChanged,
}

pub struct Vga {
    seq_index: u8,
    seq: [u8; 8],
    gc_index: u8,
    gc: [u8; 16],
    /// 4プレーン × 64KB
    planes: Vec<u8>,
    /// チェーン4が切れている (= 窓を装置として扱う)。熱い経路の 1 判定
    pub planar: bool,
}

impl Default for Vga {
    fn default() -> Self {
        Self::new()
    }
}

impl Vga {
    pub fn new() -> Self {
        let mut v = Self {
            seq_index: 0,
            seq: [0; 8],
            gc_index: 0,
            gc: [0; 16],
            planes: vec![0; 4 * PLANE],
            planar: false,
        };
        v.reset_mode13();
        v
    }

    /// mode 13h の既定: チェーン4 on、全プレーン書き込み可、読み出しはプレーン0、
    /// 書き込みモード0・256色、ビットマスク全通し
    pub fn reset_mode13(&mut self) {
        self.seq = [0x03, 0x01, 0x0F, 0x00, 0x0E, 0, 0, 0];
        self.gc = [0; 16];
        self.gc[5] = 0x40;
        self.gc[8] = 0xFF;
        self.planar = false;
        self.planes.fill(0);
    }

    pub fn seq_write_index(&mut self, v: u8) {
        self.seq_index = v & 7;
    }

    pub fn seq_read_data(&self) -> u8 {
        self.seq[self.seq_index as usize]
    }

    pub fn seq_write_data(&mut self, v: u8) -> VgaEvent {
        self.seq[self.seq_index as usize] = v;
        if self.seq_index == 4 {
            // メモリモード: bit3 = チェーン4
            let planar = v & 0x08 == 0;
            if planar != self.planar {
                self.planar = planar;
                return if planar {
                    VgaEvent::EnteredPlanar
                } else {
                    VgaEvent::LeftPlanar
                };
            }
        }
        VgaEvent::None
    }

    pub fn gc_write_index(&mut self, v: u8) {
        self.gc_index = v & 0xF;
    }

    pub fn gc_read_data(&self) -> u8 {
        self.gc[self.gc_index as usize]
    }

    pub fn gc_write_data(&mut self, v: u8) -> VgaEvent {
        let old = self.gc[self.gc_index as usize];
        self.gc[self.gc_index as usize] = v;
        if self.gc_index == 4 && old & 3 != v & 3 {
            VgaEvent::ReadMapChanged
        } else {
            VgaEvent::None
        }
    }

    /// 書き込み先プレーンのマスク (シーケンサ reg 2)
    pub fn map_mask(&self) -> u8 {
        self.seq[2] & 0x0F
    }

    /// 読み出しプレーン (GC reg 4)
    pub fn read_map(&self) -> usize {
        (self.gc[4] & 3) as usize
    }

    /// プレーンへの書き込み (チェーン4 off)。マスクの立ったプレーン全部へ同じ値。
    /// 返り値は「読み出しマップのプレーンのその位置の値」— RAM の窓に写す値
    pub fn write(&mut self, off: usize, val: u8) -> u8 {
        let off = off & (PLANE - 1);
        let mask = self.map_mask();
        for p in 0..4 {
            if mask & (1 << p) != 0 {
                self.planes[p * PLANE + off] = val;
            }
        }
        self.planes[self.read_map() * PLANE + off]
    }

    pub fn plane(&self, p: usize) -> &[u8] {
        &self.planes[p * PLANE..(p + 1) * PLANE]
    }

    /// チェーン4 が切れた瞬間: 線形の窓をプレーンへ展開 (番地 a → プレーン a&3、a>>2)。
    /// 実機では同じメモリを別の番地付けで見ているだけなので、中身は引き継がれる
    pub fn unchain(&mut self, window: &[u8]) {
        for (a, &b) in window.iter().enumerate().take(PLANE) {
            self.planes[(a & 3) * PLANE + (a >> 2)] = b;
        }
    }

    /// チェーン4 が戻った瞬間: プレーンを線形の窓へ畳む (unchain の逆)
    pub fn rechain(&self, window: &mut [u8]) {
        for (a, b) in window.iter_mut().enumerate().take(PLANE) {
            *b = self.planes[(a & 3) * PLANE + (a >> 2)];
        }
    }

    /// 表示の合成 (320×200): 画素 (x, y) = プレーン x&3 の start + y*80 + x/4
    pub fn compose(&self, start: usize, out: &mut [u8]) {
        for y in 0..200 {
            let row = start + y * 80;
            for x in 0..320 {
                out[y * 320 + x] = self.planes[(x & 3) * PLANE + ((row + x / 4) & (PLANE - 1))];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_mask_spreads_writes_and_compose_interleaves() {
        let mut v = Vga::new();
        v.seq_write_index(4);
        assert_eq!(v.seq_write_data(0x06), VgaEvent::EnteredPlanar);
        assert!(v.planar);
        // プレーン1だけに 7 を書く → 画素 x=1 (x&3=1) に出る
        v.seq_write_index(2);
        v.seq_write_data(0x02);
        assert_eq!(v.write(0, 7), 0); // 読み出しマップはプレーン0 → 窓の値は 0 のまま
        let mut out = vec![0u8; 320 * 200];
        v.compose(0, &mut out);
        assert_eq!(&out[0..4], &[0, 7, 0, 0]);
        // 読み出しマップをプレーン1へ
        v.gc_write_index(4);
        assert_eq!(v.gc_write_data(1), VgaEvent::ReadMapChanged);
        assert_eq!(v.plane(1)[0], 7);
        // ページめくり: start=80 なら 1 行下が先頭に来る
        v.seq_write_index(2);
        v.seq_write_data(0x0F);
        v.write(80, 9); // 全プレーン、行1の先頭 → 画素 (0..4, 1)
        v.compose(80, &mut out);
        assert_eq!(&out[0..4], &[9, 9, 9, 9]);
    }

    #[test]
    fn unchain_and_rechain_are_inverse() {
        let mut v = Vga::new();
        let window: Vec<u8> = (0..PLANE).map(|a| (a * 7) as u8).collect();
        v.unchain(&window);
        assert_eq!(v.plane(1)[0], window[1]);
        assert_eq!(v.plane(0)[1], window[4]);
        let mut back = vec![0u8; PLANE];
        v.rechain(&mut back);
        assert_eq!(back, window);
    }
}
