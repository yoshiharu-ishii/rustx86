//! DP8390 (NIC) — Ethernetコントローラの**素子**。
//!
//! Nationalの8390は、Novellが1987年に出した安物カード NE2000 に載って
//! ISA時代のネットワークの共通語になった。あまりに安くて全メーカーがレジスタ配置
//! ごと真似た結果、ELKSのne2kドライバも、DOSのパケットドライバ (NE2000.COM) も、
//! そしてPCI時代のRTL8029すらこの配置を引きずっている。
//!
//! **このファイルは素子だけを持つ。** どの番地の窓から覗かれるか、MACの並びを
//! PROMにどう置くか、設定空間でどう名乗るかは**基板の都合**なので
//! [`card`](crate::dev::card) 側にある — 同じ素子に別の基板を着せたのが
//! NE2000 (ISA) と RTL8029 (PCI) で、実物のRTL8029ASも「NE2000互換であること」を
//! 売りにした廉価チップだった。Linuxのドライバも `lib8390.c` を共有している。
//!
//! ## 構造は「共有メモリ + 窓口」
//!
//! カードは16KBのSRAM (アドレス 0x4000-0x7FFF) を持ち、CPUとはリモートDMA
//! という名の窓口 (データポート) 越しにやり取りする。DMAと言っても
//! バスマスタではなく、**CPUが IN/OUT を叩くたびに1バイトずつ進む**だけの
//! アドレスカウンタである。その下 (0x4000未満) に基板のPROMが重なって見える。
//!
//! - 送信: ドライバがフレームをリモートDMAでSRAMへ書き、CRのTXPを立てる
//! - 受信: カードがSRAM内のリング (PSTART-PSTOP) にフレームを積み、
//!   4バイトのヘッダ (状態・次ページ・長さ) を先頭に付けて CURR を進める。
//!   ドライバは BNRY と CURR の差分を読んで追いかける
//!
//! ## 境界の流儀
//!
//! フレームの出入りはUARTのバイト列と同じ形にする: 外から来たフレームは
//! [`Dp8390::inject_frame`] で線に並び、ゲストが送ったフレームは
//! [`Dp8390::tx_out`] に溜まって外側 (WebSocket等の非決定な世界) が回収する。
//! coreは時計もソケットも知らないので、**NICを繋いでも決定性は壊れない**
//! (同じフレーム列を同じタイミングで入れれば同じ実行になる)。

use std::collections::VecDeque;

/// SRAMの先頭 (リモートDMAのアドレス空間で)。ここより下はPROM
const RAM_START: usize = 0x4000;
/// SRAMの末尾 (排他)
const RAM_END: usize = 0x8000;
/// 1ページ = 256バイト。リングの通貨単位
const PAGE: usize = 256;
/// 線の上で待たせておけるフレーム数。**ここを超えたら本当に落とす** —
/// 受け取り手が居ない (ドライバが止まっている・遅すぎる) のに溜め続けると、
/// 遅延だけが伸びて誰も得をしない。実機の線とスイッチのバッファも有限である
const RX_QUEUE_MAX: usize = 256;

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
pub struct Dp8390 {
    /// このカードのMACアドレス
    pub mac: [u8; 6],
    cr: u8,
    /// 受信機が回っているか。**STA/STPは状態ではなくコマンド**で、
    /// どちらも立てずにCRを書いてもこの状態は変わらない。Linuxの8390.cは
    /// ページ切替のたびに 0x20 (素のNODMA) を書くので、crの生値で判定すると
    /// 割り込みのたびに受信機が止まって見える (実際にARPを取りこぼした)
    running: bool,
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
    /// **まだリングに入れていない受信フレーム = 線の上に居るぶん。**
    ///
    /// 外の世界 (WebSocket) はフレームを**束で**届ける — TCPの1ウィンドウが
    /// 一度に落ちてくる。ところが受信リングは16KBしかなく、フル長フレームなら
    /// 9枚で満杯になる。届いた束をその場で全部リングへ押し込もうとすると、
    /// 入らなかった分がそこで消える (実測で受信フレームの6〜9%が消え、
    /// TCPは再送と輻輳制御の縮小で応じて実効速度が半分以下になった)。
    ///
    /// 実機では束は消えない — 10Mbpsの線を1枚ずつ流れてくるので、
    /// カードは自分のペースで受け取れる。**その「線」をここで表す**。
    /// リングに空きができるたび (tick ごと) に前から詰めていく
    rx_queue: VecDeque<Vec<u8>>,
    /// デバッグ用I/Oトレース (一時計測。1 = 書き込み<<31 | off<<16 | 値)
    pub trace: Vec<u32>,
}

impl Dp8390 {
    /// 素子を1つ。**PROMは空のまま** — MACをどう並べ、どこに 'W' の印を置くかは
    /// 基板ごとに違う ([`crate::dev::card`] が [`write_prom`](Self::write_prom) で書く)
    pub fn new(mac: [u8; 6]) -> Self {
        let mem = vec![0u8; RAM_END];
        Self {
            mac,
            cr: CR_STP, // 電源投入時は停止
            running: false,
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
            rx_queue: VecDeque::new(),
            trace: Vec::new(),
        }
    }

    /// PROM (リモートDMAのアドレス0から見える領域) を基板が書く。
    ///
    /// 素子から見ればここはただの読み出し専用の窓で、**中身の並べ方は基板の都合**
    /// である。ISAの8bit経路では各バイトが2度ずつ並び、PCI版は連続バイトで読む —
    /// 倍幅のまま渡すとMACが `52:52:54:…` に化ける (実際に化けた)
    pub fn write_prom(&mut self, prom: &[u8; 32]) {
        self.mem[..32].copy_from_slice(prom);
    }

    /// 基板が焼いたPROM (基板側のテストが自分の並べ方を確かめるための読み口)
    pub fn prom(&self) -> &[u8] {
        &self.mem[..32]
    }

    /// 詰まりを覗く窓 — 線の待ち枚数・ISR/IMR・CURR/BNRY・受信機の状態。
    ///
    /// 受信が止まったとき、原因は必ずこの数字の組み合わせに出る
    /// (「リング満杯 + ISR=0 + 線に行列」= 誰も割り込みを上げていない、など)。
    /// 外から見えないと当てずっぽうになるので、道具として残してある
    /// (tools/webtest/netbench.mjs の NICDBG=1 が使う)
    pub fn debug_state(&self) -> String {
        format!(
            "q={} isr={:02x} imr={:02x} curr={:02x} bnry={:02x} pstart={:02x} pstop={:02x} run={}",
            self.rx_queue.len(),
            self.isr,
            self.imr,
            self.curr,
            self.bnry,
            self.pstart,
            self.pstop,
            self.running
        )
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
                self.running = false;
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
                    self.running = true;
                } else if val & CR_STP != 0 {
                    self.isr |= ISR_RST;
                    self.running = false;
                }
                if val & CR_TXP != 0 {
                    self.transmit();
                }
            }
            0x10..=0x17 => self.dma_write(val),
            0x18..=0x1F => {
                self.isr |= ISR_RST;
                self.running = false;
            }
            _ => match (self.cr >> 6, off) {
                (0, 0x01) => self.pstart = val,
                (0, 0x02) => self.pstop = val,
                // BNRYを進める = ドライバが1枚読み終えてページを返した瞬間。
                // **リングに空きができるのはここ**なので、線で待っている次の
                // フレームをすぐ入れる (待たせると次の割り込みまで遅れる)
                (0, 0x03) => {
                    self.bnry = val;
                    self.drain_rx();
                }
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

    /// 外から来たフレームを受け取る。**受け取れたら true**。
    ///
    /// 受け取ったフレームはまず線 ([`rx_queue`](Self::rx_queue)) に並び、
    /// 入るぶんだけリングへ移る。false になるのは「カードが止まっている」
    /// 「宛先が自分でない」「線も一杯」の3つだけで、**リングが満杯なだけなら
    /// 落とさない** — 束で届くのは外の世界の都合で、線の上ではまだ流れている
    pub fn inject_frame(&mut self, frame: &[u8]) -> bool {
        // 受信機が動いていない (STP中・リング未設定) なら黙って落とす。
        // 線に溜めても意味が無い — 電源の入っていないカードの前に届いた
        // フレームは実機でも消える
        if !self.running || self.pstart >= self.pstop {
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
        // 線も一杯 = 本当の取りこぼし。カードの受信オーバーランとして
        // ドライバに知らせる (8390のOVW。回復手順はドライバが持っている)
        if self.rx_queue.len() >= RX_QUEUE_MAX {
            self.isr |= ISR_OVW;
            return false;
        }
        self.rx_queue.push_back(frame.to_vec());
        self.drain_rx();
        true
    }

    /// 線で待っているフレームを、入るだけリングへ移す。
    /// **リングに空きができる契機 (tick) ごとに呼ぶ** — ドライバがBNRYを
    /// 進めた直後に次が入るので、束で届いても取りこぼしが出ない
    pub fn drain_rx(&mut self) {
        while let Some(frame) = self.rx_queue.pop_front() {
            if !self.ring_put(&frame) {
                self.rx_queue.push_front(frame);
                // **リングが満杯 = 読まれていないフレームがリングに居る。**
                // その合図 (PRX) を立て直す。下ろすのはドライバの仕事で、
                // 下ろした後もまだ残っていれば次のtickでまた立つ。
                //
                // 立て直さないと機械が止まる (実際に止まった): ドライバは
                // 1回の割り込みで読む枚数に上限を持っていて、上限で降りると
                // リングに未読が残ったままISRを下ろす。そこへ新しいフレームが
                // 入れない (満杯) と、二度と割り込みが上がらない —
                // リングは満杯・線には行列・ゲストはHLTで永眠、という三すくみ
                if self.running {
                    self.isr |= ISR_PRX;
                }
                break;
            }
        }
    }

    /// 受信リングへ1枚積む。積めなければ false (状態は変えない)。
    ///
    /// 4バイトのヘッダ (受信状態・次ページ・長さ) を付け、PSTART-PSTOP の
    /// リングにページ単位で書く
    fn ring_put(&mut self, frame: &[u8]) -> bool {
        if !self.running || self.pstart >= self.pstop {
            return false;
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

impl Dp8390 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.bytes(&self.mac);
        w.bool(self.running);
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
            .map_err(|_| "DP8390のMACが6バイトでない".to_string())?;
        let mut n = Self::new(mac);
        n.running = r.bool()?;
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
            .map_err(|_| "DP8390のPARが6バイトでない".to_string())?;
        n.mar = r
            .bytes()?
            .try_into()
            .map_err(|_| "DP8390のMARが8バイトでない".to_string())?;
        n.mem = r.rle()?;
        if n.mem.len() != RAM_END {
            return Err(format!("DP8390のSRAMの大きさが合わない ({})", n.mem.len()));
        }
        let frames = r.u32()?;
        for _ in 0..frames {
            n.tx_out.push_back(r.bytes()?);
        }
        Ok(n)
    }
}
