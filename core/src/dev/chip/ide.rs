//! IDE (legacy ATA バス) の secondary チャネル + ATAPI の CD-ROM (6c の 2 段目)。
//!
//! ポート 0x170-0x177 (コマンドブロック) と 0x376 (制御ブロック)、IRQ15。
//! master に CD-ROM が 1 台、slave は空。Linux は `pata_legacy` がこの番地を叩き、
//! `sr_mod` が `/dev/sr0` を生やす。BIOS の CD (INT 13h ドライブ 0xE0、El Torito) は
//! 別の高位エミュレーションで、像 (`Machine.cd`) は両者で共有する — この素子は像を
//! **持たない**。READ の要求 (LBA と数) を返し、詰めるのは Machine の仕事
//! (像を 2 つ持たない、という一点のため)。
//!
//! ## 作法
//!
//! ATAPI は ATA の上に SCSI のパケット (12 バイト) を乗せた規格である。流れは:
//!
//! 1. ホストが byte count (LBA mid/high) にデータの上限を書き、コマンド 0xA0 (PACKET)
//! 2. 素子が DRQ を立て、interrupt reason = CoD (コマンド待ち) — ホストがデータポートへ
//!    12 バイトのパケットを 16bit × 6 回で書く
//! 3. 素子がパケットを解釈。データがあれば DRQ + IO でホストに読ませ (上限ごとに
//!    区切って、区切りごとに IRQ)、無ければ完了 (CoD|IO) で IRQ
//! 4. ホストが status を読むと IRQ が下りる
//!
//! DMA は無い (pata_legacy は PIO しか使わない)。IDENTIFY PACKET DEVICE の DRQ 種別は
//! 「マイクロプロセッサ DRQ」(bit 6:5 = 00) にしてある — パケット受付に割り込みを
//! 待たず、ホストが DRQ をポーリングする形。libata はどちらも扱えるが、こちらが単純
//!
//! 実装しているパケット: TEST UNIT READY / REQUEST SENSE / INQUIRY / MODE SENSE(10) の
//! 0x2A / READ CAPACITY / READ(10) / READ(12) / READ CD (user data) / READ TOC /
//! GET CONFIGURATION / GET EVENT STATUS / START STOP / PREVENT ALLOW / READ SUBCHANNEL /
//! MECHANISM STATUS。知らないものは ILLEGAL REQUEST で断る (ホストは諦めて進む)

/// コマンドブロックの先頭 (secondary)
pub const BASE: u16 = 0x170;
/// 制御ブロック (alternate status / device control)
pub const CTRL: u16 = 0x376;
/// secondary チャネルの割り込み線 (スレーブ PIC の 7 番)
pub const IRQ: u8 = 15;

const SECTOR: usize = 2048;

// status
const ST_BSY: u8 = 0x80;
const ST_DRDY: u8 = 0x40;
const ST_DSC: u8 = 0x10;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;
// interrupt reason (sector count レジスタ)
const IR_COD: u8 = 0x01;
const IR_IO: u8 = 0x02;
// error
const ERR_ABRT: u8 = 0x04;
// device control
const CTL_NIEN: u8 = 0x02;
const CTL_SRST: u8 = 0x04;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// 何もしていない
    Idle,
    /// PACKET を受けて、12 バイトのパケットを待っている
    PacketWait,
    /// ホストへデータを渡している (buf[pos..block_end] が今の DRQ ブロック)
    DataIn,
}

/// Machine に頼む仕事 (像はこちらに無い)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// 像の `lba` から `count` セクタ (2048B) を読んで [`Ide::data_ready`] で渡してほしい
    Read { lba: u32, count: u32 },
}

pub struct Ide {
    error: u8,
    features: u8,
    /// ATAPI では interrupt reason (CoD / IO)
    sector_count: u8,
    lba_lo: u8,
    lba_mid: u8,
    lba_hi: u8,
    drive: u8,
    status: u8,
    control: u8,
    phase: Phase,
    packet: [u8; 12],
    packet_len: usize,
    buf: Vec<u8>,
    pos: usize,
    block_end: usize,
    /// ホストが PACKET のときに申告した 1 ブロックの上限 (バイト)
    limit: usize,
    /// 直近の sense (key, asc, ascq)
    sense: (u8, u8, u8),
    /// 像の大きさ (2048B セクタ数)
    sectors: u32,
    /// 割り込みの挙手。status を読むと下りる
    pub irq_pending: bool,
    /// ゲストが叩いた回数 (覗き窓用)
    pub commands: u32,
}

impl Ide {
    /// `sectors` = 像の 2048B セクタ数
    pub fn new(sectors: u32) -> Self {
        let mut d = Ide {
            error: 0,
            features: 0,
            sector_count: 0,
            lba_lo: 0,
            lba_mid: 0,
            lba_hi: 0,
            drive: 0,
            status: 0,
            control: 0,
            phase: Phase::Idle,
            packet: [0; 12],
            packet_len: 0,
            buf: Vec::new(),
            pos: 0,
            block_end: 0,
            limit: 0xFFFE,
            sense: (0, 0, 0),
            sectors,
            irq_pending: false,
            commands: 0,
        };
        d.reset();
        d
    }

    /// 控えに書く (像そのものは入れない — 大きさだけ。復元側が同じ像を挿し直す)
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        for v in [
            self.error,
            self.features,
            self.sector_count,
            self.lba_lo,
            self.lba_mid,
            self.lba_hi,
            self.drive,
            self.status,
            self.control,
        ] {
            w.u8(v);
        }
        w.u8(match self.phase {
            Phase::Idle => 0,
            Phase::PacketWait => 1,
            Phase::DataIn => 2,
        });
        w.bytes(&self.packet);
        w.u32(self.packet_len as u32);
        w.bytes(&self.buf);
        w.u32(self.pos as u32);
        w.u32(self.block_end as u32);
        w.u32(self.limit as u32);
        w.u8(self.sense.0);
        w.u8(self.sense.1);
        w.u8(self.sense.2);
        w.u32(self.sectors);
        w.bool(self.irq_pending);
        w.u32(self.commands);
    }

    pub fn load(r: &mut crate::snapshot::Reader) -> Result<Self, String> {
        let mut d = Ide::new(0);
        d.error = r.u8()?;
        d.features = r.u8()?;
        d.sector_count = r.u8()?;
        d.lba_lo = r.u8()?;
        d.lba_mid = r.u8()?;
        d.lba_hi = r.u8()?;
        d.drive = r.u8()?;
        d.status = r.u8()?;
        d.control = r.u8()?;
        d.phase = match r.u8()? {
            0 => Phase::Idle,
            1 => Phase::PacketWait,
            2 => Phase::DataIn,
            p => return Err(format!("IDE の段が不正 ({p})")),
        };
        let packet = r.bytes()?;
        if packet.len() != 12 {
            return Err(format!("IDE のパケット長が不正 ({})", packet.len()));
        }
        d.packet.copy_from_slice(&packet);
        d.packet_len = r.u32()? as usize;
        d.buf = r.bytes()?;
        d.pos = r.u32()? as usize;
        d.block_end = r.u32()? as usize;
        d.limit = r.u32()? as usize;
        d.sense = (r.u8()?, r.u8()?, r.u8()?);
        d.sectors = r.u32()?;
        d.irq_pending = r.bool()?;
        d.commands = r.u32()?;
        if d.block_end > d.buf.len() || d.pos > d.block_end {
            return Err("IDE の DRQ ブロックがバッファからはみ出る".into());
        }
        Ok(d)
    }

    /// 像を差し替えたとき (大きさだけ知ればよい)
    pub fn set_sectors(&mut self, sectors: u32) {
        self.sectors = sectors;
    }

    /// リセット: ATAPI の署名 (0x14EB) を置く。ホストはこれで「パケット装置だ」と知る
    fn reset(&mut self) {
        self.error = 0x01;
        self.sector_count = 0x01;
        self.lba_lo = 0x01;
        self.lba_mid = 0x14;
        self.lba_hi = 0xEB;
        self.status = ST_DRDY | ST_DSC;
        self.phase = Phase::Idle;
        self.buf.clear();
        self.pos = 0;
        self.block_end = 0;
        self.irq_pending = false;
    }

    /// master (CD) が選ばれているか。slave は空なので 0 を返す
    fn master(&self) -> bool {
        self.drive & 0x10 == 0
    }

    fn raise_irq(&mut self) {
        if self.control & CTL_NIEN == 0 {
            self.irq_pending = true;
        }
    }

    // ---- レジスタ ----

    /// 8bit の読み出し (データポート以外)
    pub fn read8(&mut self, port: u16) -> u8 {
        if port == CTRL {
            return if self.master() { self.status } else { 0 };
        }
        if !self.master() {
            return 0;
        }
        match port.wrapping_sub(BASE) {
            0 => 0, // データポートの 8bit 読みは使わない (16bit 経路が本道)
            1 => self.error,
            2 => self.sector_count,
            3 => self.lba_lo,
            4 => self.lba_mid,
            5 => self.lba_hi,
            6 => self.drive,
            7 => {
                // status を読むと割り込みが下りる (ATA の約束)
                self.irq_pending = false;
                self.status
            }
            _ => 0xFF,
        }
    }

    /// 8bit の書き込み
    pub fn write8(&mut self, port: u16, val: u8) {
        if port == CTRL {
            // SRST の立ち下がりでリセット
            let was = self.control & CTL_SRST != 0;
            self.control = val;
            if was && val & CTL_SRST == 0 {
                self.reset();
            } else if val & CTL_SRST != 0 {
                self.status = ST_BSY;
            }
            return;
        }
        match port.wrapping_sub(BASE) {
            1 => self.features = val,
            2 => self.sector_count = val,
            3 => self.lba_lo = val,
            4 => self.lba_mid = val,
            5 => self.lba_hi = val,
            6 => self.drive = val,
            7 if self.master() => self.command(val),
            _ => {}
        }
    }

    /// データポートの 16bit 読み (insw)
    pub fn read_data16(&mut self) -> u16 {
        if self.phase != Phase::DataIn || !self.master() {
            return 0;
        }
        let lo = self.buf.get(self.pos).copied().unwrap_or(0);
        let hi = self.buf.get(self.pos + 1).copied().unwrap_or(0);
        self.pos += 2;
        if self.pos >= self.block_end {
            if self.pos < self.buf.len() {
                self.next_block();
            } else {
                self.complete();
            }
        }
        u16::from_le_bytes([lo, hi])
    }

    /// データポートの 16bit 書き (outsw)。パケットが揃ったら解釈し、像の読みが
    /// 要るなら [`Request`] を返す (Machine が詰めて [`Self::data_ready`] を呼ぶ)
    pub fn write_data16(&mut self, val: u16) -> Option<Request> {
        if self.phase != Phase::PacketWait || !self.master() {
            return None;
        }
        let [lo, hi] = val.to_le_bytes();
        if self.packet_len < 12 {
            self.packet[self.packet_len] = lo;
            self.packet[self.packet_len + 1] = hi;
            self.packet_len += 2;
        }
        if self.packet_len < 12 {
            return None;
        }
        self.execute_packet()
    }

    /// Machine が像から読んだデータを渡す ([`Request::Read`] の返事)
    pub fn data_ready(&mut self, data: Vec<u8>) {
        self.send_data(data);
    }

    // ---- ATA コマンド ----

    fn command(&mut self, cmd: u8) {
        self.commands = self.commands.wrapping_add(1);
        self.error = 0;
        match cmd {
            // PACKET: パケットを待つ。byte count の上限を控える
            0xA0 => {
                let lim = self.lba_mid as usize | (self.lba_hi as usize) << 8;
                self.limit = match lim {
                    0 | 0xFFFF => 0xFFFE,
                    n => n & !1,
                };
                self.packet_len = 0;
                self.phase = Phase::PacketWait;
                self.sector_count = IR_COD;
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                // パケット受付の割り込みは出さない (マイクロプロセッサ DRQ)
            }
            // IDENTIFY PACKET DEVICE
            0xA1 => {
                let id = self.identify();
                self.send_data(id);
            }
            // IDENTIFY DEVICE: ATAPI は断って署名を置く (ホストはこれで PACKET 側へ)
            0xEC => {
                self.lba_mid = 0x14;
                self.lba_hi = 0xEB;
                self.abort();
            }
            // DEVICE RESET
            0x08 => {
                self.reset();
                self.status = ST_DRDY | ST_DSC;
            }
            // SET FEATURES / IDLE / STANDBY / FLUSH / CHECK POWER MODE: 受けるだけ
            0xEF | 0xE0 | 0xE1 | 0xE2 | 0xE3 | 0xE7 | 0xE5 => {
                if cmd == 0xE5 {
                    self.sector_count = 0xFF; // active
                }
                self.status = ST_DRDY | ST_DSC;
                self.phase = Phase::Idle;
                self.raise_irq();
            }
            _ => self.abort(),
        }
    }

    /// コマンドの拒否 (ABRT)
    fn abort(&mut self) {
        self.error = ERR_ABRT;
        self.status = ST_DRDY | ST_DSC | ST_ERR;
        self.phase = Phase::Idle;
        self.raise_irq();
    }

    fn identify(&self) -> Vec<u8> {
        let mut w = [0u16; 256];
        // word 0: ATAPI (bit15-14=10)、CD-ROM (bit12-8=00101)、removable (bit7)、
        // DRQ 種別 = マイクロプロセッサ (bit6-5=00)、パケット 12 バイト (bit1-0=00)
        w[0] = 0x8580;
        put_str(&mut w[10..20], b"RX86-CD-0001        ");
        put_str(&mut w[23..27], b"1.0     ");
        put_str(&mut w[27..47], b"RUSTX86 CD-ROM                          ");
        w[49] = 0x0200; // LBA 対応、DMA 無し
        w[53] = 0x0006; // word 64-70 / 88 が有効
        w[63] = 0x0000; // MWDMA 無し
        w[64] = 0x0003; // PIO 3/4
        w[65] = 120;
        w[66] = 120;
        w[67] = 120;
        w[68] = 120;
        w[80] = 0x001E; // ATA/ATAPI-1〜4 に対応
        w[82] = 0x4000; // NOP
        w[83] = 0x4000;
        w[84] = 0x4000;
        w[85] = 0x4000;
        w[86] = 0x0000;
        w[87] = 0x4000;
        w[88] = 0x0000; // UDMA 無し
        let mut out = Vec::with_capacity(512);
        for x in w {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out
    }

    // ---- データの受け渡し ----

    /// ホストへデータを渡し始める (空なら完了)
    fn send_data(&mut self, data: Vec<u8>) {
        if data.is_empty() {
            self.ok();
            return;
        }
        self.buf = data;
        self.pos = 0;
        self.next_block();
    }

    fn next_block(&mut self) {
        let remain = self.buf.len() - self.pos;
        let n = remain.min(self.limit.max(2));
        self.block_end = self.pos + n;
        self.lba_mid = n as u8;
        self.lba_hi = (n >> 8) as u8;
        self.sector_count = IR_IO;
        self.status = ST_DRDY | ST_DSC | ST_DRQ;
        self.phase = Phase::DataIn;
        self.raise_irq();
    }

    fn complete(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.block_end = 0;
        self.ok();
    }

    /// パケット完了 (データ無し / 転送済み)
    fn ok(&mut self) {
        self.sector_count = IR_COD | IR_IO;
        self.status = ST_DRDY | ST_DSC;
        self.phase = Phase::Idle;
        self.raise_irq();
    }

    /// CHECK CONDITION: sense を控えて断る
    fn check(&mut self, key: u8, asc: u8, ascq: u8) {
        self.sense = (key, asc, ascq);
        self.error = (key << 4) | ERR_ABRT;
        self.sector_count = IR_COD | IR_IO;
        self.status = ST_DRDY | ST_DSC | ST_ERR;
        self.phase = Phase::Idle;
        self.buf.clear();
        self.raise_irq();
    }

    // ---- SCSI パケット ----

    fn execute_packet(&mut self) -> Option<Request> {
        let p = self.packet;
        let alloc16 = u16::from_be_bytes([p[7], p[8]]) as usize;
        match p[0] {
            // TEST UNIT READY / START STOP / PREVENT ALLOW / SEEK / SYNC / SET CD SPEED
            0x00 | 0x1B | 0x1E | 0x2B | 0x35 | 0xBB => self.ok(),
            // REQUEST SENSE
            0x03 => {
                let (k, asc, ascq) = self.sense;
                let mut s = vec![0u8; 18];
                s[0] = 0x70; // current error
                s[2] = k;
                s[7] = 10;
                s[12] = asc;
                s[13] = ascq;
                self.sense = (0, 0, 0);
                s.truncate((p[4] as usize).clamp(1, 18));
                self.send_data(s);
            }
            // INQUIRY
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = 0x05; // CD-ROM
                d[1] = 0x80; // removable
                d[2] = 0x00;
                d[3] = 0x21; // response data format 1 (ATAPI 慣例)
                d[4] = 31;
                d[8..16].copy_from_slice(b"RUSTX86 ");
                d[16..32].copy_from_slice(b"CD-ROM          ");
                d[32..36].copy_from_slice(b"1.0 ");
                d.truncate((p[4] as usize).clamp(1, 36));
                self.send_data(d);
            }
            // MODE SENSE(10): 0x2A (CD capabilities) だけ答える
            0x5A => {
                let page = p[2] & 0x3F;
                if page == 0x2A || page == 0x3F {
                    let mut d = vec![0u8; 8 + 20];
                    let len = (d.len() - 2) as u16;
                    d[0..2].copy_from_slice(&len.to_be_bytes());
                    d[8] = 0x2A;
                    d[9] = 18;
                    d[10] = 0x00; // CD-R 読み不可 (素の CD-ROM)
                    d[11] = 0x00;
                    d[12] = 0x01; // audio play
                    d[13] = 0x03; // CD-DA / multisession
                    d[14] = 0x20; // lock
                    d[15] = 0x00;
                    d[16..18].copy_from_slice(&706u16.to_be_bytes()); // max speed (kB/s, 4x)
                    d[18..20].copy_from_slice(&0u16.to_be_bytes());
                    d[20..22].copy_from_slice(&0u16.to_be_bytes());
                    d[22..24].copy_from_slice(&706u16.to_be_bytes()); // current speed
                    d.truncate(alloc16.max(1).min(d.len()));
                    self.send_data(d);
                } else {
                    self.check(0x05, 0x24, 0x00); // invalid field in CDB
                }
            }
            // READ CAPACITY
            0x25 => {
                let mut d = vec![0u8; 8];
                d[0..4].copy_from_slice(&self.sectors.saturating_sub(1).to_be_bytes());
                d[4..8].copy_from_slice(&(SECTOR as u32).to_be_bytes());
                self.send_data(d);
            }
            // READ(10) / READ(12) / READ CD
            0x28 | 0xA8 | 0xBE => {
                let lba = u32::from_be_bytes([p[2], p[3], p[4], p[5]]);
                let count = match p[0] {
                    0x28 => u16::from_be_bytes([p[7], p[8]]) as u32,
                    0xA8 => u32::from_be_bytes([p[6], p[7], p[8], p[9]]),
                    _ => u32::from_be_bytes([0, p[6], p[7], p[8]]),
                };
                if p[0] == 0xBE && p[9] & 0xF8 != 0x10 {
                    // user data (2048B) 以外の形は持っていない
                    self.check(0x05, 0x24, 0x00);
                    return None;
                }
                if count == 0 {
                    self.ok();
                    return None;
                }
                if lba.saturating_add(count) > self.sectors {
                    self.check(0x05, 0x21, 0x00); // LBA out of range
                    return None;
                }
                return Some(Request::Read { lba, count });
            }
            // READ TOC
            0x43 => {
                let msf = p[1] & 0x02 != 0;
                let format = p[2] & 0x0F;
                let addr = |lba: u32| -> [u8; 4] {
                    if msf {
                        let x = lba + 150;
                        [0, (x / 4500) as u8, ((x / 75) % 60) as u8, (x % 75) as u8]
                    } else {
                        lba.to_be_bytes()
                    }
                };
                let d = match format {
                    // TOC: トラック 1 (データ) とリードアウト
                    0 | 2 => {
                        let mut d = vec![0u8; 4];
                        d[2] = 1;
                        d[3] = 1;
                        let start = p[6];
                        if start <= 1 {
                            d.extend_from_slice(&[0, 0x14, 1, 0]);
                            d.extend_from_slice(&addr(0));
                        }
                        d.extend_from_slice(&[0, 0x16, 0xAA, 0]);
                        d.extend_from_slice(&addr(self.sectors));
                        let len = (d.len() - 2) as u16;
                        d[0..2].copy_from_slice(&len.to_be_bytes());
                        d
                    }
                    // session info
                    1 => {
                        let mut d = vec![0u8; 4];
                        d[0..2].copy_from_slice(&10u16.to_be_bytes());
                        d[2] = 1;
                        d[3] = 1;
                        d.extend_from_slice(&[0, 0x14, 1, 0]);
                        d.extend_from_slice(&addr(0));
                        d
                    }
                    _ => {
                        self.check(0x05, 0x24, 0x00);
                        return None;
                    }
                };
                let mut d = d;
                d.truncate(alloc16.max(1).min(d.len()));
                self.send_data(d);
            }
            // GET CONFIGURATION: 今のプロファイルは CD-ROM (0x0008)
            0x46 => {
                let mut d = vec![0u8; 8];
                d[6..8].copy_from_slice(&0x0008u16.to_be_bytes());
                // feature 0000h (profile list) に CD-ROM 1 つ
                d.extend_from_slice(&[0x00, 0x00, 0x03, 4, 0x00, 0x08, 0x01, 0x00]);
                let len = (d.len() - 4) as u32;
                d[0..4].copy_from_slice(&len.to_be_bytes());
                d.truncate(alloc16.max(1).min(d.len()));
                self.send_data(d);
            }
            // GET EVENT STATUS NOTIFICATION: 何も起きていない (NEA)
            0x4A => {
                let mut d = vec![0x00, 0x02, 0x80, 0x00];
                d.truncate(alloc16.max(1).min(d.len()));
                self.send_data(d);
            }
            // READ SUBCHANNEL: 空の頭だけ
            0x42 => {
                let mut d = vec![0u8; 4];
                d.truncate(alloc16.clamp(1, 4));
                self.send_data(d);
            }
            // MECHANISM STATUS
            0xBD => {
                let mut d = vec![0u8; 8];
                d.truncate(alloc16.clamp(1, 8));
                self.send_data(d);
            }
            _ => self.check(0x05, 0x20, 0x00), // invalid command operation code
        }
        None
    }
}

/// ATA の文字列 (2 文字ずつ入れ替わる) を word 列に置く
fn put_str(words: &mut [u16], s: &[u8]) {
    for (i, w) in words.iter_mut().enumerate() {
        let a = s.get(i * 2).copied().unwrap_or(b' ');
        let b = s.get(i * 2 + 1).copied().unwrap_or(b' ');
        *w = (a as u16) << 8 | b as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(d: &mut Ide, p: [u8; 12]) -> Option<Request> {
        d.write8(BASE + 4, 0xFE);
        d.write8(BASE + 5, 0xFF);
        d.write8(BASE + 7, 0xA0);
        assert_eq!(d.read8(BASE + 7) & ST_DRQ, ST_DRQ);
        let mut r = None;
        for i in 0..6 {
            r = d.write_data16(u16::from_le_bytes([p[i * 2], p[i * 2 + 1]]));
        }
        r
    }

    fn drain(d: &mut Ide) -> Vec<u8> {
        let mut out = Vec::new();
        while d.read8(CTRL) & ST_DRQ != 0 {
            out.extend_from_slice(&d.read_data16().to_le_bytes());
        }
        out
    }

    #[test]
    fn signature_and_identify() {
        let mut d = Ide::new(100);
        assert_eq!((d.read8(BASE + 4), d.read8(BASE + 5)), (0x14, 0xEB));
        d.write8(BASE + 7, 0xA1);
        let id = drain(&mut d);
        assert_eq!(id.len(), 512);
        assert_eq!(u16::from_le_bytes([id[0], id[1]]), 0x8580);
        assert_eq!(&id[54..68], b"URTS8X 6DCR-MO"); // 入れ替わった "RUSTX86 CD-ROM"
        assert!(d.irq_pending);
        d.read8(BASE + 7);
        assert!(!d.irq_pending);
    }

    #[test]
    fn inquiry_capacity_and_read() {
        let mut d = Ide::new(100);
        assert!(packet(&mut d, [0x12, 0, 0, 0, 36, 0, 0, 0, 0, 0, 0, 0]).is_none());
        let inq = drain(&mut d);
        assert_eq!(inq[0], 0x05);
        assert_eq!(&inq[8..16], b"RUSTX86 ");
        assert_eq!(d.read8(BASE + 2), IR_COD | IR_IO);

        packet(&mut d, [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let cap = drain(&mut d);
        assert_eq!(u32::from_be_bytes(cap[0..4].try_into().unwrap()), 99);
        assert_eq!(u32::from_be_bytes(cap[4..8].try_into().unwrap()), 2048);

        let r = packet(&mut d, [0x28, 0, 0, 0, 0, 16, 0, 0, 3, 0, 0, 0]);
        assert_eq!(r, Some(Request::Read { lba: 16, count: 3 }));
        d.data_ready((0..3 * 2048).map(|i| i as u8).collect());
        // 上限 0xFFFE なので 1 ブロック
        let data = drain(&mut d);
        assert_eq!(data.len(), 3 * 2048);
        assert_eq!(data[2049], 1);

        // 範囲外は sense 5/21
        let r = packet(&mut d, [0x28, 0, 0, 0, 0, 99, 0, 0, 2, 0, 0, 0]);
        assert!(r.is_none());
        assert_eq!(d.read8(BASE + 7) & ST_ERR, ST_ERR);
        assert_eq!(d.read8(BASE + 1) >> 4, 0x05);
    }

    #[test]
    fn blocks_follow_host_limit() {
        let mut d = Ide::new(100);
        d.write8(BASE + 4, 0x00);
        d.write8(BASE + 5, 0x08); // 2048B ずつ
        d.write8(BASE + 7, 0xA0);
        // READ(10) LBA 0、3 セクタ (count は byte 7-8、BE)
        let p = [0x28, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0];
        let mut r = None;
        for i in 0..6 {
            r = d.write_data16(u16::from_le_bytes([p[i * 2], p[i * 2 + 1]]));
        }
        assert_eq!(r, Some(Request::Read { lba: 0, count: 3 }));
        d.data_ready(vec![7u8; 3 * 2048]);
        let mut blocks = 0;
        while d.read8(CTRL) & ST_DRQ != 0 {
            assert_eq!(
                d.read8(BASE + 4) as usize | (d.read8(BASE + 5) as usize) << 8,
                2048
            );
            for _ in 0..1024 {
                d.read_data16();
            }
            blocks += 1;
        }
        assert_eq!(blocks, 3);
    }
}
