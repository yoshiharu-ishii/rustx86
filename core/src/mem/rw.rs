//! 線形アドレスの読み書き — CPUが触る正規の経路と、テキストVRAMの窓。
//!
//! 読み出しは最も回数の多い経路なので分岐を足さない、書き込み側に
//! 仕掛けを寄せる (VRAM検出・自己書き換え検出)、という非対称が設計の芯。

use crate::{bus, debug, Machine};

impl Machine {
    /// 線形アドレスから読む。**ページングが有効ならここで物理へ変換する**。
    /// CPUが触るのはこちら (呼び出し側は線形アドレスを渡す)
    pub fn read8(&self, addr: u32) -> u8 {
        match self.translate_for(addr, false) {
            Ok(pa) => self.read_phys8(pa),
            Err(f) => {
                self.note_fault(f);
                0xFF // フォールトした読みの器。命令の終わりに#PFで巻き戻す
            }
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        // ページ内に収まるなら**1回の変換で2バイト**読む。
        // ページ跨ぎ (稀) のときだけバイトごとに落とす
        if addr & 0xFFF <= 0xFFE {
            match self.translate_for(addr, false) {
                Ok(pa) => {
                    let a = pa as usize;
                    if a + 2 <= self.mem.len() {
                        return self.mem[a] as u16 | (self.mem[a + 1] as u16) << 8;
                    }
                    0xFFFF
                }
                Err(f) => {
                    self.note_fault(f);
                    0xFFFF
                }
            }
        } else {
            self.read8(addr) as u16 | (self.read8(addr.wrapping_add(1)) as u16) << 8
        }
    }

    pub fn read32(&self, addr: u32) -> u32 {
        // ページ内に収まるなら**1回の変換で4バイト**読む
        if addr & 0xFFF <= 0xFFC {
            match self.translate_for(addr, false) {
                Ok(pa) => self.read_phys32(pa),
                Err(f) => {
                    self.note_fault(f);
                    0xFFFF_FFFF
                }
            }
        } else {
            self.read16(addr) as u32 | (self.read16(addr.wrapping_add(2)) as u32) << 16
        }
    }

    /// メモリ書き込み。
    ///
    /// テキストVRAMは**メモリ空間に居座る装置**なので、素通しで `mem` に書く。
    /// 実機でもビデオカードのRAMがCPUのアドレス空間に窓として現れているだけで、
    /// 書き込み経路に特別な変換は無い。ここで足しているのは描画側への合図と、
    /// 自己書き換えの申告 (note_write) だけ。
    ///
    /// 読み出し ([`read8`](Self::read8)) には一切分岐を入れていない。
    /// メモリアクセスは最も回数の多い経路なので、**書き込み側だけで済む
    /// 仕掛けなら書き込み側に寄せる**。
    pub fn write8(&mut self, addr: u32, val: u8) {
        // 線形→物理。以後の VRAM 判定もデバッガも**物理番地**で語る
        // (VRAMは物理アドレス空間の窓なので、そこに写像された線形から書いても
        //  正しく dirty が立つ)
        let a = match self.translate_for(addr, true) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return; // フォールトした書き込みは実行しない (再実行で改めて書く)
            }
        } as usize;
        if a >= self.mem.len() {
            return; // RAMを超えた書き込みは捨てる
        }
        // デバッガを切っていれば真偽値1つで抜ける。**最も回数の多い経路**なので
        // 見張る番地の集合を引く前に元締めで落とす
        if self.dbg.on && self.dbg.mem_write.contains(&(a as u32)) {
            self.dbg.stop = Some(debug::Stop::WriteMem {
                addr: a as u32,
                old: self.mem[a],
                new: val,
                at: self.dbg.at,
            });
        }
        self.mem[a] = val;
        // コードを控えたページへの書き込みは写しを無効化 (自己書き換え対策)。
        // データページなら has_code の1判定で素通り (ADR-0007の許容コスト)
        self.dcache.note_write(a as u32);
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            self.vram_dirty = true;
        }
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        if addr & 0xFFF <= 0xFFE && self.write_wide(addr, val as u32, 2) {
            return;
        }
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        if addr & 0xFFF <= 0xFFC && self.write_wide(addr, val, 4) {
            return;
        }
        self.write16(addr, val as u16);
        self.write16(addr.wrapping_add(2), (val >> 16) as u16);
    }

    /// ページ内に収まる2/4バイト書き込みを**1回の変換**で行う。
    /// 成功したら true。フォールト・跨ぎ・見張り対象などで速い道を使えないときは
    /// false を返し、呼び出し側がバイトごとの道へ落とす
    fn write_wide(&mut self, addr: u32, val: u32, width: u32) -> bool {
        let pa = match self.translate_for(addr, true) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return true; // フォールトは「書かない」で完了 (再実行が改めて書く)
            }
        };
        let a = pa as usize;
        if a + width as usize > self.mem.len() {
            return true; // RAM超えは捨てる (完了扱い)
        }
        // デバッガが見張っている、または VRAM に落ちるなら、遅い道で
        // バイトごとの合図を出す (ここは熱くない)
        if self.dbg.on || (bus::VRAM_TEXT_BASE..=bus::VRAM_TEXT_END).contains(&(a as u32)) {
            return false;
        }
        for i in 0..width as usize {
            self.mem[a + i] = (val >> (i * 8)) as u8;
        }
        // ページ内に収まる書き込み (呼び手が保証) なので申告は先頭1回でよい
        self.dcache.note_write(a as u32);
        true
    }

    // --- テキストVRAM ---

    /// テキスト画面の生バイト列 (80×25、文字と属性が交互)。
    ///
    /// **先頭から4000バイトではなく、CRTCが指す位置から4000バイトを返す。**
    ///
    /// テキストVRAMの窓は32KBあり、80x25の1画面はそのうち4000バイトでしかない。
    /// どこから表示するかを決めるのはCRTCのレジスタ 0x0C/0x0D で、ここを動かすと
    /// **メモリを1バイトも書き換えずに画面をスクロールできる** (ハードウェアスクロール)。
    /// 80年代の機械が遅いCPUで滑らかにスクロールできたのはこの仕組みによる。
    ///
    /// これを見ずに常に先頭を返していたため、CGA向けにハードウェアスクロールで
    /// 描くソフト (zmiy など) は**画面の下が永久に出てこなかった**。
    /// CRTCは実装してあり、説明にも「ここを動かすとスクロールできる」と
    /// 書いてあったのに、**描く側が見ていなかった**。
    pub fn text_vram(&self) -> &[u8] {
        let win = (bus::VRAM_TEXT_END - bus::VRAM_TEXT_BASE + 1) as usize;
        // 開始位置は文字単位。1文字2バイトなので倍にする
        let start = (self.devices.crtc.start_offset() as usize * bus::TEXT_CELL) % win;
        let b = bus::VRAM_TEXT_BASE as usize + start;
        // 窓の端をまたぐ場合は、素直に先頭を返す (実機は巻き戻るが、
        // そこまで使うソフトは見ていない。使うものが出てきたら組み立てる)
        if start + bus::TEXT_LEN <= win {
            &self.mem[b..b + bus::TEXT_LEN]
        } else {
            let base = bus::VRAM_TEXT_BASE as usize;
            &self.mem[base..base + bus::TEXT_LEN]
        }
    }
}
