//! RAMDAC (INMOS IMS G171系、0x3C6-0x3C9) — 色番号を実際の色に変える表。
//!
//! mode 13h の画素は「256色のどれか」という**番号**でしかなく、番号が何色かは
//! このチップの256エントリ表 (各色 R/G/B 6bitずつ) が決める。ゲームが
//! パレットアニメーション (表だけ書き換えて絵を動かす) をできるのはこのため。
//!
//! ポートは4本だが芯は**自動歩進**にある: 0x3C8 に色番号を書いてから
//! 0x3C9 へ R→G→B と3回書くと、3回目で番号が勝手に次へ進む。
//! 256色×3バイトを OUT 命令の列だけで流し込むための、時代の工夫である。
//! 読み側 (0x3C7) も同じ作法で別のカウンタを持つ。
//!
//! 値は**6bitのまま持つ** (0〜63)。8bitへの伸長は描画側 (ブラウザ) の仕事で、
//! チップが持っている値をそのまま見せるのは speaker_tone や NIC と同じ境界。

/// 1エントリ3バイト (R,G,B) × 256色
pub const PALETTE_LEN: usize = 256 * 3;

#[derive(Debug)]
pub struct Dac {
    /// パレット本体。6bit値 (0〜63) の R,G,B が256色ぶん並ぶ
    rgb: [u8; PALETTE_LEN],
    /// 書き込みカーソル: 色番号 (0x3C8で設定)
    write_index: u8,
    /// 書き込みカーソル: R=0 / G=1 / B=2 のどこまで来たか
    write_phase: u8,
    /// 読み出しカーソル (0x3C7で設定)。書きと独立
    read_index: u8,
    read_phase: u8,
    /// PELマスク (0x3C6)。画素値とANDされる。ほぼ全ソフトが0xFFのまま
    pel_mask: u8,
}

/// 起動時のパレットの先頭16色 = EGAの16色 (6bit値)。
///
/// 全256色の既定表 (グレー階調+色相環) は実BIOSが持つものだが、
/// mode 13h を使うソフトはほぼ例外なく自分のパレットを流し込むので、
/// **使う者が現れるまで作らない** (台帳)。16色だけ埋めるのは、テキストモードの
/// 色番号と同じ感覚で描く小さなプログラムがそのまま映るようにするため。
const EGA16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], // 黒
    [0x00, 0x00, 0x2A], // 青
    [0x00, 0x2A, 0x00], // 緑
    [0x00, 0x2A, 0x2A], // シアン
    [0x2A, 0x00, 0x00], // 赤
    [0x2A, 0x00, 0x2A], // マゼンタ
    [0x2A, 0x15, 0x00], // 茶
    [0x2A, 0x2A, 0x2A], // 白
    [0x15, 0x15, 0x15], // 明るい黒
    [0x15, 0x15, 0x3F], // 明るい青
    [0x15, 0x3F, 0x15], // 明るい緑
    [0x15, 0x3F, 0x3F], // 明るいシアン
    [0x3F, 0x15, 0x15], // 明るい赤
    [0x3F, 0x15, 0x3F], // 明るいマゼンタ
    [0x3F, 0x3F, 0x15], // 黄
    [0x3F, 0x3F, 0x3F], // 明るい白
];

impl Default for Dac {
    fn default() -> Self {
        Self::new()
    }
}

impl Dac {
    pub fn new() -> Self {
        let mut rgb = [0u8; PALETTE_LEN];
        for (i, c) in EGA16.iter().enumerate() {
            rgb[i * 3..i * 3 + 3].copy_from_slice(c);
        }
        Self {
            rgb,
            write_index: 0,
            write_phase: 0,
            read_index: 0,
            read_phase: 0,
            pel_mask: 0xFF,
        }
    }

    /// 0x3C8: これから書き込む色番号。フェーズはRへ巻き戻る
    pub fn write_write_index(&mut self, val: u8) {
        self.write_index = val;
        self.write_phase = 0;
    }

    /// 0x3C7: これから読み出す色番号
    pub fn write_read_index(&mut self, val: u8) {
        self.read_index = val;
        self.read_phase = 0;
    }

    /// 0x3C9 書き: R→G→B の順に1バイトずつ。Bを書くと色番号が自動歩進する。
    /// 上位2bitは配線が無い (6bit DAC) ので落とす
    pub fn write_data(&mut self, val: u8) {
        let at = self.write_index as usize * 3 + self.write_phase as usize;
        self.rgb[at] = val & 0x3F;
        self.write_phase += 1;
        if self.write_phase == 3 {
            self.write_phase = 0;
            self.write_index = self.write_index.wrapping_add(1);
        }
    }

    /// 0x3C9 読み: 書きと同じ作法で、読み側のカーソルが歩く
    pub fn read_data(&mut self) -> u8 {
        let at = self.read_index as usize * 3 + self.read_phase as usize;
        let val = self.rgb[at];
        self.read_phase += 1;
        if self.read_phase == 3 {
            self.read_phase = 0;
            self.read_index = self.read_index.wrapping_add(1);
        }
        val
    }

    /// 0x3C8 読み: 今の書き込みカーソルの色番号
    pub fn read_write_index(&self) -> u8 {
        self.write_index
    }

    pub fn write_pel_mask(&mut self, val: u8) {
        self.pel_mask = val;
    }

    pub fn read_pel_mask(&self) -> u8 {
        self.pel_mask
    }

    /// パレット全体 (6bit値のまま)。描画側が毎フレーム読む
    pub fn palette(&self) -> &[u8; PALETTE_LEN] {
        &self.rgb
    }

    /// 1色ぶん (R,G,B)。テストと診断用
    pub fn color(&self, index: u8) -> [u8; 3] {
        let at = index as usize * 3;
        [self.rgb[at], self.rgb[at + 1], self.rgb[at + 2]]
    }
}

impl Dac {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.bytes(&self.rgb);
        w.u8(self.write_index);
        w.u8(self.write_phase);
        w.u8(self.read_index);
        w.u8(self.read_phase);
        w.u8(self.pel_mask);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        let rgb = r.bytes()?;
        if rgb.len() != self.rgb.len() {
            return Err("DACのパレット長が合わない".into());
        }
        self.rgb.copy_from_slice(&rgb);
        self.write_index = r.u8()?;
        self.write_phase = r.u8()?;
        self.read_index = r.u8()?;
        self.read_phase = r.u8()?;
        self.pel_mask = r.u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0x3C8→0x3C9×3 の自動歩進。256色の流し込みがこの形で成立する
    #[test]
    fn write_auto_increments_after_blue() {
        let mut d = Dac::new();
        d.write_write_index(16);
        for v in [1u8, 2, 3, 4, 5, 6] {
            d.write_data(v);
        }
        assert_eq!(d.color(16), [1, 2, 3]);
        assert_eq!(d.color(17), [4, 5, 6], "Bを書いた瞬間に次の色へ進む");
    }

    /// 6bit DACに上位2bitの配線は無い
    #[test]
    fn six_bit_only() {
        let mut d = Dac::new();
        d.write_write_index(0);
        d.write_data(0xFF);
        assert_eq!(d.color(0)[0], 0x3F);
    }

    /// 読みカーソルは書きカーソルと独立に歩く
    #[test]
    fn read_cursor_is_independent() {
        let mut d = Dac::new();
        d.write_read_index(1); // EGA16の青
        assert_eq!([d.read_data(), d.read_data(), d.read_data()], [0, 0, 0x2A]);
        assert_eq!(d.read_write_index(), 0, "書き側は動いていない");
        // 3回読んだので読み側は次の色 (緑) へ
        assert_eq!([d.read_data(), d.read_data(), d.read_data()], [0, 0x2A, 0]);
    }
}
