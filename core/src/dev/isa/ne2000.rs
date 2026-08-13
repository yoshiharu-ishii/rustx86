//! NE2000 (DP8390) — ISAのEthernetカード。
//!
//! Novellが1987年に出した安物カードが、あまりに安くて全メーカーがレジスタ配置
//! ごと真似た結果、**「NE2000互換」がISA時代のネットワークの共通語**になった。
//! ELKSのne2kドライバも、DOSのパケットドライバ (NE2000.COM) も、そして
//! PCI時代のRTL8029すらこの配置を引きずっている — ここで作る8390コアは
//! PCI段でそのまま皮を替えて使い回す ([ADR-0017](../../../../docs/adr/0017-network-isa-first.md))。
//!
//! ## 構造は「共有メモリ + 窓口」
//!
//! カードは16KBのSRAM (アドレス 0x4000-0x7FFF) を持ち、CPUとはリモートDMA
//! という名の窓口 (データポート) 越しにやり取りする。DMAと言っても
//! バスマスタではなく、**CPUが IN/OUT を叩くたびに1バイトずつ進む**だけの
//! アドレスカウンタである。
//!
//! - 送信: ドライバがフレームをリモートDMAでSRAMへ書き、CRのTXPを立てる
//! - 受信: カードがSRAM内のリング (PSTART-PSTOP) にフレームを積み、
//!   4バイトのヘッダ (状態・次ページ・長さ) を先頭に付けて CURR を進める。
//!   ドライバは BNRY と CURR の差分を読んで追いかける
//!
//! ## 境界の流儀
//!
//! フレームの出入りはUARTのバイト列と同じ形にする: 外から来たフレームは
//! [`Ne2000::inject_frame`] でリングに積まれ、ゲストが送ったフレームは
//! [`Ne2000::tx_out`] に溜まって外側 (WebSocket等の非決定な世界) が回収する。
//! coreは時計もソケットも知らないので、**NICを繋いでも決定性は壊れない**
//! (同じフレーム列を同じタイミングで入れれば同じ実行になる)。

use std::collections::VecDeque;

/// SRAMの先頭 (リモートDMAのアドレス空間で)。ここより下はPROM
const RAM_START: usize = 0x4000;
/// SRAMの末尾 (排他)
const RAM_END: usize = 0x8000;
/// 1ページ = 256バイト。リングの通貨単位
const PAGE: usize = 256;

// CR (コマンドレジスタ) のビット
const CR_STP: u8 = 0x01; // 停止
const CR_STA: u8 = 0x02; // 開始
const CR_TXP: u8 = 0x04; // 送信要求

// ISR / IMR のビット (同じ並び)
const ISR_PRX: u8 = 0x01; // 受信完了
const ISR_PTX: u8 = 0x02; // 送信完了
const ISR_OVW: u8 = 0x10; // リング溢れ
const ISR_RDC: u8 = 0x40; // リモートDMA完了
const ISR_RST: u8 = 0x80; // リセット済み

#[derive(Debug)]
pub struct Ne2000 {
    /// このカードのMACアドレス
    pub mac: [u8; 6],
    cr: u8,
    isr: u8,
    imr: u8,
    dcr: u8,
    tcr: u8,
    rcr: u8,
    /// リモートDMAのアドレスカウンタと残量
    rsar: u16,
    rbcr: u16,
    /// 送信バッファの位置と長さ
    tpsr: u8,
    tbcr: u16,
    /// 受信リングの区画と現在位置
    pstart: u8,
    pstop: u8,
    bnry: u8,
    curr: u8,
    /// ドライバが書き込むMAC (PAR)。PROMの写しとは別に持つのが実機の作法
    par: [u8; 6],
    mar: [u8; 8],
    /// SRAM (0x4000-0x7FFF) + 下位に読み出し専用のPROMを重ねた32KB
    mem: Vec<u8>,
    /// ゲストが送信したフレーム。外側 (トランスポート) が回収する
    pub tx_out: VecDeque<Vec<u8>>,
    /// デバッグ用I/Oトレース (一時計測。1 = 書き込み<<31 | off<<16 | 値)
    pub trace: Vec<u32>,
}

impl Ne2000 {
    pub fn new(mac: [u8; 6]) -> Self {
        let mut mem = vec![0u8; RAM_END];
        // PROM: MACの各バイトを2度ずつ (16bitカードは偶数バイトしか読まないため
        // 倍幅で置くのが慣例)。末尾の 0x57 'W' はドライバのNE2000判定の印
        for (i, b) in mac.iter().enumerate() {
            mem[i * 2] = *b;
            mem[i * 2 + 1] = *b;
        }
        for i in [14, 15, 28, 29, 30, 31] {
            mem[i] = 0x57;
        }
        Self {
            mac,
            cr: CR_STP, // 電源投入時は停止
            isr: ISR_RST,
            imr: 0,
            dcr: 0,
            tcr: 0,
            rcr: 0,
            rsar: 0,
            rbcr: 0,
            tpsr: 0,
            tbcr: 0,
            pstart: 0,
            pstop: 0,
            bnry: 0,
            curr: 0,
            par: [0; 6],
            mar: [0; 8],
            mem,
            tx_out: VecDeque::new(),
            trace: Vec::new(),
        }
    }

    /// 割り込みを上げるべきか。ISRとIMRの重なりがある間は上げ続ける
    /// (レベルトリガの流儀。tick_devices が毎回見るのでこれで足りる)
    pub fn irq_pending(&self) -> bool {
        self.isr & self.imr & 0x7F != 0
    }

    /// I/O読み出し。offはカードのベース (0x300) からのオフセット
    pub fn read(&mut self, off: u16) -> u8 {
        let v = self.read_inner(off);
        if self.trace.len() < 4096 {
            self.trace.push((off as u32) << 16 | v as u32);
        }
        v
    }

    fn read_inner(&mut self, off: u16) -> u8 {
        match off {
            0x00 => self.cr,
            0x10..=0x17 => self.dma_read(),
            0x18..=0x1F => {
                // リセットポート。読むとカードがリセットされ、ISRのRSTが立つ。
                // ドライバは「リセットを掛けてRSTが立つか」で存在確認をする
                self.isr |= ISR_RST;
                self.cr = (self.cr & !CR_STA) | CR_STP;
                0
            }
            _ => match (self.cr >> 6, off) {
                // ページ0: 実行状態
                (0, 0x03) => self.bnry,
                (0, 0x04) => 0x01, // TSR: 送信は常に成功する世界
                (0, 0x07) => self.isr,
                (0, 0x0D..=0x0F) => 0, // カウンタ (エラー数)。エラーは起きない
                // ページ1: アドレス設定
                (1, 0x01..=0x06) => self.par[off as usize - 1],
                (1, 0x07) => self.curr,
                (1, 0x08..=0x0F) => self.mar[off as usize - 8],
                _ => 0,
            },
        }
    }

    /// I/O書き込み
    pub fn write(&mut self, off: u16, val: u8) {
        if self.trace.len() < 4096 {
            self.trace.push(1 << 31 | (off as u32) << 16 | val as u32);
        }
        match off {
            0x00 => {
                // TXPは「実行した」ら消える1回きりのビット。送信は瞬時に済む
                // 世界なので、立てられた場でフレームを取り出して下ろす
                self.cr = val & !CR_TXP;
                // ISRのRSTは「停止中」の印: STOPで立ち、**STARTで自動的に下りる**
                // (データシートの仕様)。ここを忘れるとELKSのISRハンドラが
                // 「誰も処理しない0x80」を延々読み続けて無限ループする (実話。
                // Crynwrは行儀よくISRに0xFFを書いて掃除するので気づけなかった)
                if val & CR_STA != 0 {
                    self.isr &= !ISR_RST;
                } else if val & CR_STP != 0 {
                    self.isr |= ISR_RST;
                }
                if val & CR_TXP != 0 {
                    self.transmit();
                }
            }
            0x10..=0x17 => self.dma_write(val),
            0x18..=0x1F => {
                self.isr |= ISR_RST;
            }
            _ => match (self.cr >> 6, off) {
                (0, 0x01) => self.pstart = val,
                (0, 0x02) => self.pstop = val,
                (0, 0x03) => self.bnry = val,
                (0, 0x04) => self.tpsr = val,
                (0, 0x05) => self.tbcr = (self.tbcr & 0xFF00) | val as u16,
                (0, 0x06) => self.tbcr = (self.tbcr & 0x00FF) | (val as u16) << 8,
                // ISRは「1を書いたビットが下りる」— 割り込みの受領確認
                (0, 0x07) => self.isr &= !val,
                (0, 0x08) => self.rsar = (self.rsar & 0xFF00) | val as u16,
                (0, 0x09) => self.rsar = (self.rsar & 0x00FF) | (val as u16) << 8,
                (0, 0x0A) => self.rbcr = (self.rbcr & 0xFF00) | val as u16,
                (0, 0x0B) => self.rbcr = (self.rbcr & 0x00FF) | (val as u16) << 8,
                (0, 0x0C) => self.rcr = val,
                (0, 0x0D) => self.tcr = val,
                (0, 0x0E) => self.dcr = val,
                (0, 0x0F) => self.imr = val,
                (1, 0x01..=0x06) => self.par[off as usize - 1] = val,
                (1, 0x07) => self.curr = val,
                (1, 0x08..=0x0F) => self.mar[off as usize - 8] = val,
                _ => {}
            },
        }
    }

    /// データポート読み: リモートDMAが1バイト進む。
    /// 16bit転送 (DCRのWTS) でも、io_read16 は連続2ポートの読みに分解される
    /// ので、ここは常に1バイトずつでよい — 順序も数も同じになる
    fn dma_read(&mut self) -> u8 {
        let v = *self.mem.get(self.rsar as usize).unwrap_or(&0xFF);
        self.rsar = self.rsar.wrapping_add(1);
        self.rbcr = self.rbcr.saturating_sub(1);
        if self.rbcr == 0 {
            self.isr |= ISR_RDC;
        }
        v
    }

    fn dma_write(&mut self, val: u8) {
        let a = self.rsar as usize;
        if (RAM_START..RAM_END).contains(&a) {
            self.mem[a] = val; // PROM側 (0x4000未満) への書き込みは無視
        }
        self.rsar = self.rsar.wrapping_add(1);
        self.rbcr = self.rbcr.saturating_sub(1);
        if self.rbcr == 0 {
            self.isr |= ISR_RDC;
        }
    }

    /// CRのTXP: SRAMのTPSRページからTBCRバイトを取り出して送信する
    fn transmit(&mut self) {
        let start = self.tpsr as usize * PAGE;
        let len = self.tbcr as usize;
        if start >= RAM_START && start + len <= RAM_END && len > 0 {
            self.tx_out.push_back(self.mem[start..start + len].to_vec());
        }
        self.isr |= ISR_PTX;
    }

    /// 外から来たフレームを受信リングへ積む。積めたら true。
    ///
    /// 4バイトのヘッダ (受信状態・次ページ・長さ) を付け、PSTART-PSTOP の
    /// リングにページ単位で書く。**リングが一杯なら落として OVW を立てる** —
    /// Ethernetはもともと「届かないことがある」層なので、落ちても上位
    /// (TCPや再送) が拾い直す。それで済むから安いカードが作れた
    pub fn inject_frame(&mut self, frame: &[u8]) -> bool {
        // 受信機が動いていない (STP中・リング未設定) なら黙って落とす
        if self.cr & CR_STA == 0 || self.pstart >= self.pstop {
            return false;
        }
        // 宛先の選別。自分宛・ブロードキャスト・(RCRの無差別ビット) だけ通す。
        // マルチキャストのハッシュ表 (MAR) は台帳 — 要るゲストが来たら足す
        if self.rcr & 0x10 == 0 {
            let dst = frame.get(0..6).unwrap_or(&[]);
            if dst != self.par && dst != [0xFF; 6] {
                return false;
            }
        }
        // 実機は60バイト未満を受け取らない (コリジョンの破片と区別できない)。
        // 短いフレームはパディングして通す — wsslirpのARP応答は42バイトで来る
        let len = frame.len().max(60);
        let total = 4 + len;
        let pages_needed = total.div_ceil(PAGE) as u8;
        let ring_pages = self.pstop - self.pstart;
        // 空きページ数: CURRからBNRYの手前まで (境界ページは使わないのが作法)
        let used = (self.curr + ring_pages - self.bnry) % ring_pages;
        let free = ring_pages - used;
        if pages_needed + 1 > free {
            self.isr |= ISR_OVW;
            return false;
        }
        let next = {
            let n = self.curr + pages_needed;
            if n >= self.pstop {
                self.pstart + (n - self.pstop)
            } else {
                n
            }
        };
        // ヘッダ + フレーム本体をリングへ (PSTOPで巻き戻す)
        let header = [
            0x01, // 受信状態: 正常
            next,
            (total & 0xFF) as u8,
            (total >> 8) as u8,
        ];
        let mut addr = self.curr as usize * PAGE;
        let put = |mem: &mut [u8], addr: &mut usize, b: u8| {
            mem[*addr] = b;
            *addr += 1;
            if *addr >= self.pstop as usize * PAGE {
                *addr = self.pstart as usize * PAGE;
            }
        };
        for b in header {
            put(&mut self.mem, &mut addr, b);
        }
        for i in 0..len {
            put(&mut self.mem, &mut addr, frame.get(i).copied().unwrap_or(0));
        }
        self.curr = next;
        self.isr |= ISR_PRX;
        true
    }
}

impl Ne2000 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.bytes(&self.mac);
        for v in [
            self.cr,
            self.isr,
            self.imr,
            self.dcr,
            self.tcr,
            self.rcr,
            self.tpsr,
            self.pstart,
            self.pstop,
            self.bnry,
            self.curr,
        ] {
            w.u8(v);
        }
        w.u16(self.rsar);
        w.u16(self.rbcr);
        w.u16(self.tbcr);
        w.bytes(&self.par);
        w.bytes(&self.mar);
        w.rle(&self.mem);
        // tx_out は「まだ回収されていない外向きフレーム」なので状態に含める
        w.u32(self.tx_out.len() as u32);
        for f in &self.tx_out {
            w.bytes(f);
        }
    }

    pub fn load(r: &mut crate::snapshot::Reader) -> Result<Self, String> {
        let mac: [u8; 6] = r
            .bytes()?
            .try_into()
            .map_err(|_| "NE2000のMACが6バイトでない".to_string())?;
        let mut n = Self::new(mac);
        for v in [
            &mut n.cr,
            &mut n.isr,
            &mut n.imr,
            &mut n.dcr,
            &mut n.tcr,
            &mut n.rcr,
            &mut n.tpsr,
            &mut n.pstart,
            &mut n.pstop,
            &mut n.bnry,
            &mut n.curr,
        ] {
            *v = r.u8()?;
        }
        n.rsar = r.u16()?;
        n.rbcr = r.u16()?;
        n.tbcr = r.u16()?;
        n.par = r
            .bytes()?
            .try_into()
            .map_err(|_| "NE2000のPARが6バイトでない".to_string())?;
        n.mar = r
            .bytes()?
            .try_into()
            .map_err(|_| "NE2000のMARが8バイトでない".to_string())?;
        n.mem = r.rle()?;
        if n.mem.len() != RAM_END {
            return Err(format!("NE2000のSRAMの大きさが合わない ({})", n.mem.len()));
        }
        let frames = r.u32()?;
        for _ in 0..frames {
            n.tx_out.push_back(r.bytes()?);
        }
        Ok(n)
    }
}
